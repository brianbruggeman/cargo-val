#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cargo_val::edge_stream_unix::ChildStream;
use futures::FutureExt;

mod common;
#[path = "common/exit_wait.rs"]
mod exit_wait;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fifo.rs"]
mod fifo;
#[path = "common/supervise.rs"]
mod supervise;

use fixture::{fixture_deadline, fixture_output};
use exit_wait::until_exit;
use supervise::supervised;

const CALLER_PANIC: &str = "the caller panics while the blocked test is running";

fn probe_directory(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixture-panic")
        .join(label)
        .join(std::process::id().to_string());
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("probe directory is creatable under the target tmpdir");
    for name in ["ready", "held", "block"] {
        let fifo = CString::new(directory.join(name).as_os_str().as_bytes()).expect("path has no nul byte");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo {name}");
    }
    directory
}

fn blocked_test_command(directory: Option<&Path>) -> Command {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocked_test/Cargo.toml");
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--lib", "unit_blocks_when_requested"])
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-panic-target"))
        .env_remove("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR");
    if let Some(directory) = directory {
        command.env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", directory);
    }
    command
}

fn open_reader(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read without a writer")
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
fn open_ready(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read-write")
}

fn gone(pid: libc::pid_t) -> bool {
    let signalled = unsafe { libc::kill(pid, 0) };
    signalled == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn group_gone(group: libc::pid_t) -> bool {
    let signalled = unsafe { libc::killpg(group, 0) };
    signalled == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[proxima::test(timeout = "310s")]
async fn a_panicking_supervised_test_still_kills_its_group() {
    let warm = fixture_output(&mut blocked_test_command(None)).await;
    assert_eq!(warm.status.code(), Some(0), "the fixture builds and its unblocked test passes");
    let directory = probe_directory("supervised-panic");
    let ready = ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready"))));
    let held = ChildStream::new(OwnedFd::from(open_reader(&directory.join("held"))));
    let mut command = blocked_test_command(Some(&directory));
    let mut leader: libc::pid_t = 0;
    let mut blocked: libc::pid_t = 0;
    let unwound = AssertUnwindSafe(supervised(&mut command, fixture_deadline(), async |leader_pid, stdout, stderr, exit| {
        leader = libc::pid_t::try_from(leader_pid).expect("a pid fits pid_t");
        let what = "the blocked test never reported it was blocked";
        let text = until_exit(fifo::first_line(&ready, what), what, exit, &[("stdout", stdout), ("stderr", stderr)]).await;
        blocked = text.trim().parse().unwrap_or_else(|error| panic!("the ready fifo carries one pid, got {text:?}: {error}"));
        panic!("{CALLER_PANIC}");
    }))
    .catch_unwind()
    .await;

    let payload = unwound.expect_err("the caller's panic must propagate out of supervised");
    let message = payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied());
    assert_eq!(message, Some(CALLER_PANIC), "supervised resumed a different panic than the caller's");
    fifo::until_eof(&held, "the held fifo never reached EOF: the blocked test still holds its write end").await;
    assert!(gone(blocked), "the blocked test (pid {blocked}) survived the unwind");
    assert!(group_gone(blocked), "the blocked test's process group survived the unwind");
    assert!(gone(leader), "the group leader (pid {leader}) survived the unwind");
    assert!(group_gone(leader), "the leader's process group survived the unwind");
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default()
}

#[proxima::test(timeout = "60s")]
async fn a_child_that_exits_before_writing_ready_fails_promptly_naming_its_status_and_output() {
    let directory = probe_directory("ready-wait-child-exits");
    let ready = ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready"))));
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "echo said-this-first; echo and-this-to-stderr >&2; exit 7"]);
    let started = Instant::now();

    let unwound = AssertUnwindSafe(supervised(&mut command, fixture_deadline(), async |_, stdout, stderr, exit| {
        let what = "the child never wrote its pid";
        until_exit(fifo::first_line(&ready, what), what, exit, &[("stdout", stdout), ("stderr", stderr)]).await
    }))
    .catch_unwind()
    .await;

    let message = panic_text(&*unwound.expect_err("a child that never writes ready must fail the wait"));
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}: {message}", started.elapsed());
    assert!(message.contains("exit status: 7"), "{message}");
    assert!(message.contains("said-this-first"), "{message}");
    assert!(message.contains("and-this-to-stderr"), "{message}");
}

#[proxima::test(timeout = "60s")]
async fn a_line_the_child_writes_is_returned_while_it_keeps_running() {
    let directory = probe_directory("ready-wait-line-written");
    let ready = ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready"))));
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "echo 4242 > \"$READY\"; exec sleep 600"]).env("READY", directory.join("ready"));

    let line = supervised(&mut command, fixture_deadline(), async |_, stdout, stderr, exit| {
        let what = "the child never wrote its pid";
        until_exit(fifo::first_line(&ready, what), what, exit, &[("stdout", stdout), ("stderr", stderr)]).await
    })
    .await;

    assert_eq!(line.trim(), "4242");
}
