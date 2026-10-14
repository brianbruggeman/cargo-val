#![cfg(unix)]

use core::cell::Cell;
use core::future::Future;
use core::time::Duration;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cargo_val::edge_stream_unix::ChildStream;
use futures::FutureExt;
use proxima_core::time::timeout;

mod common;
#[path = "common/exit_wait.rs"]
mod exit_wait;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fifo.rs"]
mod fifo;
#[path = "common/supervise.rs"]
mod supervise;

use common::{FIXTURE_DEADLINE, drain, drain_until, fixture_output};
use exit_wait::{ExitSource, until_exit};
use binary::EXIT_WITH_PARENT;
use supervise::supervised;

const KIND: &str = "CARGO_VAL_PARENT_DEATH_KIND";
const ARGUMENTS: &str = "CARGO_VAL_PARENT_DEATH_ARGUMENTS";
const STDOUT_FILE: &str = "CARGO_VAL_PARENT_DEATH_STDOUT";
const PTY: &str = "pty";
const DETACHED: &str = "detached";
const BLOCKED: &str = "blocked";
const ARMED: &str = "armed";
const GONE_DEADLINE: Duration = Duration::from_secs(60);

fn fixture_manifest(fixture: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{fixture}/Cargo.toml"))
}

fn probe_directory(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("parent-death").join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("probe directory is creatable under the target tmpdir");
    for name in ["ready", "held", "block"] {
        let fifo = CString::new(directory.join(name).as_os_str().as_bytes()).expect("path has no nul byte");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo {name}");
    }
    directory
}

fn open_reader(path: &Path) -> File {
    OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).expect("a fifo opens for nonblocking read without a writer")
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
fn open_ready(path: &Path) -> File {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path).expect("a fifo opens for nonblocking read-write")
}

fn open_pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (-1, -1);
    let status = unsafe {
        libc::openpty(&raw mut master, &raw mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    assert_eq!(status, 0, "openpty allocates a pair");
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn wait_readable(descriptor: libc::c_int) {
    let mut watched = libc::pollfd { fd: descriptor, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&raw mut watched, 1, 300_000) };
    assert_eq!(ready, 1, "cargo-val never wrote its first bytes to the pty");
}

// the killable parent, re-executed from the test binary; the pty arm leaves the master and the original slave inherited by cargo-val, as a harness that never sets close-on-exec does
#[test]
#[ignore = "re-executed as the killable parent of cargo-val by the parent-death tests"]
fn intermediary() {
    let kind = std::env::var(KIND).expect("the parent-death tests name the arm");
    let arguments = std::env::var(ARGUMENTS).expect("the parent-death tests pass the cargo-val arguments");
    let mut command = binary::command();
    command
        .args(arguments.split('\n'))
        .args(["--output", "plain"])
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env_remove("CI")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .process_group(0);
    if kind == DETACHED {
        command.env_remove(EXIT_WITH_PARENT);
    } else {
        command.env(EXIT_WITH_PARENT, "1");
    }
    let (master, slave) = (kind == PTY).then(open_pty).unzip();
    match (slave, kind == DETACHED) {
        (Some(slave), _) => command.stdout(Stdio::from(slave)),
        (None, true) => command.stdout(File::create(std::env::var_os(STDOUT_FILE).expect("the detached arm names its stdout file")).expect("the stdout file is creatable")),
        (None, false) => command.stdout(Stdio::null()),
    };
    let mut cargo_val = command.spawn().expect("cargo-val is spawnable");
    drop(command);
    if let Some(master) = master {
        wait_readable(master.as_raw_fd());
        std::mem::forget(master);
    }
    println!("{ARMED} {}", cargo_val.id());
    cargo_val.wait().expect("the intermediary outlives cargo-val until it is killed");
}

fn intermediary_command(kind: &str, arguments: &str, label: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("the test binary path"));
    command
        .args(["--ignored", "--exact", "--nocapture", "intermediary"])
        .env(KIND, kind)
        .env(ARGUMENTS, arguments)
        .env("CARGO_VAL_C52D_FAIL", "1")
        .env("CARGO_TARGET_DIR", Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("parent-death-fixture-{label}")))
        .stdin(Stdio::null());
    command
}

// builds the fixture once before the run under test, so the rendezvous waits on a test and not on a compile
async fn warm_fixture(arguments: &str, label: &str) {
    let mut command = binary::command();
    command
        .args(arguments.split('\n'))
        .env("NO_COLOR", "1")
        .env_remove("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR")
        .env("CARGO_TARGET_DIR", Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("parent-death-fixture-{label}")));
    let warm = fixture_output(&mut command).await;
    assert_eq!(warm.status.code(), Some(0), "the fixture builds and its unblocked test passes");
}

async fn armed_pid(announcements: &ChildStream, cargo_val: &Cell<libc::pid_t>, exit: &ExitSource, lifeline: &ChildStream) {
    let settled = drain_until(announcements, |bytes| bytes.windows(ARMED.len()).any(|window| window == ARMED.as_bytes()) && bytes.ends_with(b"\n"));
    let (text, fault) = until_exit(settled, "the intermediary never armed", exit, &[("stdout", announcements), ("stderr", lifeline)]).await;
    assert!(fault.is_none(), "the intermediary's stdout failed: {fault:?}");
    let text = String::from_utf8_lossy(&text);
    let line = text.lines().find_map(|line| line.strip_prefix(ARMED)).unwrap_or_else(|| panic!("the intermediary ended before it armed:\n{text}"));
    cargo_val.set(line.trim().parse().expect("the intermediary announces cargo-val's pid"));
}

// cargo-val leads its own process group, which the supervising test's group kill does not reach, so a failed assertion kills it here
async fn reaping_on_panic(cargo_val: &Cell<libc::pid_t>, body: impl Future<Output = ()>) {
    if let Err(payload) = AssertUnwindSafe(body).catch_unwind().await {
        if cargo_val.get() > 0 {
            unsafe { libc::killpg(cargo_val.get(), libc::SIGKILL) };
        }
        panic::resume_unwind(payload);
    }
}

// the lifeline is the intermediary's stderr, which cargo-val inherits: once the intermediary is dead only cargo-val holds its write end, so EOF means cargo-val is gone
async fn await_cargo_val_gone(lifeline: &ChildStream, what: &str) {
    let (_, fault) = timeout(GONE_DEADLINE, drain(lifeline)).await.unwrap_or_else(|elapsed| panic!("{what}: still running after {GONE_DEADLINE:?} ({elapsed:?})"));
    assert!(fault.is_none(), "{what}: the lifeline failed to read: {fault:?}");
}

// select(2), not the reactor: a kqueue stream on a fifo is not woken when its last writer leaves, and a poll(2) never reports it on macOS
fn await_fifo_eof(held: &File, what: &str) {
    let descriptor = held.as_raw_fd();
    let mut readable: libc::fd_set = unsafe { core::mem::zeroed() };
    unsafe {
        libc::FD_ZERO(&raw mut readable);
        libc::FD_SET(descriptor, &raw mut readable);
    }
    let mut limit = libc::timeval { tv_sec: libc::time_t::try_from(GONE_DEADLINE.as_secs()).expect("the deadline fits time_t"), tv_usec: 0 };
    let ready = unsafe { libc::select(descriptor + 1, &raw mut readable, std::ptr::null_mut(), std::ptr::null_mut(), &raw mut limit) };
    assert_eq!(ready, 1, "{what}: the held fifo never reached EOF");
    let mut byte = [0_u8; 1];
    let count = unsafe { libc::read(descriptor, byte.as_mut_ptr().cast(), 1) };
    assert_eq!(count, 0, "{what}: the held fifo carried data instead of EOF");
}

const PRIME_ROOT_THREADS: usize = 1;

#[cfg(target_os = "macos")]
fn thread_count(pid: libc::pid_t) -> usize {
    let mut info: libc::proc_taskinfo = unsafe { core::mem::zeroed() };
    let size = i32::try_from(size_of::<libc::proc_taskinfo>()).expect("taskinfo size fits");
    let written = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
    assert_eq!(written, size, "proc_pidinfo returns a full proc_taskinfo for pid {pid}");
    usize::try_from(info.pti_threadnum).expect("thread count is non-negative")
}

#[cfg(target_os = "linux")]
fn thread_count(pid: libc::pid_t) -> usize {
    fs::read_dir(format!("/proc/{pid}/task")).expect("the task directory of a live pid lists its threads").count()
}

fn kill_intermediary(leader: u32) {
    let leader = libc::pid_t::try_from(leader).expect("a pid fits pid_t");
    assert_eq!(unsafe { libc::kill(leader, libc::SIGKILL) }, 0, "the intermediary is killable");
}

#[proxima::test(timeout = "310s")]
async fn a_run_waiting_on_a_blocked_test_exits_and_kills_it_when_its_parent_is_sigkilled() {
    let directory = probe_directory(BLOCKED);
    let ready = Box::new(ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready")))));
    let held = open_reader(&directory.join("held"));
    let arguments = format!("--manifest-path\n{}\n--lib\nunit_blocks_when_requested", fixture_manifest("blocked_test").display());
    warm_fixture(&arguments, BLOCKED).await;
    let mut command = intermediary_command(BLOCKED, &arguments, BLOCKED);
    command.env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", &directory);

    let cargo_val = Cell::new(0);
    supervised(&mut command, FIXTURE_DEADLINE, async |leader, announcements, lifeline, exit| {
        reaping_on_panic(&cargo_val, async {
            armed_pid(announcements, &cargo_val, exit, lifeline).await;
            let what = "the blocked test never reported it was blocked";
            let line = until_exit(fifo::first_line(&ready, what), what, exit, &[("stdout", announcements), ("stderr", lifeline)]).await;
            let blocked: libc::pid_t = line.trim().parse().expect("the ready fifo carries one pid");
            assert_eq!(unsafe { libc::kill(blocked, 0) }, 0, "the blocked test is alive while its parent is");

            kill_intermediary(leader);

            await_fifo_eof(&held, "the blocked test still holds the held fifo after its run's parent died");
            await_cargo_val_gone(lifeline, "cargo-val after its parent died").await;
        })
        .await;
    })
    .await;
}

#[proxima::test(timeout = "310s")]
async fn a_run_blocked_writing_to_an_unread_terminal_exits_when_its_parent_is_sigkilled() {
    let arguments = format!("--manifest-path\n{}\n--tests\n--no-fail-fast", fixture_manifest("cli_run").display());
    let mut command = intermediary_command(PTY, &arguments, PTY);

    let cargo_val = Cell::new(0);
    supervised(&mut command, FIXTURE_DEADLINE, async |leader, announcements, lifeline, exit| {
        reaping_on_panic(&cargo_val, async {
            armed_pid(announcements, &cargo_val, exit, lifeline).await;
            assert_eq!(thread_count(cargo_val.get()), PRIME_ROOT_THREADS, "cargo-val runs only on the prime root thread: no watcher thread exists");

            kill_intermediary(leader);

            await_cargo_val_gone(lifeline, "cargo-val after its parent died").await;
        })
        .await;
    })
    .await;
}

#[proxima::test(timeout = "310s")]
async fn a_run_without_the_setting_survives_its_parent_being_sigkilled_and_finishes() {
    let directory = probe_directory(DETACHED);
    let ready = Box::new(ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready")))));
    let held = Box::new(ChildStream::new(OwnedFd::from(open_reader(&directory.join("held")))));
    let stdout_file = directory.join("stdout.txt");
    let arguments = format!("--manifest-path\n{}\n--lib\nunit_blocks_when_requested", fixture_manifest("blocked_test").display());
    warm_fixture(&arguments, DETACHED).await;
    let mut command = intermediary_command(DETACHED, &arguments, DETACHED);
    command.env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", &directory).env(STDOUT_FILE, &stdout_file);

    let cargo_val = Cell::new(0);
    supervised(&mut command, FIXTURE_DEADLINE, async |leader, announcements, lifeline, exit| {
        reaping_on_panic(&cargo_val, async {
            armed_pid(announcements, &cargo_val, exit, lifeline).await;
            let what = "the blocked test never reported it was blocked";
            let line = until_exit(fifo::first_line(&ready, what), what, exit, &[("stdout", announcements), ("stderr", lifeline)]).await;
            let blocked: libc::pid_t = line.trim().parse().expect("the ready fifo carries one pid");

            kill_intermediary(leader);

            // the blocked test opens this fifo read-write before announcing itself, so a nonblocking write open fails with ENXIO only if that process is gone, which is how a run that died with its parent fails here
            let mut release = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(directory.join("block"))
                .unwrap_or_else(|error| {
                    let path = directory.join("block");
                    panic!("open(O_WRONLY|O_NONBLOCK) of {} failed with {error} (errno {:?}): no process holds the block fifo open, and the blocked test (pid {blocked}, alive={}) opens it read-write before announcing itself", path.display(), error.raw_os_error(), unsafe { libc::kill(blocked, 0) } == 0)
                });
            release.write_all(b"1").expect("the release byte fits the fifo the blocked test reads");
            drop(release);
            await_cargo_val_gone(lifeline, "the released run").await;
            timeout(GONE_DEADLINE, fifo::until_eof(&held, "the released test"))
                .await
                .unwrap_or_else(|elapsed| panic!("the released test never closed the held fifo ({elapsed:?})"));
        })
        .await;
    })
    .await;

    let report = fs::read_to_string(&stdout_file).expect("the run wrote its report to the stdout file");
    assert!(report.lines().any(|line| line.starts_with("SUMMARY  passed=1 failed=0")), "the run did not finish normally:\n{report}");
}

#[test]
fn a_nonblocking_write_open_of_a_fifo_needs_a_holder_and_fails_enxio_without_one() {
    let directory = probe_directory("enxio-control");
    let block = directory.join("block");
    let write_open = || OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&block);

    let unheld = write_open().expect_err("no process holds the fifo, as when the blocked test has not yet reached its open");
    assert_eq!(unheld.raw_os_error(), Some(libc::ENXIO), "{unheld} opening {}", block.display());

    let holder = OpenOptions::new().read(true).write(true).open(&block).expect("a read-write open of a fifo never waits for a peer");
    write_open().expect("a held fifo accepts the harness's nonblocking write open");

    drop(holder);
    let released = write_open().expect_err("the holder died, which is what a run killed with its parent looks like");
    assert_eq!(released.raw_os_error(), Some(libc::ENXIO), "{released} opening {}", block.display());
}

#[test]
fn signalling_a_group_whose_only_member_is_an_unreaped_zombie_is_not_a_failure() {
    let mut leader = Command::new("/bin/sleep").arg("600").process_group(0).spawn().expect("sleep spawns as its own group leader");
    let pid = leader.id();
    let group = libc::pid_t::try_from(pid).expect("a pid fits pid_t");
    assert_eq!(unsafe { libc::kill(group, libc::SIGKILL) }, 0, "the leader is killable");
    let mut exited: libc::siginfo_t = unsafe { core::mem::zeroed() };
    let waited = unsafe { libc::waitid(libc::P_PID, libc::id_t::from(pid), &raw mut exited, libc::WEXITED | libc::WNOWAIT) };
    assert_eq!(waited, 0, "waitid reports the leader dead without reaping it");

    let interrupted = common::interrupt(pid);
    let killed = common::kill_group(pid);
    leader.wait().expect("the zombie leader is reaped");

    assert_eq!(interrupted, Ok(()), "SIGINT to a zombie-only group");
    assert_eq!(killed, Ok(()), "SIGKILL to a zombie-only group");
}

#[test]
fn the_thread_count_check_sees_a_thread_the_process_spawned() {
    let own_pid = libc::pid_t::try_from(std::process::id()).expect("a pid fits pid_t");
    let baseline = thread_count(own_pid);
    let (started, started_receiver) = std::sync::mpsc::channel();
    let (release, release_receiver) = std::sync::mpsc::channel::<()>();
    let extra = std::thread::spawn(move || {
        started.send(()).expect("the test is waiting for the start signal");
        let _ = release_receiver.recv();
    });
    started_receiver.recv().expect("the extra thread reports it is running");

    let with_extra = thread_count(own_pid);
    release.send(()).expect("the extra thread is still waiting");
    extra.join().expect("the extra thread exits cleanly");

    assert_eq!(with_extra, baseline + 1, "the count the watcher-thread assertion relies on rises by one for one extra thread");
    assert_ne!(with_extra, PRIME_ROOT_THREADS, "a process with an extra thread fails the single-root-thread assertion");
}
