#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

#[path = "common/fifo.rs"]
mod fifo;

#[path = "common/exit_wait.rs"]
mod exit_wait;

#[path = "common/binary.rs"]
mod binary;

#[path = "common/group.rs"]
mod group;
#[path = "common/lead.rs"]
mod lead;
#[path = "common/wait.rs"]
mod wait;

use cargo_val::edge_stream_unix::ChildStream;
use exit_wait::{ExitSource, until_exit};
use core::num::NonZeroI32;

const SIGINT_STATUS: i32 = 130;
const SIGTERM_STATUS: i32 = 143;

struct Fixture {
    child: Child,
    group: Option<libc::pid_t>,
    holder: libc::pid_t,
    held: File,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            if let Some(group) = self.group {
                libc::kill(-group, libc::SIGKILL);
            }
            libc::kill(self.holder, libc::SIGKILL);
        }
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

fn open_nonblocking_reader(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
fn open_ready(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

fn start(label: &str, fixture: &str, arguments: &[&str], block_variable: &str) -> (Fixture, ChildStream) {
    start_as(label, fixture, arguments, block_variable, "rich", Stdio::null())
}

fn start_as(label: &str, fixture: &str, arguments: &[&str], block_variable: &str, output: &str, stdout: Stdio) -> (Fixture, ChildStream) {
    let directory = target_directory()
        .join("c74-rendezvous")
        .join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    let ready = open_ready(&directory.join("ready"));
    let held = open_nonblocking_reader(&directory.join("held"));
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/{fixture}/Cargo.toml"));
    let mut command = binary::command();
    lead::lead_group(&mut command);
    let child = command
        .env("CARGO_VAL_OUTPUT", output)
        .arg("--manifest-path")
        .arg(manifest)
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env("NO_COLOR", "1")
        .env(block_variable, &directory)
        .env(
            "CARGO_TARGET_DIR",
            target_directory().join(format!("c74-fixture-{label}")),
        )
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .expect("cargo-val binary is spawnable");
    let group = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
    let fixture = Fixture {
        child,
        group: Some(group),
        holder: 0,
        held,
    };
    (fixture, ChildStream::new(OwnedFd::from(ready)))
}

fn alive(target: libc::pid_t) -> bool {
    unsafe {
        libc::kill(target, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

// a macro rather than an async fn: each ChildStream owns a 20 KiB window, so awaiting a helper that holds one trips clippy::large_futures
macro_rules! interrupt {
    ($label:expr, $fixture:expr, $arguments:expr, $block_variable:expr, $signal:expr, $expected_status:expr) => {{
        let label: &str = $label;
        let (mut fixture, ready) = start(label, $fixture, $arguments, $block_variable);
        let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");
        let exit = ExitSource::new(NonZeroI32::new(cargo_val).expect("a spawned pid is nonzero"));
        let what = "the blocked process never reached the rendezvous";
        let line = until_exit(fifo::first_line(&ready, what), what, &exit, &[]).await;
        fixture.holder = line.trim().parse().expect("the blocked process wrote its pid");
        assert!(alive(fixture.holder), "the blocked process is alive before the signal");

        assert_eq!(unsafe { libc::kill(cargo_val, $signal) }, 0);
        let status = wait::wait_within(&fixture.child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not exit after the signal: {failure}"));
        let held = ChildStream::new(OwnedFd::from(fixture.held.try_clone().expect("the held fifo descriptor duplicates")));
        fifo::until_eof(&held, label).await;

        println!(
            "{{\"event\":\"c74_interrupt\",\"label\":\"{label}\",\"signal\":{},\"status\":{:?},\"held_fifo\":\"eof\"}}",
            $signal,
            status.code()
        );
        assert_eq!(status.code(), Some($expected_status), "{label}");
    }};
}

#[proxima::test(timeout = "310s")]
async fn c72_doc_only_run_forwards_sigint_to_the_doctest_group_and_exits_130() {
    interrupt!("doc-int", "doc_tests", &["--doc"], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGINT, SIGINT_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c72_doc_only_run_forwards_sigterm_to_the_doctest_group_and_exits_143() {
    interrupt!("doc-term", "doc_tests", &["--doc"], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGTERM, SIGTERM_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c72_default_run_forwards_sigint_to_the_doctest_group_and_exits_130() {
    interrupt!("all-int", "doc_tests", &[], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGINT, SIGINT_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c72_default_run_forwards_sigterm_to_the_doctest_group_and_exits_143() {
    interrupt!("all-term", "doc_tests", &[], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGTERM, SIGTERM_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c74_running_test_receives_sigint_sent_to_cargo_val_alone_and_exits_130() {
    interrupt!(
        "test-int",
        "blocked_test",
        &["--lib", "unit_blocks_when_requested"],
        "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR",
        libc::SIGINT,
        SIGINT_STATUS
    );
}

#[proxima::test(timeout = "310s")]
async fn an_interrupted_agent_run_names_the_signal_and_the_blocked_test_and_closes_with_exit_130() {
    let (mut fixture, ready) = start_as(
        "agent-int",
        "blocked_test",
        &["--lib", "unit_blocks_when_requested"],
        "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR",
        "agent",
        Stdio::piped(),
    );
    let line = fifo::first_line(&ready, "the blocked test never reached the rendezvous").await;
    fixture.holder = line.trim().parse().expect("the blocked process wrote its pid");
    let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");

    assert_eq!(unsafe { libc::kill(cargo_val, libc::SIGINT) }, 0);
    let status = wait::wait_within(&fixture.child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not exit after the signal: {failure}"));
    let mut report = String::new();
    fixture.child.stdout.take().expect("stdout is piped").read_to_string(&mut report).expect("agent output is UTF-8");

    assert_eq!(status.code(), Some(SIGINT_STATUS), "{report}");
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 4, "header, INTERRUPTED, RUNNING, EXIT:\n{report}");
    assert_eq!(lines[1], "INTERRUPTED  signal=2  forwarded_groups=1", "{report}");
    assert_eq!(lines[2], "RUNNING  cargo_val_blocked_fixture tests::unit_blocks_when_requested", "{report}");
    assert_eq!(lines[3], "EXIT     130  interrupted", "{report}");
}
