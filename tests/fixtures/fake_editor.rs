use std::io::{Read, Write};

unsafe extern "C" {
    fn tcgetattr(descriptor: i32, termios: *mut u8) -> i32;
    fn tcgetpgrp(descriptor: i32) -> i32;
    fn getpgrp() -> i32;
    fn getpid() -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(target_os = "macos")]
const LFLAG_OFFSET: usize = 24;
#[cfg(target_os = "macos")]
const LFLAG_BYTES: usize = 8;
#[cfg(target_os = "macos")]
const ICANON: u64 = 0x100;

#[cfg(target_os = "linux")]
const LFLAG_OFFSET: usize = 12;
#[cfg(target_os = "linux")]
const LFLAG_BYTES: usize = 4;
#[cfg(target_os = "linux")]
const ICANON: u64 = 0x2;

const ECHO: u64 = 0x8;

fn local_flags() -> Option<u64> {
    let mut termios = [0_u8; 256];
    if unsafe { tcgetattr(0, termios.as_mut_ptr()) } != 0 {
        return None;
    }
    let mut flags = [0_u8; 8];
    flags[..LFLAG_BYTES].copy_from_slice(&termios[LFLAG_OFFSET..LFLAG_OFFSET + LFLAG_BYTES]);
    Some(u64::from_le_bytes(flags))
}

fn record(log: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    for argument in std::env::args().skip(1) {
        writeln!(file, "arg={argument}")?;
    }
    let flags = local_flags();
    writeln!(file, "canonical={}", flags.is_some_and(|flags| flags & ICANON != 0))?;
    writeln!(file, "echo={}", flags.is_some_and(|flags| flags & ECHO != 0))?;
    writeln!(file, "pgid={}", unsafe { getpgrp() })?;
    let (terminal, group) = unsafe { (tcgetpgrp(0), getpgrp()) };
    writeln!(file, "terminal={terminal}")?;
    writeln!(file, "foreground={}", terminal == group)
}

#[cfg(target_os = "macos")]
const SIGSTOP: i32 = 17;
#[cfg(target_os = "linux")]
const SIGSTOP: i32 = 19;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--serverlist") {
        while std::env::var_os("FAKE_EDITOR_SERVERLIST_HANG").is_some() {
            std::thread::yield_now();
        }
        print!("{}", std::env::var("FAKE_EDITOR_SERVERLIST").unwrap_or_default());
        return;
    }
    let log = std::env::var("FAKE_EDITOR_LOG").expect("the test names a log file");
    record(&log).expect("the editor records what it saw");
    println!("fake-editor-ready");
    std::io::stdout().flush().expect("the ready marker reaches the terminal");
    if std::env::var_os("FAKE_EDITOR_STOP_ONCE").is_some() {
        unsafe { kill(getpid(), SIGSTOP) };
    }
    if std::env::var_os("FAKE_EDITOR_WAIT").is_some() {
        let mut byte = [0_u8; 1];
        let _ = std::io::stdin().read(&mut byte);
    }
    let released = std::env::var_os("CARGO_VAL_FIXTURE_RELEASE_FILE").map(|release| std::path::PathBuf::from(release).with_extension("done"));
    if let Some(hold) = std::env::var_os("FAKE_EDITOR_HOLD_FILE") {
        while !std::path::Path::new(&hold).exists() {
            std::thread::yield_now();
        }
    }
    if let Some(until) = released.filter(|_| std::env::var_os("FAKE_EDITOR_AWAIT_RUN").is_some()) {
        while !until.exists() {
            std::thread::yield_now();
        }
    }
    println!("fake-editor-done");
    std::io::stdout().flush().expect("the done marker reaches the terminal");
    let code = std::env::var("FAKE_EDITOR_EXIT").ok().and_then(|text| text.parse().ok()).unwrap_or(0);
    std::process::exit(code);
}
