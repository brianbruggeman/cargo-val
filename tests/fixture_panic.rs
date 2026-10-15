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

#[proxima::test(timeout = "60s")]
async fn a_missed_deadline_names_who_holds_the_build_directory_lock_and_who_does_not() {
    let target = probe_directory("deadline-lock-holders");
    let build_directory = target.join("debug");
    fs::create_dir_all(&build_directory).expect("the profile directory is creatable");
    let held_path = build_directory.join(".cargo-lock");
    let free_path = build_directory.join(".cargo-build-lock");
    let held = File::create(&held_path).expect("the held lock file is creatable");
    File::create(&free_path).expect("the free lock file is creatable");
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exec sleep 600"])
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_HOME", target.join("cargo-home"));

    let unwound = AssertUnwindSafe(supervised(&mut command, Duration::from_millis(50), async |_, _, _, _| std::future::pending::<()>().await))
        .catch_unwind()
        .await;

    let message = panic_text(&*unwound.expect_err("a child that never exits must miss the deadline"));
    let cwd = std::env::current_dir().expect("the test has a working directory");
    let holder = format!("  pid {} cwd {} argv ", std::process::id(), cwd.display());
    assert!(message.contains("lock holders at the deadline:"), "{message}");
    assert!(message.contains(&format!("{}:\n{holder}", held_path.display())), "{message}");
    assert!(message.contains(&format!("{}:\n  no holder", free_path.display())), "{message}");
    assert!(message.contains("sccache servers:"), "{message}");
    drop(held);
}

#[proxima::test(timeout = "60s")]
async fn a_missed_deadline_persists_its_report_and_names_the_path_last() {
    let directory = probe_directory("deadline-persisted-report");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read line < \"$BLOCK\""]).env("BLOCK", directory.join("block"));

    let unwound = AssertUnwindSafe(supervised(&mut command, Duration::from_millis(50), async |_, _, _, _| std::future::pending::<()>().await))
        .catch_unwind()
        .await;

    let message = panic_text(&*unwound.expect_err("a child blocked on a fifo must miss the deadline"));
    let last = message.lines().last().expect("the failure message has lines");
    let path = PathBuf::from(last.strip_prefix("deadline report: ").unwrap_or_else(|| panic!("last line must name the log: {message}")));
    let report = fs::read_to_string(&path).expect("the named log file exists");
    fs::remove_file(&path).expect("the test removes its own log");
    assert!(report.contains("a_missed_deadline_persists_its_report_and_names_the_path_last"), "{report}");
    assert!(report.contains("lock holders at the deadline:"), "{report}");
    assert!(report.contains("uptime: "), "{report}");
    assert!(report.contains("deadline: 50ms"), "{report}");
}

fn take_persisted_report(message: &str, test: &str, deadline: &str) -> String {
    let path = message
        .lines()
        .find_map(|line| line.strip_prefix("deadline report: "))
        .map_or_else(|| panic!("the failure must name its report: {message}"), PathBuf::from);
    let report = fs::read_to_string(&path).expect("the named log file exists");
    fs::remove_file(&path).expect("the test removes its own log");
    assert!(report.contains(&format!("test: {test}\n")), "{report}");
    assert!(report.contains(&format!("deadline: {deadline}\n")), "{report}");
    assert!(report.contains("lock holders at the deadline:"), "{report}");
    assert!(report.contains("uptime: "), "{report}");
    report
}

#[proxima::test(timeout = "60s")]
async fn a_bounded_fixture_run_that_misses_its_deadline_persists_its_report() {
    let directory = probe_directory("bounded-persisted-report");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read line < \"$BLOCK\""]).env("BLOCK", directory.join("block"));

    let failure = common::output_within(&mut command, Duration::from_millis(50), "FIXTURE_DEADLINE")
        .await
        .expect_err("a child blocked on a fifo must miss the deadline");

    let report = take_persisted_report(&failure, "a_bounded_fixture_run_that_misses_its_deadline_persists_its_report", "50ms");
    assert!(report.contains("/bin/sh") || report.contains("sh -c"), "the group listing names the blocked shell: {report}");
}

// settings::bound counts whole seconds, so 1 s is the shortest fifo deadline; nextest gives each test its own process, which is what makes the variable safe to set
fn with_one_second_fifo_deadline() {
    // safety: set once before any other thread of this test process reads it
    unsafe { std::env::set_var("CARGO_VAL_TEST_FIFO_DEADLINE", "1") };
}

fn hold_fifo_open_without_writing(directory: &Path) -> (ChildStream, File) {
    let reader = ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready"))));
    let writer = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(directory.join("ready")).expect("the fifo has a reader, so a nonblocking write open succeeds");
    (reader, writer)
}

#[proxima::test(timeout = "60s")]
async fn a_fifo_that_never_delivers_a_line_persists_its_report() {
    with_one_second_fifo_deadline();
    let directory = probe_directory("fifo-line-persisted-report");
    let (ready, _writer) = hold_fifo_open_without_writing(&directory);

    let unwound = AssertUnwindSafe(fifo::first_line(&ready, "the child never wrote its pid")).catch_unwind().await;

    let message = panic_text(&*unwound.expect_err("a fifo nobody writes to must miss the deadline"));
    take_persisted_report(&message, "a_fifo_that_never_delivers_a_line_persists_its_report", "1s");
}

#[proxima::test(timeout = "60s")]
async fn a_fifo_that_never_reaches_eof_persists_its_report() {
    with_one_second_fifo_deadline();
    let directory = probe_directory("fifo-eof-persisted-report");
    let (held, _writer) = hold_fifo_open_without_writing(&directory);

    let unwound = AssertUnwindSafe(fifo::until_eof(&held, "the held fifo")).catch_unwind().await;

    let message = panic_text(&*unwound.expect_err("a fifo whose writer stays open must miss the deadline"));
    take_persisted_report(&message, "a_fifo_that_never_reaches_eof_persists_its_report", "1s");
}
