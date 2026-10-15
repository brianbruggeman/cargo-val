// RUSTC_WRAPPER for fixture builds: cargo creates each child pipe and sets CLOEXEC in two steps, so a rustc forked in between inherits a sibling's pipe and holds its EOF open; this wrapper closes every inherited fd above stderr before the real rustc runs
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::{Command, ExitCode};

#[cfg(unix)]
const FIRST_INHERITED_FD: i32 = 3;
#[cfg(unix)]
const F_GETFD: i32 = 1;
#[cfg(unix)]
const LIST_FDS: &str = "--list-fds";
#[cfg(unix)]
const JOBSERVER_FLAGS: [&str; 2] = ["--jobserver-auth=", "--jobserver-fds="];

#[cfg(unix)]
unsafe extern "C" {
    fn close(descriptor: i32) -> i32;
    fn fcntl(descriptor: i32, command: i32, ...) -> i32;
}

#[cfg(unix)]
fn jobserver_fds(makeflags: &str) -> Vec<i32> {
    makeflags
        .split_whitespace()
        .filter_map(|word| JOBSERVER_FLAGS.iter().find_map(|flag| word.strip_prefix(flag)))
        .filter_map(|value| value.split_once(','))
        .filter_map(|(read, write)| Some([read.parse().ok()?, write.parse().ok()?]))
        .flatten()
        .collect()
}

#[cfg(unix)]
fn kept_fds() -> Vec<i32> {
    ["CARGO_MAKEFLAGS", "MAKEFLAGS"].iter().filter_map(|name| std::env::var(name).ok()).flat_map(|flags| jobserver_fds(&flags)).collect()
}

// the listing includes the read_dir handle's own number; it is closed by the time the snapshot is filtered, so fcntl rejects it
#[cfg(unix)]
fn open_fds() -> Vec<i32> {
    let mut listed: Vec<i32> = std::fs::read_dir("/dev/fd")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    listed.retain(|descriptor| unsafe { fcntl(*descriptor, F_GETFD) } != -1);
    listed.sort_unstable();
    listed
}

#[cfg(unix)]
fn close_inherited(keep: &[i32]) {
    for descriptor in open_fds().into_iter().filter(|descriptor| *descriptor >= FIRST_INHERITED_FD && !keep.contains(descriptor)) {
        unsafe { close(descriptor) };
    }
}

#[cfg(unix)]
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.split_first() {
        Some((mode, _)) if mode == LIST_FDS => {
            for descriptor in open_fds() {
                println!("{descriptor}");
            }
            ExitCode::SUCCESS
        }
        Some((rustc, rest)) => {
            close_inherited(&kept_fds());
            let failure = Command::new(rustc).args(rest).exec();
            eprintln!("fd_scrub_rustc: exec {rustc} failed: {failure}");
            ExitCode::from(127)
        }
        None => {
            eprintln!("fd_scrub_rustc: no rustc path given");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("fd_scrub_rustc: unix only");
    std::process::ExitCode::from(2)
}
