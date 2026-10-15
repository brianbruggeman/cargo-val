#![cfg(unix)]

use core::time::Duration;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/exit_wait.rs"]
mod exit_wait;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/supervise.rs"]
mod supervise;
#[path = "common/wait_log.rs"]
mod wait_log;

use common::{ChildStream, drain, drain_until};
use exit_wait::{ExitSource, until_exit};
use fixture::fixture_output;
use supervise::supervised;
use wait_log::{Log, guarded, stamp};

const RUN_BUDGET: Duration = Duration::from_secs(280);
const CHILD_BUDGET: Duration = Duration::from_secs(250);

// only the four held binaries: the package lib has a listing the fifo does not hold, and it would free a slot
const LISTERS: [&str; 10] = ["-p", "pipeline_listed", "--test", "lister_one", "--test", "lister_two", "--test", "lister_three", "--test", "lister_four"];

fn target_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn make_fifo(label: &str) -> PathBuf {
    let directory = target_directory().join("pipeline-overlap").join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("fifo directory is creatable");
    let path = directory.join("hold");
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
    path
}

fn command(label: &str, arguments: &[&str]) -> Command {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pipeline_overlap/Cargo.toml");
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--output", "plain", "--no-fail-fast"])
        .args(arguments)
        .env("CARGO_VAL_LOG", "debug")
        .env("CARGO_VAL_JOBS", "1")
        .envs(isolation::environment(&format!("pipeline-overlap-{label}")));
    for key in ["CI", "GITHUB_ACTIONS", "NO_COLOR", "TERM", "CARGO_VAL_OUTPUT", "CARGO_VAL_PROCESS", "CARGO_VAL_FILTERSET", "RUST_TEST_THREADS"] {
        command.env_remove(key);
    }
    command
}

fn has_step(text: &[u8], step: &str, name: &str) -> bool {
    let needle = format!("step={step} name={name}");
    String::from_utf8_lossy(text).lines().any(|line| line.contains("debug: pipeline ms=") && line.contains(&needle))
}

fn has_quit(text: &[u8], step: &str) -> bool {
    String::from_utf8_lossy(text).lines().any(|line| line.contains("debug: quit ms=") && line.contains(&format!("step={step} ")))
}

fn spawned(text: &[u8]) -> usize {
    String::from_utf8_lossy(text).matches("step=listing-spawned").count()
}

// read and write on one descriptor never blocks in open, and counts as the writer every parked reader of the fifo is waiting for
fn release(fifo: &Path) -> fs::File {
    OpenOptions::new().read(true).write(true).open(fifo).expect("the harness-made fifo opens for holding")
}

async fn until_seen(log: &Log, wait: &str, (stderr, exit): (&ChildStream, &ExitSource), done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    stamp(log, wait, "enter");
    let (seen, fault) = until_exit(drain_until(stderr, done), wait, exit, &[("stderr", stderr)]).await;
    stamp(log, wait, "exit");
    assert!(fault.is_none(), "{wait}: stderr failed: {fault:?}");
    seen
}

async fn to_the_end(log: &Log, stdout: &ChildStream, stderr: &ChildStream) -> (String, String) {
    stamp(log, "stderr_to_eof", "enter");
    let (rest, _) = drain(stderr).await;
    stamp(log, "stderr_to_eof", "exit");
    stamp(log, "stdout_to_eof", "enter");
    let (printed, _) = drain(stdout).await;
    stamp(log, "stdout_to_eof", "exit");
    (String::from_utf8_lossy(&rest).into_owned(), String::from_utf8_lossy(&printed).into_owned())
}

#[proxima::test(timeout = "500s")]
async fn a_test_binary_launches_while_a_later_package_is_still_held_compiling() {
    let build_fifo = make_fifo("build");
    let mut invocation = command("build", &["--workspace"]);
    invocation.env("CARGO_VAL_FIXTURE_BUILD_FIFO", &build_fifo).env_remove("CARGO_VAL_FIXTURE_TEST_FIFO");
    let log = Log::default();

    let (before_release, after_release) = guarded("the held build run", (&log, RUN_BUDGET), supervised(&mut invocation, CHILD_BUDGET, async |_, stdout, stderr, exit| {
        let seen = until_seen(&log, "first_launch", (stderr, exit), |bytes| has_step(bytes, "test-launched", "doubling_a_request_count_is_exact")).await;
        let writer = release(&build_fifo);
        stamp(&log, "released", "exit");
        let (rest, _) = to_the_end(&log, stdout, stderr).await;
        drop(writer);
        (String::from_utf8_lossy(&seen).into_owned(), rest)
    }))
    .await;

    assert!(!has_quit(before_release.as_bytes(), "build-cargo-exited"), "the build cargo had already exited when the first test launched:\n{before_release}");
    assert!(!has_step(before_release.as_bytes(), "build-finished-ok", ""), "the build had already finished when the first test launched:\n{before_release}");
    assert!(has_quit(after_release.as_bytes(), "build-cargo-exited"), "the build cargo exits once the fifo is released:\n{after_release}");
}

#[proxima::test(timeout = "500s")]
async fn artifacts_keep_arriving_while_every_job_slot_is_held_by_a_running_test() {
    let test_fifo = make_fifo("slots");
    let mut invocation = command("slots", &["-p", "pipeline_quick", "--lib", "--tests"]);
    invocation.env("CARGO_VAL_FIXTURE_TEST_FIFO", &test_fifo).env_remove("CARGO_VAL_FIXTURE_BUILD_FIFO");
    let binaries = ["pipeline_quick", "alpha", "gamma", "delta"];
    let log = Log::default();

    let seen = guarded("the held slots run", (&log, RUN_BUDGET), supervised(&mut invocation, CHILD_BUDGET, async |_, stdout, stderr, exit| {
        let seen = until_seen(&log, "every_artifact", (stderr, exit), |bytes| binaries.iter().all(|name| has_step(bytes, "artifact-arrived", name))).await;
        let writer = release(&test_fifo);
        let (rest, _) = to_the_end(&log, stdout, stderr).await;
        drop(writer);
        String::from_utf8_lossy(&seen).into_owned() + &rest
    }))
    .await;

    assert!(seen.contains("step=test-launched"), "a test was held on the only job slot:\n{seen}");
}

#[proxima::test(timeout = "500s")]
async fn the_doc_cargo_builds_and_runs_while_the_unit_tests_are_held() {
    let test_fifo = make_fifo("doc");
    let mut invocation = command("doc", &["-p", "pipeline_quick"]);
    invocation.env("CARGO_VAL_FIXTURE_TEST_FIFO", &test_fifo).env_remove("CARGO_VAL_FIXTURE_BUILD_FIFO");
    let log = Log::default();

    let (held, finished) = guarded("the held doc run", (&log, RUN_BUDGET), supervised(&mut invocation, CHILD_BUDGET, async |_, stdout, stderr, exit| {
        let held = until_seen(&log, "doc_tests_running", (stderr, exit), |bytes| has_step(bytes, "doc-cargo-spawned", "") && has_step(bytes, "doc-tests-running", "")).await;
        let writer = release(&test_fifo);
        let (rest, _) = to_the_end(&log, stdout, stderr).await;
        drop(writer);
        (String::from_utf8_lossy(&held).into_owned(), rest)
    }))
    .await;

    assert!(!has_quit(held.as_bytes(), "child-reaped"), "a unit test had already exited when the doc tests ran; the harness-held tests cannot exit:\n{held}");
    assert!(has_quit(finished.as_bytes(), "doc-cargo-exited") || has_quit(held.as_bytes(), "doc-cargo-exited"), "the doc cargo exits:\n{held}\n{finished}");
}

#[proxima::test(timeout = "500s")]
async fn an_unheld_run_stamps_every_binary_the_build_and_the_doc_tests() {
    let mut invocation = command("free", &["--workspace"]);
    invocation.env_remove("CARGO_VAL_FIXTURE_BUILD_FIFO").env_remove("CARGO_VAL_FIXTURE_TEST_FIFO");

    let output = fixture_output(&mut invocation).await;

    let stderr = output.stderr;
    for binary in ["pipeline_quick", "alpha", "gamma", "delta", "pipeline_slow", "beta"] {
        assert!(has_step(&stderr, "artifact-arrived", binary), "{binary} arrived:\n{}", String::from_utf8_lossy(&stderr));
    }
    assert!(has_step(&stderr, "build-finished-ok", ""), "{}", String::from_utf8_lossy(&stderr));
    assert!(has_step(&stderr, "doc-cargo-spawned", "") && has_step(&stderr, "doc-tests-running", ""), "{}", String::from_utf8_lossy(&stderr));
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stdout));
}

#[proxima::test(timeout = "500s")]
async fn every_held_listing_is_in_flight_at_once_up_to_the_jobs_limit() {
    let list_fifo = make_fifo("listing");
    let mut invocation = command("listing", &LISTERS);
    invocation.env("CARGO_VAL_FIXTURE_LIST_FIFO", &list_fifo).env("CARGO_VAL_JOBS", "4");
    let binaries = ["lister_one", "lister_two", "lister_three", "lister_four"];
    let log = Log::default();

    let (held, stdout_text) = guarded("the held listing run", (&log, RUN_BUDGET), supervised(&mut invocation, CHILD_BUDGET, async |_, stdout, stderr, exit| {
        let held = until_seen(&log, "every_listing", (stderr, exit), |bytes| binaries.iter().all(|name| has_step(bytes, "listing-spawned", name))).await;
        let writer = release(&list_fifo);
        let (_, printed) = to_the_end(&log, stdout, stderr).await;
        drop(writer);
        (String::from_utf8_lossy(&held).into_owned(), printed)
    }))
    .await;

    assert!(!held.contains("step=test-launched"), "a test launched while every listing was held:\n{held}");
    assert!(stdout_text.contains("passed=4"), "the four listed tests pass once the listings are released:\n{stdout_text}");
}

#[proxima::test(timeout = "500s")]
async fn at_sixty_four_jobs_listings_stop_short_of_the_signal_table_and_the_run_still_passes() {
    let list_fifo = make_fifo("sixty-four");
    let mut invocation = command("sixty-four", &LISTERS);
    invocation.env("CARGO_VAL_FIXTURE_LIST_FIFO", &list_fifo).env("CARGO_VAL_JOBS", "64");
    let log = Log::default();

    let (held, stdout_text, stderr_text) = guarded("the sixty-four job run", (&log, RUN_BUDGET), supervised(&mut invocation, CHILD_BUDGET, async |_, stdout, stderr, exit| {
        let held = until_seen(&log, "three_listings", (stderr, exit), |bytes| spawned(bytes) >= 3).await;
        let writer = release(&list_fifo);
        let (rest, printed) = to_the_end(&log, stdout, stderr).await;
        drop(writer);
        (String::from_utf8_lossy(&held).into_owned(), printed, rest)
    }))
    .await;

    assert_eq!(spawned(held.as_bytes()), 3, "73 signal slots less 64 tests and the build, doc and editor groups leave three listings:\n{held}");
    assert!(stdout_text.contains("passed=4"), "all four listed tests pass once released:\n{stdout_text}\n{stderr_text}");
    assert!(!stderr_text.contains("signal table"), "no signal-table refusal:\n{stderr_text}");
}
