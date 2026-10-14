#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_val::edge_stream_unix::ChildStream;
use proxima_primitives::pipe::Pipe;

mod common;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fifo.rs"]
mod fifo;
#[path = "common/supervise.rs"]
mod supervise;

use common::{fixture_deadline, drain, drain_until, fixture_output};
use supervise::supervised;

const BINARY: &str = "cargo_val_blocked_fixture";
const TEST: &str = "tests::unit_blocks_when_requested";

type Blocked = (String, String, Option<i32>);

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from)
}

fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
fn open_ready(path: &Path) -> ChildStream {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read-write");
    ChildStream::new(OwnedFd::from(file))
}

fn open_held(path: &Path) -> ChildStream {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read without a writer");
    ChildStream::new(OwnedFd::from(file))
}

fn rendezvous(label: &str) -> PathBuf {
    let directory = target_directory().join("running-milestones").join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    directory
}

fn command(directory: &Path, arguments: &[&str], backoff: &str) -> Command {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocked_test/Cargo.toml");
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--lib", "--output", "plain"])
        .args(arguments)
        .env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", directory)
        .env("CARGO_VAL_STALL_BACKOFF", backoff)
        .env("CARGO_TARGET_DIR", target_directory().join("running-milestones-fixture"));
    for key in ["CI", "GITHUB_ACTIONS", "NO_COLOR", "TERM", "CARGO_VAL_OUTPUT", "CARGO_VAL_PROCESS", "CARGO_VAL_TEST_TIMEOUT", "RUST_TEST_THREADS"] {
        command.env_remove(key);
    }
    command
}

fn running_lines(text: &str) -> Vec<&str> {
    let Some((complete, _)) = text.rsplit_once('\n') else {
        return Vec::new();
    };
    complete.lines().filter(|line| line.starts_with(&format!("RUNNING  {BINARY} {TEST} age="))).collect()
}

fn age_seconds(line: &str) -> f64 {
    let age = line.split(' ').find_map(|column| column.strip_prefix("age=")).and_then(|column| column.strip_suffix('s'));
    age.and_then(|number| number.parse().ok()).unwrap_or_else(|| panic!("no age field in {line:?}"))
}

// the fixture blocks on `block` until this side opens it for writing; the open rendezvouses with the fixture's blocked open, so no wait is needed
async fn blocked_run(label: &str, arguments: &[&str], backoff: &str) -> Blocked {
    let directory = rendezvous(label);
    let ready = open_ready(&directory.join("ready"));
    let held = open_held(&directory.join("held"));
    let mut invocation = command(&directory, arguments, backoff);

    let outcome = supervised(&mut invocation, fixture_deadline(), async |_, stdout, _, exit| {
        let (seen, fault) = drain_until(stdout, |bytes| !running_lines(&String::from_utf8_lossy(bytes)).is_empty()).await;
        assert!(fault.is_none(), "stdout failed before the first milestone: {fault:?}");
        let before = String::from_utf8(seen).expect("plain output is UTF-8");
        let running = running_lines(&before).first().map_or_else(|| panic!("stdout ended with no RUNNING line:\n{before}"), |line| (*line).to_owned());
        let pid = fifo::first_line(&ready, "the fixture never reached the rendezvous").await;
        assert!(pid.trim().parse::<u32>().is_ok_and(|pid| pid > 0), "the fixture wrote its pid: {pid:?}");
        let mut released = OpenOptions::new().write(true).open(directory.join("block")).expect("the fixture holds the block fifo before announcing");
        released.write_all(b"1").expect("the release byte fits the fifo the blocked test reads");
        drop(released);
        let (after, fault) = drain(stdout).await;
        assert!(fault.is_none(), "stdout failed after the release: {fault:?}");
        let status = Pipe::call(exit, ()).await.expect("the exit source is unspent").expect("the exit status reads");
        (running, before + &String::from_utf8(after).expect("plain output is UTF-8"), status.code())
    })
    .await;
    fifo::until_eof(&held, "the held fifo never reached EOF: the blocked fixture still holds its write end").await;
    drop(ready);
    outcome
}

const FIRST_MILESTONE_AT_ONE_SECOND: &str = "exp:1s,2,5m";

#[proxima::test(timeout = "500s")]
async fn a_blocked_test_prints_running_once_at_its_first_milestone_then_passes_when_released() {
    let (running, text, code) = Box::pin(blocked_run("first", &[], FIRST_MILESTONE_AT_ONE_SECOND)).await;

    let age = age_seconds(&running);
    assert!((1.0..3.0).contains(&age), "the first milestone is at 1s and the second not before 3s: {running}");
    assert_eq!(running_lines(&text).len(), 1, "released right after the first milestone, so no second fires: {text}");
    assert!(text.lines().any(|line| line.starts_with(&format!("PASS     {BINARY}  {TEST}  "))), "{text}");
    assert!(text.lines().any(|line| line.starts_with("SUMMARY  passed=1 failed=0")), "{text}");
    assert_eq!(code, Some(0), "{text}");
}

#[proxima::test(timeout = "500s")]
async fn a_declared_test_timeout_shows_as_the_limit_on_the_running_line() {
    let (running, text, code) = Box::pin(blocked_run("limit", &["--test-timeout", "120"], FIRST_MILESTONE_AT_ONE_SECOND)).await;

    assert!(running.ends_with(" limit=120.000s"), "{running}");
    assert_eq!(code, Some(0), "{text}");
}

#[proxima::test(timeout = "500s")]
async fn without_a_declared_deadline_the_running_line_has_no_limit_field() {
    let (running, text, code) = Box::pin(blocked_run("nolimit", &[], FIRST_MILESTONE_AT_ONE_SECOND)).await;

    assert!(!running.contains("limit="), "{running}");
    assert_eq!(code, Some(0), "{text}");
}

#[proxima::test(timeout = "500s")]
async fn a_test_that_ends_before_its_first_milestone_prints_no_running_line() {
    let mut invocation = command(&target_directory(), &[], "exp:30s,2,5m");
    invocation.env_remove("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR");

    let output = fixture_output(&mut invocation).await;

    let text = String::from_utf8(output.stdout).expect("plain output is UTF-8");
    assert!(running_lines(&text).is_empty(), "{text}");
    assert!(text.lines().any(|line| line.starts_with("SUMMARY  passed=1 failed=0")), "{text}");
}

#[proxima::test(timeout = "500s")]
async fn a_plain_run_held_past_several_tick_periods_emits_only_the_milestone_line_and_no_terminal_control() {
    let (running, text, code) = Box::pin(blocked_run("plain", &[], FIRST_MILESTONE_AT_ONE_SECOND)).await;

    assert!(age_seconds(&running) >= 1.0, "{running}");
    assert!(!text.as_bytes().contains(&0x1b), "plain output holds no escape byte, so no frame was redrawn on a tick: {text:?}");
    assert_eq!(text.lines().filter(|line| line.starts_with("RUNNING")).count(), 1, "{text}");
    assert_eq!(code, Some(0), "{text}");
}
