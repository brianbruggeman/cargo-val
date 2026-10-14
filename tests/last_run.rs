use std::path::PathBuf;
use std::process::{Command, Output};

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use cargo_val::last_run::{decode, encode};
use fixture::fixture_output;

const PREVIOUS_MARK: &str = "cargo-val: debug: last-run previous wall_ms=";

fn target_directory(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("last-run-{label}"))
}

fn record_path(label: &str) -> PathBuf {
    target_directory(label).join("cargo-val").join("last-run")
}

fn command(label: &str) -> Command {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_run/Cargo.toml"))
        .args(["--tests", "--no-fail-fast"])
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_VAL_OUTPUT", "plain")
        .env("CARGO_VAL_LOG", "debug")
        .env("CARGO_VAL_C52D_FAIL", "1")
        .env("CARGO_TARGET_DIR", target_directory(label));
    command
}

async fn run(label: &str) -> Output {
    fixture_output(&mut command(label)).await
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn fresh(label: &str) {
    let _ = std::fs::remove_file(record_path(label));
}

#[proxima::test(timeout = "310s")]
async fn the_second_run_state_carries_the_first_runs_wall_time_and_test_count() {
    fresh("chain");

    let first = run("chain").await;
    let after_first = std::fs::read(record_path("chain")).expect("the first run leaves a record");
    let (wall_ms, tests, exit, _, _, _) = decode(&after_first).expect("the record is well formed");
    let second = run("chain").await;

    assert!(!stderr_of(&first).contains(PREVIOUS_MARK), "no record existed before the first run:\n{}", stderr_of(&first));
    assert_eq!((tests > 0, exit), (true, 1), "the fixture discovers tests and its failing test exits 1");
    let expected = format!("{PREVIOUS_MARK}{wall_ms} tests={tests}\n");
    assert!(stderr_of(&second).contains(&expected), "second run's state carries {expected:?}:\n{}", stderr_of(&second));
    assert_eq!(first.status.code(), Some(1));
    assert_eq!(second.status.code(), Some(1));
}

#[proxima::test(timeout = "310s")]
async fn a_corrupt_record_is_ignored_and_replaced_by_the_run() {
    fresh("corrupt");
    std::fs::create_dir_all(record_path("corrupt").parent().expect("record has a directory")).expect("directory is creatable");
    let mut wrong_version = encode((9, 9, 0, 0, 0, 0));
    wrong_version[0] = 7;
    for damaged in [vec![1_u8, 2, 3], wrong_version.to_vec()] {
        std::fs::write(record_path("corrupt"), &damaged).expect("damaged record is writable");

        let output = run("corrupt").await;

        assert_eq!(output.status.code(), Some(1), "a damaged record never changes the exit:\n{}", stderr_of(&output));
        assert!(!stderr_of(&output).contains(PREVIOUS_MARK), "{}", stderr_of(&output));
        let replaced = std::fs::read(record_path("corrupt")).expect("the run rewrites the record");
        assert!(decode(&replaced).is_ok(), "{replaced:?}");
    }
}

#[proxima::test(timeout = "310s")]
async fn a_build_failure_after_a_completed_run_leaves_the_record_byte_for_byte() {
    fresh("build-failure");
    let passing = run("build-failure").await;
    let before = std::fs::read(record_path("build-failure")).expect("the completed run leaves a record");
    let mut failing = binary::command();
    failing
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_error/Cargo.toml"))
        .arg("--lib")
        .env_remove("CI")
        .env_remove("NO_COLOR")
        .env("CARGO_VAL_OUTPUT", "plain")
        .env("CARGO_TARGET_DIR", target_directory("build-failure"));

    let broken = fixture_output(&mut failing).await;

    let after = std::fs::read(record_path("build-failure")).expect("the record survives the build failure");
    assert_eq!(passing.status.code(), Some(1), "{}", stderr_of(&passing));
    assert_eq!(broken.status.code(), Some(3), "{}", stderr_of(&broken));
    assert_eq!(after, before, "a build failure must not replace the record");
}
