#![cfg(unix)]

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use cargo_val::edge_stack::capture;
use cargo_val::stack_capture::Capture;
use cargo_val::stack_frames::{Class, Frame, Rows};

#[path = "../src/test_alloc.rs"]
mod test_alloc;

const READY_TIMEOUT_MILLISECONDS: i32 = 300_000;
const WORKSPACE: [&str; 1] = ["cargo_val_blocked_fixture"];

struct Blocked {
    child: Child,
}

impl Drop for Blocked {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from)
}

fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

fn open_nonblocking_reader(path: &Path) -> libc::c_int {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let descriptor = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    assert!(descriptor >= 0, "open {}: {}", path.display(), std::io::Error::last_os_error());
    descriptor
}

fn read_pid(descriptor: libc::c_int) -> u32 {
    let mut collected = Vec::new();
    while !collected.contains(&b'\n') {
        let mut watched = libc::pollfd { fd: descriptor, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&raw mut watched, 1, READY_TIMEOUT_MILLISECONDS) };
        assert_eq!(ready, 1, "the fixture never reached the rendezvous");
        let mut bytes = [0_u8; 64];
        let count = unsafe { libc::read(descriptor, bytes.as_mut_ptr().cast(), bytes.len()) };
        let count = usize::try_from(count).expect("read from the ready fifo succeeds");
        assert!(count > 0, "the ready fifo closed before a full line arrived");
        collected.extend_from_slice(&bytes[..count]);
    }
    String::from_utf8(collected).expect("the pid line is text").trim().parse().expect("the fixture wrote its pid")
}

fn build_fixture_test_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocked_test/Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args(["test", "--lib", "--no-run", "--message-format=json", "--manifest-path"])
        .arg(manifest)
        .env("CARGO_TARGET_DIR", target_directory().join("hang-stack-fixture"))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo builds the blocked fixture");
    assert!(output.status.success(), "fixture build failed");
    let text = String::from_utf8(output.stdout).expect("cargo json is text");
    let marker = "\"executable\":\"";
    let start = text.find(marker).expect("cargo reported the fixture test executable") + marker.len();
    let length = text[start..].find('"').expect("the executable path is quoted");
    PathBuf::from(&text[start..start + length])
}

fn block_fixture() -> (Blocked, u32) {
    let executable = build_fixture_test_binary();
    let directory = target_directory().join("hang-stack-rendezvous").join(std::process::id().to_string());
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    let ready = open_nonblocking_reader(&directory.join("ready"));
    let _held = open_nonblocking_reader(&directory.join("held"));
    let child = Command::new(executable)
        .args(["--exact", "tests::unit_blocks_when_requested"])
        .env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", &directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("fixture test binary is spawnable");
    let blocked = Blocked { child };
    let pid = read_pid(ready);
    unsafe { libc::close(ready) };
    (blocked, pid)
}

fn user_frames(rows: Rows<'_>) -> Vec<Frame<'_>> {
    rows.filter_map(|row| if let cargo_val::stack_frames::Row::Frame(frame) = row { Some(frame) } else { None })
        .filter(|frame| frame.class == Class::User)
        .collect()
}

#[cfg(target_os = "macos")]
#[test]
fn macos_sample_of_a_real_blocked_child_finds_the_user_frame_at_its_file_and_line() {
    let (_blocked, pid) = block_fixture();
    let mut buffer = vec![0_u8; 32 * 1024];

    let (outcome, allocations) = test_alloc::measure_allocations(|| capture(pid, &mut buffer));

    let Capture::Captured { format, written, dropped, source } = outcome else {
        panic!("sample of a live child must capture: {outcome:?}");
    };
    let rows = Rows::folded(&buffer[..written], format, &WORKSPACE).expect("sample output is text");
    let users = user_frames(rows.clone());
    let useful: usize = rows
        .filter_map(|row| if let cargo_val::stack_frames::Row::Frame(frame) = row { Some(frame) } else { None })
        .map(|frame| frame.symbol.len() + frame.file.map_or(0, str::len) + 8)
        .sum();
    println!(
        "{{\"event\":\"hang_stack_capture\",\"pid\":{pid},\"source_bytes\":{source},\"written\":{written},\"dropped\":{dropped},\"useful_bytes\":{useful},\"user_frames\":{},\"allocations\":{allocations}}}",
        users.len()
    );
    let blocked = users
        .iter()
        .rfind(|frame| frame.thread.contains("unit_blocks_when_requested"))
        .expect("the blocked test thread has a user frame");
    assert!(blocked.symbol.contains("unit_blocks_when_requested"), "{}", blocked.symbol);
    assert_eq!((blocked.file, blocked.line), (Some("lib.rs"), Some(26)));
    assert_eq!(dropped, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_stack_of_a_real_blocked_child_parses_or_names_why_it_cannot() {
    use cargo_val::stack_capture::Unavailable;

    let (_blocked, pid) = block_fixture();
    let mut buffer = vec![0_u8; 32 * 1024];

    match capture(pid, &mut buffer) {
        Capture::Captured { format, written, .. } => {
            let rows = Rows::raw(&buffer[..written], format, &WORKSPACE).expect("proc stacks are text");
            assert!(rows.count() > 0, "captured bytes must hold rows");
        }
        Capture::Unavailable(reason) => {
            let Unavailable::Denied { os_code } = reason else { panic!("unexpected reason: {reason}") };
            assert!(os_code == libc::EACCES || os_code == libc::EPERM, "os error {os_code}");
            assert_eq!(
                reason.to_string(),
                format!("unavailable: kernel stack read denied (os error {os_code}); ptrace fallback needs an unwinder dependency and Cargo.lock has none")
            );
        }
    }
}
