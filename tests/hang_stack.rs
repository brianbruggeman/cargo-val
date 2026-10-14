#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::time::Instant;

use cargo_val::edge_stack::{FORMAT, capture};
use cargo_val::edge_stream_unix::ChildStream;
use cargo_val::stack_frames::{Class, Frame, frames};

mod common;
#[path = "common/fifo.rs"]
mod fifo;
#[path = "common/supervise.rs"]
mod supervise;
#[path = "../src/test_alloc.rs"]
mod test_alloc;

use common::{fixture_deadline, fixture_output};
use supervise::supervised;

const WORKSPACE: [&str; 1] = ["cargo_val_blocked_fixture"];

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from)
}

fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
fn open_ready(path: &Path) -> ChildStream {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read-write");
    ChildStream::new(OwnedFd::from(file))
}

fn open_held(path: &Path) -> ChildStream {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read without a writer");
    ChildStream::new(OwnedFd::from(file))
}

// the sampled frame must point at the statement that blocks, found in the fixture source so editing the fixture cannot silently stale a hardcoded line
#[cfg(target_os = "macos")]
fn blocking_line_of_fixture() -> u32 {
    let source = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocked_test/src/lib.rs")).expect("the fixture source is readable");
    let index = source.lines().position(|line| line.contains("block.read(")).expect("the fixture blocks on a read of the block fifo");
    u32::try_from(index + 1).expect("the fixture is shorter than u32 lines")
}

async fn build_fixture_test_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocked_test/Cargo.toml");
    let mut command = Command::new(env!("CARGO"));
    command
        .args(["test", "--lib", "--no-run", "--message-format=json", "--manifest-path"])
        .arg(manifest)
        .env("CARGO_TARGET_DIR", target_directory().join("hang-stack-fixture"));
    let output = fixture_output(&mut command).await;
    assert!(output.status.success(), "fixture build failed: {}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).expect("cargo json is text");
    let marker = "\"executable\":\"";
    let start = text.find(marker).expect("cargo reported the fixture test executable") + marker.len();
    let length = text[start..].find('"').expect("the executable path is quoted");
    PathBuf::from(&text[start..start + length])
}

async fn blocked_command() -> (Command, ChildStream, ChildStream) {
    let executable = build_fixture_test_binary().await;
    let directory = target_directory().join("hang-stack-rendezvous").join(std::process::id().to_string());
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    let ready = open_ready(&directory.join("ready"));
    let held = open_held(&directory.join("held"));
    let mut command = Command::new(executable);
    command
        .args(["--exact", "tests::unit_blocks_when_requested"])
        .env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", &directory);
    (command, ready, held)
}

async fn blocked_pid(ready: &ChildStream) -> u32 {
    let line = fifo::first_line(ready, "the fixture never reached the rendezvous").await;
    line.trim().parse().expect("the fixture wrote its pid")
}

// a task's state reads S while it is still on a run queue and preempted just before it sleeps, so only a symbol in wchan (or a readable stack) proves it is off the CPU; a refused read cannot be waited out and capture names it as denied
#[cfg(target_os = "linux")]
fn until_every_thread_is_named_asleep(pid: u32) {
    let started = Instant::now();
    loop {
        let tasks: Vec<PathBuf> = fs::read_dir(format!("/proc/{pid}/task"))
            .expect("the blocked child's task directory is readable")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        if tasks.len() >= 2 && tasks.iter().all(|task| sleeps_by_the_kernels_account(task)) {
            return;
        }
        assert!(started.elapsed() < fixture_deadline(), "threads of {pid} never all named a sleep: {tasks:?}");
        std::thread::yield_now();
    }
}

#[cfg(target_os = "linux")]
fn sleeps_by_the_kernels_account(task: &Path) -> bool {
    if fs::read(task.join("stack")).is_ok_and(|stack| !stack.trim_ascii().is_empty()) {
        return true;
    }
    fs::read(task.join("wchan")).map_or(true, |symbol| !matches!(symbol.as_slice(), b"" | b"0"))
}

#[cfg(target_os = "macos")]
#[proxima::test(timeout = "500s")]
async fn macos_sample_of_a_real_blocked_child_finds_the_user_frame_at_its_file_and_line() {
    let (mut command, ready, held) = blocked_command().await;

    supervised(&mut command, fixture_deadline(), async |_, _, _, _| {
        let pid = blocked_pid(&ready).await;
        let mut buffer = vec![0_u8; 32 * 1024];

        let (outcome, allocations) = test_alloc::measure_allocations(|| capture(pid, &mut buffer));

        let (written, dropped) = outcome.expect("sample of a live child must capture");
        let all = frames(&buffer[..written], FORMAT, &WORKSPACE).expect("sample output is text");
        let users: Vec<Frame<'_>> = all.filter(|frame| frame.class == Class::User).collect();
        println!(
            "{{\"event\":\"hang_stack_capture\",\"pid\":{pid},\"written\":{written},\"dropped\":{dropped},\"user_frames\":{},\"allocations\":{allocations}}}",
            users.len()
        );
        let blocked = users
            .iter()
            .rfind(|frame| frame.thread.contains("unit_blocks_when_requested"))
            .expect("the blocked test thread has a user frame");
        assert!(blocked.symbol.contains("unit_blocks_when_requested"), "{}", blocked.symbol);
        assert_eq!((blocked.file, blocked.line), (Some("lib.rs"), Some(blocking_line_of_fixture())));
        assert_eq!(dropped, 0);
    })
    .await;
    fifo::until_eof(&held, "the held fifo never reached EOF: the blocked fixture still holds its write end").await;
}

#[cfg(target_os = "linux")]
#[proxima::test(timeout = "500s")]
async fn linux_proc_of_a_real_blocked_child_yields_a_kernel_frame_per_thread_or_names_why_not() {
    use cargo_val::stack_capture::Unavailable;

    let (mut command, ready, held) = blocked_command().await;

    supervised(&mut command, fixture_deadline(), async |_, _, _, _| {
        let pid = blocked_pid(&ready).await;
        until_every_thread_is_named_asleep(pid);
        let mut buffer = vec![0_u8; 32 * 1024];

        let (outcome, allocations) = test_alloc::measure_allocations(|| capture(pid, &mut buffer));

        match outcome {
            Ok((written, dropped)) => {
                let all: Vec<Frame<'_>> = frames(&buffer[..written], FORMAT, &WORKSPACE).expect("proc stacks are text").collect();
                let main_thread = format!("Thread_{pid}");
                let symbols_of = |is_main: bool| -> Vec<&str> {
                    all.iter().filter(|frame| (frame.thread == main_thread) == is_main).map(|frame| frame.symbol).collect()
                };
                let (main_symbols, test_symbols) = (symbols_of(true), symbols_of(false));
                println!("{{\"event\":\"hang_stack_capture\",\"pid\":{pid},\"written\":{written},\"dropped\":{dropped},\"allocations\":{allocations},\"main_symbols\":{main_symbols:?},\"test_symbols\":{test_symbols:?}}}");
                assert!(main_symbols.iter().any(|symbol| symbol.contains("futex")), "the main thread sleeps in libtest's channel wait: {all:?}");
                assert!(test_symbols.iter().any(|symbol| symbol.contains("pipe_read")), "the test thread sleeps in its read of the block fifo: {all:?}");
                assert!(all.iter().all(|frame| frame.image == "kernel" && frame.class == Class::Runtime), "{all:?}");
            }
            Err(reason) => {
                let Unavailable::Denied { os_code } = reason else { panic!("unexpected reason: {reason}") };
                assert!(os_code == libc::EACCES || os_code == libc::EPERM, "os error {os_code}");
            }
        }
    })
    .await;
    fifo::until_eof(&held, "the held fifo never reached EOF: the blocked fixture still holds its write end").await;
}
