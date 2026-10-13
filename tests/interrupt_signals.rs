#![cfg(unix)]

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const READY_TIMEOUT_MILLISECONDS: i32 = 300_000;
const EOF_TIMEOUT_SECONDS: libc::time_t = 60;
const SIGINT_STATUS: i32 = 130;
const SIGTERM_STATUS: i32 = 143;

struct Fixture {
    child: Child,
    group: Option<libc::pid_t>,
    holder: libc::pid_t,
    held: libc::c_int,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            if let Some(group) = self.group {
                libc::kill(-group, libc::SIGKILL);
            }
            libc::kill(self.holder, libc::SIGKILL);
            libc::close(self.held);
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

fn open_nonblocking_reader(path: &Path) -> libc::c_int {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let descriptor = unsafe { libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    assert!(
        descriptor >= 0,
        "open {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    descriptor
}

fn read_line(descriptor: libc::c_int) -> String {
    let mut collected = Vec::new();
    while !collected.contains(&b'\n') {
        let mut watched = libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&raw mut watched, 1, READY_TIMEOUT_MILLISECONDS) };
        assert_eq!(ready, 1, "the doctest never reached the rendezvous");
        let mut bytes = [0_u8; 64];
        let count = unsafe { libc::read(descriptor, bytes.as_mut_ptr().cast(), bytes.len()) };
        let count = usize::try_from(count).expect("read from the ready fifo succeeds");
        assert!(
            count > 0,
            "the ready fifo closed before a full line arrived"
        );
        collected.extend_from_slice(&bytes[..count]);
    }
    String::from_utf8(collected).expect("the pid line is text")
}

fn start(label: &str, fixture: &str, arguments: &[&str], block_variable: &str) -> Fixture {
    let directory = target_directory()
        .join("c74-rendezvous")
        .join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    let ready = open_nonblocking_reader(&directory.join("ready"));
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
    let mut fixture = Fixture {
        child,
        group: None,
        holder: 0,
        held,
    };
    let line = read_line(ready);
    unsafe { libc::close(ready) };
    fixture.holder = line
        .trim()
        .parse()
        .expect("the blocked process wrote its pid");
    fixture
}

// poll(2) never reports a fifo whose last writer left on macOS; select(2) does
fn await_eof(descriptor: libc::c_int, label: &str) {
    let mut readable: libc::fd_set = unsafe { core::mem::zeroed() };
    unsafe {
        libc::FD_ZERO(&raw mut readable);
        libc::FD_SET(descriptor, &raw mut readable);
    }
    let mut timeout = libc::timeval {
        tv_sec: EOF_TIMEOUT_SECONDS,
        tv_usec: 0,
    };
    let ready = unsafe {
        libc::select(
            descriptor + 1,
            &raw mut readable,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &raw mut timeout,
        )
    };
    assert_eq!(ready, 1, "{label}: the held fifo never reached EOF");
    let mut bytes = [0_u8; 1];
    let count = unsafe { libc::read(descriptor, bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(
        count, 0,
        "{label}: the held fifo carried data instead of EOF"
    );
}

fn alive(target: libc::pid_t) -> bool {
    unsafe {
        libc::kill(target, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

fn interrupt(
    label: &str,
    fixture: &str,
    arguments: &[&str],
    block_variable: &str,
    signal: libc::c_int,
    expected_status: i32,
) {
    let mut fixture = start(label, fixture, arguments, block_variable);
    let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");
    assert!(
        alive(fixture.holder),
        "the blocked process is alive before the signal"
    );

    assert_eq!(unsafe { libc::kill(cargo_val, signal) }, 0);
    let status = fixture
        .child
        .wait()
        .expect("cargo-val exits after the signal");
    await_eof(fixture.held, label);

    println!(
        "{{\"event\":\"c74_interrupt\",\"label\":\"{label}\",\"signal\":{signal},\"status\":{:?},\"held_fifo\":\"eof\"}}",
        status.code()
    );
    assert_eq!(status.code(), Some(expected_status), "{label}");
}

fn interrupt_doctest(label: &str, arguments: &[&str], signal: libc::c_int, expected_status: i32) {
    interrupt(
        label,
        "doc_tests",
        arguments,
        "CARGO_VAL_FIXTURE_BLOCK_DIR",
        signal,
        expected_status,
    );
}

#[proxima::test]
async fn c72_doc_only_run_forwards_sigint_to_the_doctest_group_and_exits_130() {
    interrupt_doctest("doc-int", &["--doc"], libc::SIGINT, SIGINT_STATUS);
}

#[proxima::test]
async fn c72_doc_only_run_forwards_sigterm_to_the_doctest_group_and_exits_143() {
    interrupt_doctest("doc-term", &["--doc"], libc::SIGTERM, SIGTERM_STATUS);
}

#[proxima::test]
async fn c72_default_run_forwards_sigint_to_the_doctest_group_and_exits_130() {
    interrupt_doctest("all-int", &[], libc::SIGINT, SIGINT_STATUS);
}

#[proxima::test]
async fn c72_default_run_forwards_sigterm_to_the_doctest_group_and_exits_143() {
    interrupt_doctest("all-term", &[], libc::SIGTERM, SIGTERM_STATUS);
}

#[proxima::test]
async fn c74_running_test_receives_sigint_sent_to_cargo_val_alone_and_exits_130() {
    interrupt(
        "test-int",
        "blocked_test",
        &["--lib", "unit_blocks_when_requested"],
        "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR",
        libc::SIGINT,
        SIGINT_STATUS,
    );
}
