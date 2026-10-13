#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
#[path = "common/fifo.rs"]
mod fifo;
#[path = "common/supervise.rs"]
mod supervise;

use common::fixture_output;
use supervise::supervised;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const CALLER_PANIC: &str = "the caller panics while the blocked test is running";

fn probe_directory() -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixture-panic")
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
    let mut command = Command::new(CARGO_VAL);
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

fn read_pid(ready: &File) -> libc::pid_t {
    let text = fifo::read_line(ready.as_raw_fd(), "the blocked test never reported it was blocked");
    text.trim().parse().unwrap_or_else(|error| panic!("the ready fifo carries one pid, got {text:?}: {error}"))
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
    let warm = fixture_output(&mut blocked_test_command(None));
    assert_eq!(warm.status.code(), Some(0), "the fixture builds and its unblocked test passes");
    let directory = probe_directory();
    let ready = open_reader(&directory.join("ready"));
    let held = open_reader(&directory.join("held"));
    let mut command = blocked_test_command(Some(&directory));
    let (mut blocked, mut leader): (libc::pid_t, libc::pid_t) = (0, 0);

    let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
        supervised(&mut command, |leader_pid, _, _, _| {
            blocked = read_pid(&ready);
            leader = libc::pid_t::try_from(leader_pid).expect("a pid fits pid_t");
            panic!("{CALLER_PANIC}");
        })
    }));

    let payload = unwound.expect_err("the caller's panic must propagate out of supervised");
    let message = payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied());
    assert_eq!(message, Some(CALLER_PANIC), "supervised resumed a different panic than the caller's");
    fifo::await_readable(held.as_raw_fd(), "the held fifo never reached EOF: the blocked test still holds its write end");
    assert!(gone(blocked), "the blocked test (pid {blocked}) survived the unwind");
    assert!(group_gone(blocked), "the blocked test's process group survived the unwind");
    assert!(gone(leader), "the group leader (pid {leader}) survived the unwind");
    assert!(group_gone(leader), "the leader's process group survived the unwind");
}
