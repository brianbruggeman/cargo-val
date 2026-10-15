#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const NOTICE: &str = "cargo-val: warn: run store unavailable at";

fn run_directory(environment: &[(&str, PathBuf); 2]) -> PathBuf {
    environment[0].1.join("cargo-val").join("run")
}

fn command(environment: &[(&str, PathBuf); 2]) -> Command {
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
        .env("CARGO_VAL_C52D_FAIL", "1")
        .envs(environment.clone());
    command
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn writable_again(run: &Path) {
    if let Ok(metadata) = std::fs::metadata(run) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(run, permissions).expect("the run directory is writable again");
    }
    let _ = std::fs::remove_dir_all(run);
}

#[proxima::test(timeout = "310s")]
async fn a_writable_target_gets_a_run_store_with_the_magic_and_the_exit_is_unchanged() {
    let environment = isolation::environment("run-store-writable");

    let output = fixture_output(&mut command(&environment)).await;

    let run = run_directory(&environment);
    let stores: Vec<_> = std::fs::read_dir(&run).expect("the run directory exists").flatten().collect();
    let data = std::fs::read(stores.first().expect("one run directory").path().join("data")).expect("the run left a data file");
    assert_eq!(output.status.code(), Some(1), "{}", stderr_of(&output));
    assert_eq!(stores.len(), 1);
    assert!(data.starts_with(b"CVRUN001"), "{data:?}");
    assert!(!stderr_of(&output).contains(NOTICE), "{}", stderr_of(&output));
}

#[proxima::test(timeout = "310s")]
async fn an_unwritable_run_directory_warns_once_and_the_run_exits_as_before() {
    let environment = isolation::environment("run-store-readonly");
    let run = run_directory(&environment);
    std::fs::create_dir_all(&run).expect("run directory is creatable");
    let mut permissions = std::fs::metadata(&run).expect("run directory exists").permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(&run, permissions).expect("run directory becomes read-only");

    let output = fixture_output(&mut command(&environment)).await;

    let stderr = stderr_of(&output);
    assert_eq!(output.status.code(), Some(1), "the failing fixture test exits 1 as with a writable store:\n{stderr}");
    assert_eq!(stderr.matches(NOTICE).count(), 1, "one notice:\n{stderr}");
    writable_again(&run);
}
