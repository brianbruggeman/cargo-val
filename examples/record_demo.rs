use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const WINDOW_SIZE: &str = "80x34";
const COLUMNS: &str = "80";
const ROWS: &str = "34";
const DRIVE_FLAG: &str = "--drive";
const THEME: &str = "dracula";
const FONT_SIZE: &str = "16";
const MAX_GIF_BYTES: u64 = 2 * 1024 * 1024;

// recorder dwell times: a viewer needs to read each screen before the next key changes it
#[cfg(unix)]
const HOLD_LIVE_RUN_MS: u64 = 0;
#[cfg(unix)]
const HOLD_REVIEW_LIST_MS: u64 = 2000;
#[cfg(unix)]
const HOLD_AFTER_J_MS: u64 = 1200;
#[cfg(unix)]
const HOLD_AFTER_K_MS: u64 = 1200;
#[cfg(unix)]
const HOLD_DETAIL_MS: u64 = 3000;
#[cfg(unix)]
const HOLD_BACK_TO_LIST_MS: u64 = 0;
#[cfg(unix)]
const HOLD_RERUN_RUNNING_MS: u64 = 0;
#[cfg(unix)]
const HOLD_RERUN_RESULT_MS: u64 = 2000;
#[cfg(unix)]
const HOLD_CONFIRM_QUIT_MS: u64 = 1000;
#[cfg(unix)]
const HOLD_RECEIPT_MS: u64 = 3000;

fn main() -> ExitCode {
    let driving = env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == DRIVE_FLAG);
    match if driving { terminal::drive() } else { record() } {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("record_demo: {message}");
            ExitCode::FAILURE
        }
    }
}

fn record() -> Result<(), String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cast = root.join("assets/demo.cast");
    let gif = root.join("assets/demo.gif");
    fs::create_dir_all(root.join("assets"))
        .map_err(|error| format!("assets directory: {error}"))?;
    let target_directory = demo_target_directory(&root);
    prebuild_demo(&root, &target_directory)?;
    clean_demo_crate(&root, &target_directory)?;

    record_session(&root, &target_directory)?;
    drop_leading_delay(&cast)?;
    run(Command::new("agg").current_dir(&root).args([
        "--theme",
        THEME,
        "--font-size",
        FONT_SIZE,
        "--cols",
        COLUMNS,
        "--rows",
        ROWS,
        "assets/demo.cast",
        "assets/demo.gif",
    ]))?;
    report(&cast, &gif)
}

fn record_session(root: &Path, target_directory: &Path) -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    run(Command::new("asciinema")
        .current_dir(root)
        .env("PATH", path_with_built_binary()?)
        .envs(isolated_cargo(target_directory))
        .args([
            "rec",
            "--overwrite",
            "--window-size",
            WINDOW_SIZE,
            "-c",
            &format!("{} {DRIVE_FLAG}", executable.display()),
            "assets/demo.cast",
        ]))
}

// a v3 cast stamps each event with the delay since the previous one, so zeroing the first delay starts the recording at the first output byte instead of on a blank screen
fn drop_leading_delay(cast: &Path) -> Result<(), String> {
    let text = fs::read_to_string(cast).map_err(|error| format!("{}: {error}", cast.display()))?;
    let (header, events) = text.split_once('\n').ok_or("cast has no events")?;
    let (_, rest) = events.split_once(',').ok_or("first event has no delay")?;
    fs::write(cast, format!("{header}\n[0.0,{rest}"))
        .map_err(|error| format!("{}: {error}", cast.display()))
}

fn demo_target_directory(root: &Path) -> PathBuf {
    env::args_os()
        .nth(1)
        .map_or_else(|| root.join("examples/demo/target"), PathBuf::from)
}

fn isolated_cargo(target_directory: &Path) -> [(&'static str, PathBuf); 2] {
    // cargo's package cache lock is global to CARGO_HOME, so a private home keeps other cargo processes out of the cast
    [
        ("CARGO_TARGET_DIR", target_directory.to_path_buf()),
        ("CARGO_HOME", target_directory.join("cargo-home")),
    ]
}

fn prebuild_demo(root: &Path, target_directory: &Path) -> Result<(), String> {
    // a red demo run exits nonzero by design, so only a launch failure is fatal; the run warms the isolated build dir, and --tests skips the concurrent doc child whose cargo would contend for the package cache
    run(Command::new("cargo")
        .current_dir(root)
        .env("PATH", path_with_built_binary()?)
        .envs(isolated_cargo(target_directory))
        .args([
            "val",
            "--manifest-path",
            "examples/demo/Cargo.toml",
            "--target-dir",
            "isolated",
            "--tests",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null()))
}

// the prebuild leaves every unit cached, so the recording would open on a build bar that jumps to done; cleaning the demo crate alone keeps its dependencies warm and gives the cast a visible build phase
fn clean_demo_crate(root: &Path, target_directory: &Path) -> Result<(), String> {
    let workspaces = target_directory.join("cargo-val");
    let entries = fs::read_dir(&workspaces)
        .map_err(|error| format!("{}: {error}", workspaces.display()))?;
    for entry in entries.flatten() {
        let test_directory = entry.path().join("test");
        if test_directory.is_dir() {
            run(Command::new("cargo")
                .current_dir(root)
                .envs(isolated_cargo(target_directory))
                .args(["clean", "--package", "tinyserve", "--manifest-path", "examples/demo/Cargo.toml", "--target-dir"])
                .arg(&test_directory))?;
        }
    }
    Ok(())
}

fn path_with_built_binary() -> Result<OsString, String> {
    let executable = env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    let profile_directory = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("example executable has no profile directory")?;
    if !profile_directory
        .join(format!("cargo-val{}", env::consts::EXE_SUFFIX))
        .is_file()
    {
        return Err(format!(
            "build the cargo-val binary first: {} has none",
            profile_directory.display()
        ));
    }
    let mut entries = vec![profile_directory.to_path_buf()];
    entries.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(entries).map_err(|error| format!("PATH: {error}"))
}

fn run(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().display().to_string();
    let status = command
        .status()
        .map_err(|error| format!("{program} did not launch: {error}"))?;
    // a red demo run exits nonzero by design, so only the recorder's own failure to launch is fatal
    if status.code().is_none() {
        return Err(format!("{program} was killed by a signal"));
    }
    Ok(())
}

fn report(cast: &Path, gif: &Path) -> Result<(), String> {
    let cast_bytes = fs::metadata(cast)
        .map_err(|error| format!("{}: {error}", cast.display()))?
        .len();
    let gif_bytes = fs::metadata(gif)
        .map_err(|error| format!("{}: {error}", gif.display()))?
        .len();
    let frames =
        gif_frames(&fs::read(gif).map_err(|error| format!("{}: {error}", gif.display()))?)?;
    println!("{}: {cast_bytes} bytes", cast.display());
    println!("{}: {gif_bytes} bytes, {frames} frames", gif.display());
    if gif_bytes > MAX_GIF_BYTES {
        return Err(format!(
            "{} is {gif_bytes} bytes, over the {MAX_GIF_BYTES} byte limit",
            gif.display()
        ));
    }
    Ok(())
}

fn gif_frames(bytes: &[u8]) -> Result<usize, String> {
    let malformed = || "malformed gif".to_string();
    let flags = *bytes.get(10).ok_or_else(malformed)?;
    let mut cursor = 13 + color_table_len(flags);
    let mut frames = 0;
    loop {
        match *bytes.get(cursor).ok_or_else(malformed)? {
            0x3B => return Ok(frames),
            0x21 => cursor = skip_sub_blocks(bytes, cursor + 2)?,
            0x2C => {
                frames += 1;
                let local_flags = *bytes.get(cursor + 9).ok_or_else(malformed)?;
                cursor = skip_sub_blocks(bytes, cursor + 10 + color_table_len(local_flags) + 1)?;
            }
            _ => return Err(malformed()),
        }
    }
}

fn color_table_len(flags: u8) -> usize {
    if flags & 0x80 == 0 {
        0
    } else {
        3 << ((flags & 0x07) + 1)
    }
}

fn skip_sub_blocks(bytes: &[u8], mut cursor: usize) -> Result<usize, String> {
    loop {
        let length = usize::from(*bytes.get(cursor).ok_or("malformed gif")?);
        cursor += 1 + length;
        if length == 0 {
            return Ok(cursor);
        }
    }
}
#[cfg(unix)]
mod terminal {
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        HOLD_AFTER_J_MS, HOLD_AFTER_K_MS, HOLD_BACK_TO_LIST_MS, HOLD_CONFIRM_QUIT_MS,
        HOLD_DETAIL_MS, HOLD_LIVE_RUN_MS, HOLD_RECEIPT_MS, HOLD_RERUN_RESULT_MS,
        HOLD_RERUN_RUNNING_MS, HOLD_REVIEW_LIST_MS,
    };

    // each step names the screen text that must be drawn, how long it stays on screen, then the keys typed; an empty key list only waits for the screen
    const KEYS: [(&str, u64, &[u8]); 10] = [
        ("searching", HOLD_LIVE_RUN_MS, b""),
        ("\u{203a}ASSERT", HOLD_REVIEW_LIST_MS, b"j"),
        ("\u{203a}PANIC", HOLD_AFTER_J_MS, b"k"),
        ("\u{203a}ASSERT", HOLD_AFTER_K_MS, b"\r"),
        ("enter list", HOLD_DETAIL_MS, b"\r"),
        ("j/k move", HOLD_BACK_TO_LIST_MS, b"r"),
        ("RERUN", HOLD_RERUN_RUNNING_MS, b""),
        ("rerun 1", HOLD_RERUN_RESULT_MS, b"q"),
        ("again to confirm", HOLD_CONFIRM_QUIT_MS, b"q"),
        ("SUMMARY", HOLD_RECEIPT_MS, b""),
    ];

    // the screen is matched as the text a reader sees, so colour and hyperlink escapes between words do not hide a marker
    fn visible_text(raw: &str) -> String {
        let mut text = String::with_capacity(raw.len());
        let mut characters = raw.chars().peekable();
        while let Some(character) = characters.next() {
            if character != '\x1b' {
                text.push(character);
                continue;
            }
            match characters.next() {
                Some('[') => {
                    characters.by_ref().find(|byte| ('\x40'..='\x7e').contains(byte));
                }
                Some(']') => {
                    while let Some(byte) = characters.next() {
                        if byte == '\x07' || (byte == '\x1b' && characters.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        text
    }

    // asciinema records output only, so the recorded command is this example again: it owns the pty cargo val runs on and types the keys when the screen each one answers has been drawn
    pub fn drive() -> Result<(), String> {
        let (mut master, slave) = open_terminal()?;
        let mut child = spawn_on_terminal(&slave)?;
        let mut shown = String::new();
        let mut keys = KEYS.iter();
        let mut next = keys.next();
        let mut buffer = [0_u8; 4096];
        let mut stdout = io::stdout();
        let mut open = true;
        while open {
            open = forward(&mut master, &mut buffer, &mut stdout, &mut shown, None)?;
            while let Some((screen, hold_ms, bytes)) = next.filter(|(screen, _, _)| visible_text(&shown).contains(screen)) {
                let deadline = Instant::now() + Duration::from_millis(*hold_ms);
                while open && Instant::now() < deadline {
                    open = forward(&mut master, &mut buffer, &mut stdout, &mut shown, Some(deadline))?;
                }
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                master
                    .write_all(bytes)
                    .map_err(|error| format!("keys after {screen:?}: {error}"))?;
                shown.clear();
                next = keys.next();
            }
        }
        child
            .wait()
            .map_err(|error| format!("cargo val wait: {error}"))?;
        next.map_or(Ok(()), |(screen, _, _)| {
            Err(format!("the run never drew {screen:?}"))
        })
    }

    // output keeps flowing to the recording while a screen is held, so a redraw during a dwell is stamped when it happens; returns false once the child closes the terminal
    fn forward(
        master: &mut File,
        buffer: &mut [u8],
        stdout: &mut io::Stdout,
        shown: &mut String,
        deadline: Option<Instant>,
    ) -> Result<bool, String> {
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let timeout = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            if unsafe { libc::poll(&raw mut poll, 1, timeout) } <= 0 {
                return Ok(true);
            }
        }
        let read = match master.read(buffer) {
            Ok(0) | Err(_) => return Ok(false),
            Ok(read) => read,
        };
        stdout
            .write_all(&buffer[..read])
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("stdout: {error}"))?;
        shown.push_str(&String::from_utf8_lossy(&buffer[..read]));
        Ok(true)
    }

    fn spawn_on_terminal(slave: &OwnedFd) -> Result<Child, String> {
        let stream = || {
            slave
                .try_clone()
                .map(Stdio::from)
                .map_err(|error| format!("slave: {error}"))
        };
        let mut command = Command::new("cargo");
        // the slave becomes the controlling terminal, as it is in a shell
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0);
                Ok(())
            });
        }
        command
            .args([
                "val",
                "--manifest-path",
                "examples/demo/Cargo.toml",
                "--target-dir",
                "isolated",
                "--tests",
                "--no-fail-fast",
                "--output",
                "rich",
            ])
            .env_remove("NO_COLOR")
            .env_remove("CI")
            .stdin(stream()?)
            .stdout(stream()?)
            .stderr(stream()?)
            .spawn()
            .map_err(|error| format!("cargo val did not launch: {error}"))
    }

    // asciinema only forwards keys from a terminal, so the recorder holds the master end of a pty and types into it
    fn open_terminal() -> Result<(File, OwnedFd), String> {
        let mut size = libc::winsize {
            ws_row: 34,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (mut master, mut slave) = (0, 0);
        let opened = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                &raw mut size,
            )
        };
        if opened != 0 {
            return Err(format!("openpty: {}", std::io::Error::last_os_error()));
        }
        Ok(unsafe {
            (
                File::from(OwnedFd::from_raw_fd(master)),
                OwnedFd::from_raw_fd(slave),
            )
        })
    }
}

#[cfg(not(unix))]
mod terminal {
    pub fn drive() -> Result<(), String> {
        Err("the recorder drives a pty, which needs unix".to_string())
    }
}
