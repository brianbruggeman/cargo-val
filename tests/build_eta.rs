use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

mod common;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fixture.rs"]
mod fixture;

use fixture::fixture_output;

const PREDICTED_PREFIX: &str = "cargo-val: debug: eta units predicted=";
const MATCHED_YES: &str = "eta fingerprint-matched=yes";
const MATCHED_NO: &str = "eta fingerprint-matched=no";
const TOLERANCE_PERCENT: u32 = 10;

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("the destination is creatable");
    for entry in fs::read_dir(from).expect("the source is listable") {
        let entry = entry.expect("the entry is readable");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("the entry has a type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("the file is copyable");
        }
    }
}

async fn run_fixture(manifest: &Path, target: &Path) -> Output {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--no-fail-fast")
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CLAUDECODE")
        .env_remove("TERM")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_LOG", "debug")
        .env("CARGO_TARGET_DIR", target);
    fixture_output(&mut command).await
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn predicted_units(stderr: &str) -> u32 {
    let line = stderr.lines().find(|line| line.starts_with(PREDICTED_PREFIX)).unwrap_or_else(|| panic!("no predicted-units line in:\n{stderr}"));
    line[PREDICTED_PREFIX.len()..].trim().parse().unwrap_or_else(|_| panic!("predicted units are a number: {line}"))
}

fn record_of(target: &Path) -> cargo_val::last_run::Record {
    let bytes = fs::read(target.join("cargo-val").join("last-run")).expect("the run leaves a record");
    cargo_val::last_run::decode(&bytes).expect("the record is well formed")
}

fn private_target(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("build-eta-{label}-{}", std::process::id()))
}

#[proxima::test(timeout = "900s")]
async fn a_cold_workspace_build_is_predicted_within_tolerance_then_recognised_exactly_until_its_manifest_changes() {
    let scratch = tempfile::tempdir().expect("scratch directory is creatable");
    let workspace = scratch.path().join("build_eta");
    copy_tree(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_eta"), &workspace);
    let manifest = workspace.join("Cargo.toml");
    let target = private_target("fixture");
    let _ = fs::remove_dir_all(&target);

    let cold = run_fixture(&manifest, &target).await;
    let (_, _, _, fingerprint, actual, build_ms) = record_of(&target);
    let predicted = predicted_units(&stderr_of(&cold));
    let error = predicted.abs_diff(actual);
    println!("predicted={predicted} actual={actual} error={error} build_ms={build_ms}");

    assert!(actual > 0 && fingerprint != 0 && build_ms > 0, "the cold run recorded its units, fingerprint and build time: {actual} {fingerprint} {build_ms}");
    assert!(!stderr_of(&cold).contains(MATCHED_YES), "{}", stderr_of(&cold));
    assert!(error * 100 <= actual * TOLERANCE_PERCENT, "predicted {predicted} against actual {actual} is off by more than {TOLERANCE_PERCENT}%");

    let warm = run_fixture(&manifest, &target).await;
    assert!(stderr_of(&warm).contains(MATCHED_YES), "an unchanged manifest and lock match the recorded fingerprint:\n{}", stderr_of(&warm));
    assert_eq!(record_of(&target).3, fingerprint);

    let mut changed = fs::read_to_string(&manifest).expect("the manifest is readable");
    changed.push_str("# touched\n");
    fs::write(&manifest, changed).expect("the manifest is writable");
    let touched = run_fixture(&manifest, &target).await;
    assert!(stderr_of(&touched).contains(MATCHED_NO), "a changed manifest falls back to the prediction:\n{}", stderr_of(&touched));
    assert_ne!(record_of(&target).3, fingerprint);
    let _ = fs::remove_dir_all(&target);
}
