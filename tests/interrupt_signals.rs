#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[path = "common/fifo.rs"]
mod fifo;

use cargo_val::edge_stream_unix::ChildStream;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
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
    let child = Command::new(CARGO_VAL)
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cargo-val binary is spawnable");
    let fixture = Fixture {
        child,
        group: None,
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
        let line = fifo::first_line(&ready, "the blocked process never reached the rendezvous").await;
        fixture.holder = line.trim().parse().expect("the blocked process wrote its pid");
        let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");
        assert!(alive(fixture.holder), "the blocked process is alive before the signal");

        assert_eq!(unsafe { libc::kill(cargo_val, $signal) }, 0);
        let status = fixture.child.wait().expect("cargo-val exits after the signal");
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
