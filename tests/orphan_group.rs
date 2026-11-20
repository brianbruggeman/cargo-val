#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const REAP_DEADLINE: Duration = Duration::from_secs(30);

fn gone(pid: libc::pid_t) -> bool {
    let signalled = unsafe { libc::kill(pid, 0) };
    signalled == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn probe_directory() -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("orphan-group").join(std::process::id().to_string());
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("probe directory is creatable under the target tmpdir");
    directory
}

fn reaped_within_deadline(pid: libc::pid_t) -> bool {
    let started = Instant::now();
    while started.elapsed() < REAP_DEADLINE {
        if gone(pid) {
            return true;
        }
        std::thread::yield_now();
    }
    gone(pid)
}

#[proxima::test(timeout = "310s")]
async fn cancelled_session_leaves_no_grandchild_running() {
    let directory = probe_directory();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/orphan_group/Cargo.toml");
    let mut command = binary::command();
    command
        .args(["--process", "test", "--output", "plain", "--manifest-path"])
        .arg(manifest)
        .args(["--", "--test-threads=2"])
        .env("CARGO_VAL_FIXTURE_ORPHAN_DIR", &directory)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env("NO_COLOR", "1")
        .envs(isolation::environment("orphan-group"));

    let output = fixture_output(&mut command).await;

    let text = fs::read_to_string(directory.join("grandchild")).unwrap_or_else(|failure| panic!("no grandchild pid ({failure}); stdout={} stderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)));
    let grandchild: libc::pid_t = text.trim().parse().expect("the pid file carries one pid");
    let reaped = reaped_within_deadline(grandchild);
    if !reaped {
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
    }
    assert!(reaped, "grandchild pid {grandchild} survived the cancelled session; stdout={}", String::from_utf8_lossy(&output.stdout));
}
