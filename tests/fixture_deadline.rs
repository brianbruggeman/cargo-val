#![cfg(unix)]

use core::future::Future;
use core::pin::pin;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use futures::future::{Either, join3, select};
use proxima_primitives::pipe::Pipe;

use cargo_val::edge_stream_unix::ChildStream;

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fifo.rs"]
mod fifo;

use common::{ExitSource, Finished, drain, finish, group, launch, release_group};
use fixture::fixture_output;

// the expiry is driven by the test, not by a clock, so the blocked child is killed only once it has announced itself
async fn output_until(command: &mut Command, expire: impl Future<Output = ()>) -> Result<Output, String> {
    let (leader, stdout, stderr, exit): (u32, _, _, ExitSource) = launch(command)?;
    let mut collect = pin!(join3(drain(&stdout), drain(&stderr), exit.call(())));
    let mut expire = pin!(expire);
    let (finished, report): (Finished, Option<String>) = match select(collect.as_mut(), expire.as_mut()).await {
        Either::Left((finished, _)) => (finished, None),
        Either::Right(((), _)) => {
            let (finished, failures) = group::interrupt_then_kill(leader, collect.as_mut()).await;
            (finished, Some(format!("signal failures: {failures:?}")))
        }
    };
    release_group(leader);
    finish(command, leader, finished, report)
}


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
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--lib", "unit_blocks_when_requested"])
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env("NO_COLOR", "1")
        .envs(isolation::environment("fixture-deadline"));
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
async fn a_fixture_run_that_misses_its_deadline_fails_and_leaves_no_process_behind() {
    let warm = fixture_output(&mut blocked_test_command(None)).await;
    assert_eq!(warm.status.code(), Some(0), "the fixture builds and its unblocked test passes");
    let directory = probe_directory();
    let ready = ChildStream::new(OwnedFd::from(open_ready(&directory.join("ready"))));
    let held = ChildStream::new(OwnedFd::from(open_reader(&directory.join("held"))));
    let mut command = blocked_test_command(Some(&directory));

    let mut line = String::new();
    let expiry = async { line = fifo::first_line(&ready, "the blocked test never reported it was blocked").await };
    let failure = output_until(&mut command, expiry).await.expect_err("the blocked test never returns, so the driven expiry fires");

    let blocked: libc::pid_t = line.trim().parse().expect("the ready fifo carries one pid");
    assert!(failure.contains("outlived"), "{failure}");
    assert!(failure.contains("unit_blocks_when_requested"), "{failure}");
    fifo::until_eof(&held, "the held fifo never reached EOF: the blocked test still holds its write end").await;
    assert!(gone(blocked), "the blocked test (pid {blocked}) survived the deadline kill");
    assert!(group_gone(blocked), "the blocked test's process group survived the deadline kill");
}
