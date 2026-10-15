#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/supervise.rs"]
mod supervise;

use common::{drain, drain_until};
use fixture::{fixture_deadline, fixture_output};
use supervise::supervised;

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from)
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
        .env("CARGO_TARGET_DIR", target_directory().join(format!("pipeline-overlap-{label}")));
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

// opening the write end rendezvouses with every reader the fixture has parked on the fifo; holding it open keeps later readers from parking
fn release(fifo: &Path) -> fs::File {
    OpenOptions::new().write(true).open(fifo).expect("the fixture holds the fifo open for reading before announcing")
}

#[proxima::test(timeout = "500s")]
async fn a_test_binary_launches_while_a_later_package_is_still_held_compiling() {
    let build_fifo = make_fifo("build");
    let mut invocation = command("build", &["--workspace"]);
    invocation.env("CARGO_VAL_FIXTURE_BUILD_FIFO", &build_fifo).env_remove("CARGO_VAL_FIXTURE_TEST_FIFO");

    let (before_release, after_release) = supervised(&mut invocation, fixture_deadline(), async |_, stdout, stderr, _| {
        let (seen, fault) = drain_until(stderr, |bytes| has_step(bytes, "test-launched", "doubling_a_request_count_is_exact")).await;
        assert!(fault.is_none(), "stderr failed before the first launch: {fault:?}");
        let writer = release(&build_fifo);
        let (rest, fault) = drain(stderr).await;
        assert!(fault.is_none(), "stderr failed after the release: {fault:?}");
        let (_, fault) = drain(stdout).await;
        assert!(fault.is_none(), "stdout failed after the release: {fault:?}");
        drop(writer);
        (String::from_utf8_lossy(&seen).into_owned(), String::from_utf8_lossy(&rest).into_owned())
    })
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

    let seen = supervised(&mut invocation, fixture_deadline(), async |_, stdout, stderr, _| {
        let (seen, fault) = drain_until(stderr, |bytes| binaries.iter().all(|name| has_step(bytes, "artifact-arrived", name))).await;
        assert!(fault.is_none(), "stderr failed before every artifact arrived: {fault:?}");
        let writer = release(&test_fifo);
        let (rest, _) = drain(stderr).await;
        let (_, _) = drain(stdout).await;
        drop(writer);
        let mut all = seen;
        all.extend_from_slice(&rest);
        String::from_utf8_lossy(&all).into_owned()
    })
    .await;

    assert!(seen.contains("step=test-launched"), "a test was held on the only job slot:\n{seen}");
}

#[proxima::test(timeout = "500s")]
async fn the_doc_cargo_builds_and_runs_while_the_unit_tests_are_held() {
    let test_fifo = make_fifo("doc");
    let mut invocation = command("doc", &["-p", "pipeline_quick"]);
    invocation.env("CARGO_VAL_FIXTURE_TEST_FIFO", &test_fifo).env_remove("CARGO_VAL_FIXTURE_BUILD_FIFO");

    let (held, finished) = supervised(&mut invocation, fixture_deadline(), async |_, stdout, stderr, _| {
        let (held, fault) = drain_until(stderr, |bytes| has_step(bytes, "doc-cargo-spawned", "") && has_step(bytes, "doc-tests-running", "")).await;
        assert!(fault.is_none(), "stderr failed before the doc tests ran: {fault:?}");
        let writer = release(&test_fifo);
        let (rest, _) = drain(stderr).await;
        let (_, _) = drain(stdout).await;
        drop(writer);
        (String::from_utf8_lossy(&held).into_owned(), String::from_utf8_lossy(&rest).into_owned())
    })
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
    let mut invocation = command("listing", &["-p", "pipeline_listed", "--tests"]);
    invocation.env("CARGO_VAL_FIXTURE_LIST_FIFO", &list_fifo).env("CARGO_VAL_JOBS", "4");
    let binaries = ["lister_one", "lister_two", "lister_three", "lister_four"];

    let (held, stdout_text) = supervised(&mut invocation, fixture_deadline(), async |_, stdout, stderr, _| {
        let (held, fault) = drain_until(stderr, |bytes| binaries.iter().all(|name| has_step(bytes, "listing-spawned", name))).await;
        assert!(fault.is_none(), "stderr failed before every listing was in flight: {fault:?}");
        let writer = release(&list_fifo);
        let (_, _) = drain(stderr).await;
        let (printed, _) = drain(stdout).await;
        drop(writer);
        (String::from_utf8_lossy(&held).into_owned(), String::from_utf8_lossy(&printed).into_owned())
    })
    .await;

    assert!(!held.contains("step=test-launched"), "a test launched while every listing was held:\n{held}");
    assert!(stdout_text.contains("passed=4"), "the four listed tests pass once the listings are released:\n{stdout_text}");
}
