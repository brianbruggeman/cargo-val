#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;

mod common;

use common::{fixture_output, output_until};

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const SAFETY_BOUND_SECONDS: libc::time_t = 240;

fn probe_directory() -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixture-deadline")
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
        .env("CARGO_TARGET_DIR", Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixture-deadline-target"));
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

// select(2) reports a fifo whose last writer left on macOS where poll(2) does not; the bound only fails the test, it never decides it
fn await_readable(fifo: &File, what: &str) {
    let descriptor = fifo.as_raw_fd();
    let mut readable: libc::fd_set = unsafe { core::mem::zeroed() };
    unsafe {
        libc::FD_ZERO(&raw mut readable);
        libc::FD_SET(descriptor, &raw mut readable);
    }
    let mut timeout = libc::timeval { tv_sec: SAFETY_BOUND_SECONDS, tv_usec: 0 };
    let ready = unsafe {
        libc::select(descriptor + 1, &raw mut readable, core::ptr::null_mut(), core::ptr::null_mut(), &raw mut timeout)
    };
    assert_eq!(ready, 1, "{what}");
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
async fn a_fixture_run_that_misses_its_deadline_fails_and_leaves_no_process_behind() {
    let warm = fixture_output(&mut blocked_test_command(None));
    assert_eq!(warm.status.code(), Some(0), "the fixture builds and its unblocked test passes");
    let directory = probe_directory();
    let mut ready = open_reader(&directory.join("ready"));
    let held = open_reader(&directory.join("held"));

    let mut line = String::new();
    let failure = output_until(&mut blocked_test_command(Some(&directory)), |_| {
        await_readable(&ready, "the blocked test never reported it was blocked");
        ready
            .read_to_string(&mut line)
            .expect("the blocked test wrote its pid before the kill");
        Err(RecvTimeoutError::Timeout)
    })
    .expect_err("the blocked test never returns, so the driven expiry fires");

    let blocked: libc::pid_t = line.trim().parse().expect("the ready fifo carries one pid");
    assert!(failure.contains("outlived"), "{failure}");
    assert!(failure.contains("unit_blocks_when_requested"), "{failure}");
    await_readable(&held, "the held fifo never reached EOF: the blocked test still holds its write end");
    assert!(gone(blocked), "the blocked test (pid {blocked}) survived the deadline kill");
    assert!(group_gone(blocked), "the blocked test's process group survived the deadline kill");
}
