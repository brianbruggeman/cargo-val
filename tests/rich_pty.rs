#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;

#[path = "common/binary.rs"]
mod binary;
#[path = "common/group.rs"]
mod group;

const FRAME_START: &str = "\x1b[?25l";
const COLUMNS: u16 = 80;
const ROWS: u16 = 40;

struct Pty {
    master: File,
    slave: OwnedFd,
}

fn open_pty() -> Pty {
    let mut size = libc::winsize { ws_row: ROWS, ws_col: COLUMNS, ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master, mut slave) = (0, 0);
    let opened = unsafe { libc::openpty(&raw mut master, &raw mut slave, core::ptr::null_mut(), core::ptr::null_mut(), &raw mut size) };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { Pty { master: File::from(OwnedFd::from_raw_fd(master)), slave: OwnedFd::from_raw_fd(slave) } }
}

fn fixture_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rich_pty/Cargo.toml")
}

fn spawn_on(slave: &OwnedFd, release: &std::path::Path) -> std::process::Child {
    let stream = || Stdio::from(slave.try_clone().expect("the slave descriptor duplicates"));
    let mut command = binary::command();
    group::lead_group(&mut command);
    command
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .args(["--tests", "--no-fail-fast", "--output", "rich"])
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_FIXTURE_RELEASE_FILE", release)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-target-{}", std::process::id())))
        .stdin(stream())
        .stdout(stream())
        .stderr(stream())
        .spawn()
        .expect("cargo-val is spawnable on a pty")
}

fn count_before_slash(frame: &str, is_row: impl Fn(&str) -> bool) -> Option<usize> {
    let row = frame.lines().find(|line| is_row(line))?;
    let before = &row[..row.find('/')?];
    let digits: String = before.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse().ok()
}

fn built_count(frame: &str) -> Option<usize> {
    count_before_slash(frame, |line| line.contains("compile units"))
}

fn settled_count(frame: &str) -> Option<usize> {
    count_before_slash(frame, |line| line.starts_with(" test  "))
}

fn live_frames(output: &str) -> Vec<&str> {
    output.split(FRAME_START).skip(1).filter(|frame| frame.contains("compile units")).collect()
}

fn shown(frame: &str) -> String {
    frame.replace('\x1b', "\\e").replace('\r', "")
}

fn read_until_exit(mut master: File, release: &std::path::Path) -> String {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut released = false;
    while let Ok(length) = master.read(&mut chunk) {
        if length == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..length]);
        let text = String::from_utf8_lossy(&output);
        if !released && live_frames(&text).iter().any(|frame| settled_count(frame).is_some_and(|settled| settled > 0)) {
            File::create(release).expect("the release file is creatable");
            released = true;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[test]
fn a_later_live_frame_shows_nonzero_built_and_settled_counts() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-release-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("the release directory is creatable");
    let release = directory.join("release");
    let pty = open_pty();
    let Pty { master, slave } = pty;
    let mut child = spawn_on(&slave, &release);
    drop(slave);

    let (cancel, fired) = group::watchdog(child.id(), group::SYNC_DEADLINE);
    let output = read_until_exit(master, &release);
    let status = child.wait().expect("cargo-val is reaped");
    drop(cancel);
    assert!(!fired.join().unwrap_or(false), "cargo-val outlived {:?} and its group was killed", group::SYNC_DEADLINE);
    let frames = live_frames(&output);
    println!("first frame:\n{}", shown(frames.first().expect("a live frame was drawn")));
    println!("middle frame:\n{}", shown(frames[frames.len() / 2]));
    println!("last live frame:\n{}", shown(frames.last().expect("a live frame was drawn")));
    assert!(status.success(), "cargo-val exited {status}");
    let later = frames.iter().find(|frame| built_count(frame).is_some_and(|built| built > 0) && settled_count(frame).is_some_and(|settled| settled > 0));
    assert!(later.is_some(), "no live frame showed a nonzero built count with a nonzero settled count among {} frames", frames.len());
}
