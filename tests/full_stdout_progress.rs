#![cfg(unix)]

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use core::future::poll_fn;
use core::num::NonZeroI32;
use core::task::Poll;
use std::io::PipeWriter;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

#[path = "common/binary.rs"]
mod binary;

#[path = "common/group.rs"]
mod group;

#[cfg(target_os = "linux")]
use cargo_val::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
use cargo_val::edge_exit_macos::MacosExit as ExitSource;
use cargo_val::edge_stream_unix::ChildStream;
use futures::FutureExt;
use core::pin::pin;
use futures::future::{Either, join, select};
use prime::os::readiness::{Readiness, ReadyState};
use proxima_primitives::pipe::{Exhausted, Pipe};

#[path = "common/plain.rs"]
mod plain;
#[path = "common/plain_summary.rs"]
mod plain_summary;

use cargo_val::UNPAUSED_QUEUE_BYTES;
use cargo_val::render::Outcome;

const IGNORED_FLOOD_COUNT: usize = 2500;
const PIPE_CEILING_BYTES: usize = 65_536;
const FLOOD_PIPE_BYTES: libc::c_int = 4096;
const RUN_PIPE_BYTES: libc::c_int = 65_536;
// measured from a plain run of the fixture: 408_422 stdout bytes over 3000 tests (one START and one PASS line each) is 136.1
const PLAIN_BYTES_PER_TEST: usize = 137;
const RUN_HOLDS_BYTES: usize = RUN_PIPE_BYTES as usize + UNPAUSED_QUEUE_BYTES;
// the pump pauses once pipe plus queue are full, so the fixture is the most tests whose whole stdout both can hold
const FIXTURE_TEST_COUNT: usize = RUN_HOLDS_BYTES / PLAIN_BYTES_PER_TEST;
const CASE_COUNT_VARIABLE: &str = "FULL_STDOUT_CASE_COUNT";
const _: () = assert!(
    FIXTURE_TEST_COUNT * PLAIN_BYTES_PER_TEST > PIPE_CEILING_BYTES,
    "the fixture must out-write the pipe or the queue is never exercised"
);

// the stdout stream sees EOF as EPOLLHUP alone on Linux, which the default reactor strategy does not wake on; the exit source is the wake that ends the drain
struct Running {
    child: Child,
    exit: ExitSource,
}

impl Running {
    fn start(child: Child) -> Self {
        let pid = NonZeroI32::new(child.id().cast_signed()).expect("a spawned child has a nonzero pid");
        Self { exit: ExitSource::new(pid), child }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if Pipe::call(&self.exit, ()).now_or_never().is_some() {
            return;
        }
        let _ = group::kill_process_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(
        made,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

#[cfg(target_os = "linux")]
fn size_pipe(writer: &PipeWriter, bytes: libc::c_int) {
    let resized = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, bytes) };
    assert!(
        resized >= 0,
        "F_SETPIPE_SZ: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(not(target_os = "linux"))]
fn size_pipe(_writer: &PipeWriter, _bytes: libc::c_int) {}

fn set_nonblocking(descriptor: libc::c_int) {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    let set = unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(set, 0, "set O_NONBLOCK: {}", std::io::Error::last_os_error());
}

// waits for pending bytes without consuming them, so the pipe stays full for the stall assertion
async fn await_pending_bytes(descriptor: libc::c_int, label: &str) {
    let mut gate = Readiness::new(descriptor);
    poll_fn(|context| match gate.poll(context) {
        Ok(ReadyState::Retry) => Poll::Ready(()),
        Ok(ReadyState::Parked) => Poll::Pending,
        Ok(ReadyState::OffWorker) => panic!("{label}: gate polled off a prime worker"),
        Err(error) => panic!("{label}: gate failed: {error}"),
    })
    .await;
}

fn unread_bytes(descriptor: libc::c_int) -> libc::c_int {
    let mut count: libc::c_int = 0;
    let status = unsafe { libc::ioctl(descriptor, libc::FIONREAD, &raw mut count) };
    assert_eq!(status, 0, "FIONREAD: {}", std::io::Error::last_os_error());
    count
}

fn spawn_cargo_val(directory: &Path, fixture: &str, stdout: PipeWriter) -> Child {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture)
        .join("Cargo.toml");
    let stderr = fs::File::create(directory.join("stderr.log")).expect("stderr log is creatable");
    let mut command = binary::command();
    group::lead_group(&mut command);
    command
        .env("CARGO_VAL_OUTPUT", "plain")
        .arg("--manifest-path")
        .arg(manifest)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env("NO_COLOR", "1")
        .env(CASE_COUNT_VARIABLE, FIXTURE_TEST_COUNT.to_string())
        .env("CARGO_VAL_DONE_LOG", directory.join("done"))
        .env(
            "CARGO_TARGET_DIR",
            target_directory().join(format!("{fixture}-fixture")),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("cargo-val binary is spawnable")
}

fn expected_names() -> BTreeSet<String> {
    (0..FIXTURE_TEST_COUNT)
        .map(|index| format!("stdout_pressure_case_{index:04}"))
        .collect()
}

async fn collect_done_names(done: &ChildStream) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut pending = Vec::new();
    let mut reported = 0;
    while reported < FIXTURE_TEST_COUNT {
        let range = match done.call(()).await {
            Ok(read) => read.expect("done log reads"),
            Err(Exhausted) => panic!("done log closed before every fixture test reported"),
        };
        pending.extend_from_slice(&done.window(range).expect("the range was just read"));
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=newline).collect();
            let name = String::from_utf8(line[..newline].to_vec()).expect("done line is text");
            names.insert(name);
            reported += 1;
        }
    }
    names
}

async fn drain_stdout(stdout: &ChildStream) -> Vec<u8> {
    let mut output = Vec::new();
    while let Ok(read) = stdout.call(()).await {
        let range = read.expect("stdout reads");
        output.extend_from_slice(&stdout.window(range).expect("the range was just read"));
    }
    output
}

#[proxima::test(timeout = "310s")]
async fn c75_full_stdout_never_stalls_the_run_and_delivers_every_line_after_drain() {
    let directory = target_directory()
        .join("full-stdout")
        .join(format!("run-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("run directory is creatable");
    make_fifo(&directory.join("done"));
    let done = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(directory.join("done"))
        .expect("done fifo opens read-write so the reader never sees EOF");
    let done = ChildStream::new(OwnedFd::from(done));

    let (stdout, stdout_writer) = std::io::pipe().expect("stdout pipe is creatable");
    set_nonblocking(stdout.as_raw_fd());
    let stdout_descriptor = stdout.as_raw_fd();
    let stdout = ChildStream::new(OwnedFd::from(stdout));
    // the pump pauses once its status queue is full, so pipe plus queue must hold the fixture's whole stdout for every test to run unread
    size_pipe(&stdout_writer, RUN_PIPE_BYTES);
    let mut running = Running::start(spawn_cargo_val(&directory, "full_stdout", stdout_writer));

    let names = match select(pin!(collect_done_names(&done)), pin!(Pipe::call(&running.exit, ()))).await {
        Either::Left((names, _)) => names,
        Either::Right((exited, _)) => {
            let stderr = fs::read_to_string(directory.join("stderr.log")).unwrap_or_default();
            panic!("cargo-val ended before every fixture test reported: {exited:?}; stderr: {stderr}");
        }
    };
    assert_eq!(names, expected_names(), "every fixture test ran exactly once");
    let alive = running.child.try_wait().expect("cargo-val status reads");
    assert!(alive.is_none(), "cargo-val waits on a full stdout instead of exiting: {alive:?}");
    let unread = unread_bytes(stdout_descriptor);
    assert!(unread > 0, "no stdout was pending while the stdout pipe went unread");

    let (output, status) = drain_and_exit(&stdout, &running).await;
    let output_len = output.len();
    assert!(
        output_len > PIPE_CEILING_BYTES,
        "stdout ({output_len} bytes) must exceed any pipe buffer to prove the stall path"
    );

    assert_eq!(status.code(), Some(0), "cargo-val exit status");
    assert!(
        output_len <= RUN_HOLDS_BYTES,
        "stdout ({output_len} bytes) outgrew pipe plus queue ({RUN_HOLDS_BYTES}); the run pauses past that, so the per-test bytes grew"
    );

    let text = String::from_utf8(output).expect("stdout is text");
    let passed = plain::names(&text, Outcome::Passed);
    let printed: BTreeSet<String> = passed.iter().cloned().collect();
    assert_eq!(passed.len(), FIXTURE_TEST_COUNT, "one PASS line per fixture test");
    assert_eq!(printed, expected_names(), "PASS lines name every fixture test");
    let counts = plain_summary::summary(&text).expect("stdout carries a SUMMARY line");
    assert_eq!((counts["passed"], counts["failed"]), (FIXTURE_TEST_COUNT, 0), "summary counts: {counts:?}");
}

#[proxima::test(timeout = "310s")]
async fn ignored_listing_flood_pauses_on_a_full_queue_and_reports_every_skip_in_order_after_drain() {
    let directory = target_directory()
        .join("full-stdout")
        .join(format!("flood-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("run directory is creatable");
    let (stdout, stdout_writer) = std::io::pipe().expect("stdout pipe is creatable");
    set_nonblocking(stdout.as_raw_fd());
    let stdout_descriptor = stdout.as_raw_fd();
    let stdout = ChildStream::new(OwnedFd::from(stdout));
    size_pipe(&stdout_writer, FLOOD_PIPE_BYTES);
    let mut running = Running::start(spawn_cargo_val(&directory, "ignored_flood", stdout_writer));

    await_pending_bytes(stdout_descriptor, "stdout").await;
    let alive = running.child.try_wait().expect("cargo-val status reads");
    assert!(alive.is_none(), "cargo-val waits on a full stdout instead of exiting: {alive:?}");

    let (output, status) = drain_and_exit(&stdout, &running).await;

    let stderr = fs::read_to_string(directory.join("stderr.log")).expect("stderr log reads");
    assert_eq!(status.code(), Some(0), "cargo-val exit status; stderr: {stderr}");

    let text = String::from_utf8(output).expect("stdout is text");
    let skipped = plain::names(&text, Outcome::Ignored);
    let expected: Vec<String> = (0..IGNORED_FLOOD_COUNT)
        .map(|index| format!("ignored_flood_case_with_a_deliberately_long_descriptive_name_for_real_world_reports_{index:04}"))
        .collect();
    assert_eq!(skipped.len(), IGNORED_FLOOD_COUNT, "one skip line per ignored test");
    assert!(skipped.iter().zip(&expected).all(|(printed, name)| printed == name), "skip lines arrive in listing order");
}
// the test-level timeout only fails the test; the watchdog kills cargo-val's whole group at the bound, which ends the drain and the exit wait
async fn drain_and_exit(stdout: &ChildStream, running: &Running) -> (Vec<u8>, std::process::ExitStatus) {
    let (cancel, fired) = group::watchdog(running.child.id(), group::sync_deadline());
    let (output, exited) = join(drain_stdout(stdout), Pipe::call(&running.exit, ())).await;
    drop(cancel);
    assert!(!fired.join().unwrap_or(false), "cargo-val (pid {}) outlived {:?} while its stdout drained and its group was killed", running.child.id(), group::sync_deadline());
    (output, exited.expect("the exit source is unspent").expect("cargo-val exit status reads"))
}
