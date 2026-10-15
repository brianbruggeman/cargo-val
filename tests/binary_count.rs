use std::path::PathBuf;
use std::process::Output;

mod common;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fixture.rs"]
mod fixture;

use fixture::fixture_output;

const NOTHING: &str = "no_test_carries_this_name";

fn target_directory(name: &str) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from).join(name)
}

async fn run(manifest: &str, flags: &[&str], target: &str) -> Output {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(manifest))
        .args(["--output", "plain", "--no-fail-fast"])
        .args(flags)
        .arg(NOTHING)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_FILTERSET")
        .env_remove("CARGO_VAL_IGNORED")
        .env("CARGO_VAL_LOG", "debug")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory(target));
    fixture_output(&mut command).await
}

// the up-front count is the debug line the metadata read prints; the listed count is the closing block's `LISTED binaries=` line, cross-checked against its per-binary lines
fn counts(output: &Output) -> (usize, usize, usize) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let up_front = stderr
        .lines()
        .find_map(|line| line.strip_prefix("cargo-val: debug: metadata test-binaries="))
        .and_then(|found| found.parse().ok())
        .unwrap_or_else(|| panic!("no up-front count\n{stdout}{stderr}"));
    let listed = stdout
        .lines()
        .find_map(|line| line.strip_prefix("LISTED   binaries="))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|found| found.parse().ok())
        .unwrap_or_else(|| panic!("no LISTED line\n{stdout}{stderr}"));
    let per_binary = stdout
        .lines()
        .filter(|line| line.contains(": discovered ") && !line.starts_with("total: ") && !line.starts_with("doc-tests: "))
        .count();
    (up_front, listed, per_binary)
}

async fn assert_up_front_matches_listed(manifest: &str, flags: &[&str], target: &str, expected: usize) {
    let output = run(manifest, flags, target).await;

    let (up_front, listed, per_binary) = counts(&output);
    let context = format!("{manifest} {flags:?}\n{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert_eq!(up_front, expected, "{context}");
    assert_eq!(listed, up_front, "{context}");
    assert_eq!(per_binary, listed, "{context}");
}

const DEMO: &str = "examples/demo/Cargo.toml";
const SELF: &str = "Cargo.toml";

#[proxima::test(timeout = "310s")]
async fn demo_default_selection_counts_its_library_and_its_integration_test() {
    assert_up_front_matches_listed(DEMO, &[], "binary-count-demo", 2).await;
}

#[proxima::test(timeout = "310s")]
async fn demo_lib_flag_counts_only_the_library_harness() {
    assert_up_front_matches_listed(DEMO, &["--lib"], "binary-count-demo", 1).await;
}

#[proxima::test(timeout = "310s")]
async fn demo_test_flag_counts_only_the_named_integration_test() {
    assert_up_front_matches_listed(DEMO, &["--test", "end_to_end"], "binary-count-demo", 1).await;
}

#[proxima::test(timeout = "310s")]
async fn demo_tests_flag_counts_every_tested_target() {
    assert_up_front_matches_listed(DEMO, &["--tests"], "binary-count-demo", 2).await;
}

#[proxima::test(timeout = "1200s")]
async fn this_workspace_lib_flag_counts_the_library_harness() {
    assert_up_front_matches_listed(SELF, &["--lib"], "binary-count-self", 1).await;
}

#[proxima::test(timeout = "1200s")]
async fn this_workspace_bins_flag_counts_both_binary_harnesses() {
    assert_up_front_matches_listed(SELF, &["--bins"], "binary-count-self", 2).await;
}

#[proxima::test(timeout = "1200s")]
async fn this_workspace_named_test_flag_counts_one_integration_test() {
    assert_up_front_matches_listed(SELF, &["--test", "cli_arguments"], "binary-count-self", 1).await;
}

#[proxima::test(timeout = "1200s")]
async fn this_workspace_package_and_two_selectors_count_both() {
    assert_up_front_matches_listed(SELF, &["-p", "cargo-val", "--lib", "--bins"], "binary-count-self", 3).await;
}
