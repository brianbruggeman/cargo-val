use std::path::PathBuf;
use std::process::Output;

mod common;
#[path = "common/binary.rs"]
mod binary;

use common::fixture_output;

const ESCAPE: u8 = 0x1b;
const GREEN_BUDGET_BYTES: usize = 200;
const FAILING_TEST: &str = "fixture::fails_when_requested";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).join("Cargo.toml")
}

async fn run(fixture_name: &str, environment: &[(&str, &str)], arguments: &[&str]) -> Output {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(fixture(fixture_name))
        .args(["--tests", "--no-fail-fast"])
        .args(arguments)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("agent-fixture-target"))
        .envs(environment.iter().copied());
    fixture_output(&mut command).await
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("agent output is UTF-8")
}

fn assert_no_escape(output: &Output) {
    assert!(!output.stdout.contains(&ESCAPE), "stdout holds an ESC byte");
    assert!(!output.stderr.contains(&ESCAPE), "stderr holds an ESC byte");
}

#[proxima::test(timeout = "310s")]
async fn a_green_run_is_three_lines_within_the_byte_budget_with_no_per_test_lines() {
    let output = run("cli_run", &[], &["--output", "agent"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(0), "{text}");
    assert_eq!(text.lines().count(), 3, "header, SUMMARY, EXIT:\n{text}");
    assert!(text.len() <= GREEN_BUDGET_BYTES, "{} bytes:\n{text}", text.len());
    assert!(text.lines().next().expect("a header").contains("output=agent why=flag"), "{text}");
    assert!(text.contains("discovered=3 run=2 filtered=0"), "{text}");
    assert!(text.ends_with("EXIT     0  green\n"), "{text}");
    assert!(!text.lines().any(|line| line.starts_with("PASS ") || line.starts_with("START ")), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn an_assert_failure_block_names_the_user_file_the_message_and_the_exact_rerun() {
    let output = run("cli_run", &[("CARGO_VAL_C52D_FAIL", "1")], &["--output", "agent"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(1), "{text}");
    let block = text.lines().position(|line| line.starts_with("FAIL     cli_run  fixture::")).expect("a failure block");
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[block].ends_with(FAILING_TEST), "{text}");
    assert_eq!(lines[block + 1].replace('\\', "/"), "         PANIC  src/lib.rs:12:13", "{text}");
    assert_eq!(lines[block + 2], "         requested fixture failure", "{text}");
    assert!(text.contains("OUTPUT   bytes="), "{text}");
    let rerun = format!("RERUN    cargo val --manifest-path '{}' --no-fail-fast -E '(binary(=cli_run) & test(={FAILING_TEST}))'", fixture("cli_run").display());
    assert!(text.lines().any(|line| line == rerun), "{text}");
    assert_eq!(text.lines().filter(|line| line.starts_with("FAILED   ")).count(), 1, "{text}");
    assert!(text.ends_with("EXIT     1  failed\n"), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn a_build_error_prints_the_rustc_diagnostic_and_exits_three() {
    let output = run("build_error", &[], &["--output", "agent"]).await;
    let stderr = String::from_utf8(output.stderr.clone()).expect("diagnostics are UTF-8");

    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(stderr.contains("error[E0308]: mismatched types"), "{stderr}");
    assert!(stderr.replace('\\', "/").contains("src/lib.rs:4:5"), "{stderr}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn zero_tests_exit_four_with_a_loud_note_and_counts() {
    let output = run("cli_run", &[], &["--output", "agent", "-E", "test(=nope)"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(4), "{text}");
    assert!(text.contains("NOTE     no tests ran: discovered=3 filtered=3 ignored=0\n"), "{text}");
    assert!(text.contains("run=0 filtered=3"), "{text}");
    assert!(text.ends_with("EXIT     4  no_tests\n"), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn the_claude_code_marker_selects_agent_in_auto_mode_and_says_why() {
    let output = run("cli_run", &[("CLAUDECODE", "1")], &[]).await;
    let text = stdout_of(&output);

    assert!(text.lines().next().expect("a header").contains("output=agent why=env:CLAUDECODE"), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn the_environment_setting_selects_agent_and_names_itself() {
    let output = run("cli_run", &[("CARGO_VAL_OUTPUT", "agent")], &[]).await;
    let text = stdout_of(&output);

    assert!(text.lines().next().expect("a header").contains("output=agent why=env:CARGO_VAL_OUTPUT"), "{text}");
}
