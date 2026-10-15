use std::path::PathBuf;
use std::process::Output;

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const ESCAPE: u8 = 0x1b;
const REFUSED: i32 = 2;
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
        .env("VISUAL", "code")
        .envs(isolation::environment("agent-output"))
        .env("CARGO_VAL_LOG", "debug")
        .envs(environment.iter().copied());
    let output = fixture_output(&mut command).await;
    assert_ne!(output.status.code(), Some(REFUSED), "cargo-val refused the invocation: {:?}\nstderr:\n{}\nstdout:\n{}", output.status, String::from_utf8_lossy(&output.stderr), String::from_utf8_lossy(&output.stdout));
    output
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("agent output is UTF-8")
}

fn assert_no_escape(output: &Output) {
    assert!(!output.stdout.contains(&ESCAPE), "stdout holds an ESC byte");
    assert!(!output.stderr.contains(&ESCAPE), "stderr holds an ESC byte");
}

#[proxima::test(timeout = "310s")]
async fn a_green_run_is_one_summary_line_within_the_byte_budget_with_no_per_test_lines() {
    let output = run("cli_run", &[], &["--output", "agent"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(0), "{text}");
    assert_eq!(text.lines().count(), 1, "SUMMARY alone:\n{text}");
    assert!(text.len() <= GREEN_BUDGET_BYTES, "{} bytes:\n{text}", text.len());
    assert!(text.starts_with("SUMMARY  passed=2 failed=0 ignored=1 not_run=0  wall="), "{text}");
    assert!(text.ends_with(" discovered=3 run=2 filtered=0 binaries=1 exit=0 outcome=green schema=1\n"), "{text}");
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
    assert_eq!(lines.iter().filter(|line| line.trim() == "requested fixture failure").count(), 1, "the message prints once:\n{text}");
    assert!(!text.lines().any(|line| line.starts_with("OUTPUT ") || line.starts_with("END ")), "a parsed panic leaves no raw output block:\n{text}");
    let rollup = format!("FAILED   cli_run {FAILING_TEST} - src/lib.rs:12");
    assert!(lines.iter().any(|line| line.replace('\\', "/") == rollup), "{text}");
    let rerun = format!("RERUN    cargo val --manifest-path '{}' --no-fail-fast -E '(binary(=cli_run) & test(={FAILING_TEST}))'", fixture("cli_run").display());
    assert!(text.lines().any(|line| line == rerun), "{text}");
    assert_eq!(text.lines().filter(|line| line.starts_with("FAILED   ")).count(), 1, "{text}");
    assert_eq!(text.lines().filter(|line| line.starts_with("RERUN ")).count(), 1, "one rerun:\n{text}");
    assert!(lines[lines.len() - 2].contains(" exit=1 outcome=failed schema=1"), "the summary carries the exit code and sits before the rerun:\n{text}");
    assert!(lines[lines.len() - 1].starts_with("RERUN "), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn a_build_error_prints_one_error_block_and_a_rollup_on_stdout_nothing_on_stderr_and_exits_three() {
    let output = run("build_error", &[], &["--output", "agent"]).await;
    let stderr = String::from_utf8(output.stderr.clone()).expect("diagnostics are UTF-8");

    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(!stderr.contains("E0308") && !stderr.contains("could not compile"), "rustc's rendering is not duplicated on stderr:\n{stderr}");
    let report = stdout_of(&output);
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 5, "ERROR block, rollup, EXIT:\n{report}");
    assert_eq!(lines[0].replace('\\', "/"), "ERROR    build_error  src/lib.rs:4:5", "{report}");
    assert_eq!(lines[1], "         error[E0308]: mismatched types", "{report}");
    assert_eq!(lines[2], "         expected `u32`, found `&str`", "{report}");
    assert_eq!(lines[3].replace('\\', "/"), "FAILED   build_error - src/lib.rs:4  error[E0308]: mismatched types", "{report}");
    assert_eq!(lines[4], "EXIT     3  build_failed", "{report}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn zero_tests_exit_four_with_a_loud_note_and_counts() {
    let output = run("cli_run", &[], &["--output", "agent", "-E", "test(=nope)"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(4), "{text}");
    assert!(text.contains("NOTE     no tests ran: discovered=3 filtered=3 ignored=0\n"), "{text}");
    assert!(text.contains("run=0 filtered=3"), "{text}");
    assert!(text.contains(" exit=4 outcome=no_tests schema=1\n"), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn the_claude_code_marker_selects_agent_in_auto_mode() {
    let output = run("cli_run", &[("CLAUDECODE", "1")], &[]).await;
    let text = stdout_of(&output);

    assert!(text.starts_with("SUMMARY  ") && text.contains(" schema=1"), "{text}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn the_environment_setting_selects_agent() {
    let output = run("cli_run", &[("CARGO_VAL_OUTPUT", "agent")], &[]).await;
    let text = stdout_of(&output);

    assert!(text.starts_with("SUMMARY  ") && text.contains(" schema=1"), "{text}");
}

async fn failure_class_line(fixture_name: &str, test: &str, environment: &[(&str, &str)]) -> (Output, String) {
    let filter = format!("test(={test})");
    let output = run(fixture_name, environment, &["--output", "agent", "-E", &filter]).await;
    let report = stdout_of(&output);
    let lines: Vec<&str> = report.lines().collect();
    let block = lines.iter().position(|line| line.starts_with("FAIL     ")).unwrap_or_else(|| panic!("a failure block:\n{report}"));
    let classified = lines[block + 1].to_owned();
    (output, classified)
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn a_segfault_and_an_abort_are_crashes_carrying_the_signal_the_child_died_of() {
    for (test, signal) in [("failing::segfault_crash", 11), ("failing::abort_crash", 6)] {
        for process in ["test", "shard"] {
            let (output, classified) = failure_class_line("failure_class", test, &[("CARGO_VAL_PROCESS", process)]).await;

            assert_eq!(output.status.code(), Some(1), "{test} {process}");
            assert_eq!(classified, format!("         CRASH signal={signal}  location=absent:no_panic_site"), "{test} {process}");
        }
    }
}

#[proxima::test(timeout = "310s")]
async fn a_proxima_test_harness_timeout_is_a_timeout_enforced_by_the_harness() {
    let (output, classified) = failure_class_line("failure_harness", "failing::body_never_completes", &[("PROXIMA_TEST_TIMEOUT_MS", "200")]).await;

    assert_eq!(output.status.code(), Some(1), "{classified}");
    assert!(classified.starts_with("         TIMEOUT enforced_by=harness  "), "{classified}");
    assert!(stdout_of(&output).contains("proxima::test: body did not complete within the timeout"), "the message line survives");
}

#[proxima::test(timeout = "310s")]
async fn a_parsed_panic_prints_its_message_once_and_no_raw_output_block_however_much_the_child_wrote() {
    let output = run("failure_class", &[("CLASSIFY_NOISE_LINES", "200")], &["--output", "agent", "-E", "test(=failing::plain_panic)"]).await;
    let report = stdout_of(&output);

    assert!(!report.lines().any(|line| line.starts_with("OUTPUT ") || line.starts_with("END ")), "{report}");
    assert!(report.len() < 1200, "{} bytes of agent output for a child that wrote 8000 bytes of noise:\n{report}", report.len());
    assert!(!report.contains("flushed segment"), "the child's noise is not echoed:\n{report}");
}

#[proxima::test(timeout = "310s")]
async fn a_panic_message_appears_exactly_once_in_a_failure_block() {
    let output = run("cli_run", &[("CARGO_VAL_C52D_FAIL", "1")], &["--output", "agent"]).await;
    let report = stdout_of(&output);

    let message_lines = report.lines().filter(|line| line.trim() == "requested fixture failure").count();

    assert_eq!(message_lines, 1, "{report}");
    for framing in ["running 1 test", "failures:", "test result:", "thread '"] {
        assert!(!report.lines().any(|line| line.trim_start().starts_with(framing)), "{framing} leaked into {report}");
    }
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn a_crash_that_died_before_printing_a_result_has_a_block_with_no_output_record() {
    let output = run("failure_class", &[], &["--output", "agent", "-E", "test(=failing::segfault_crash)"]).await;
    let report = stdout_of(&output);

    assert!(report.lines().any(|line| line.starts_with("FAIL     ")), "{report}");
    assert!(!report.lines().any(|line| line.starts_with("OUTPUT ")), "{report}");
}

#[proxima::test(timeout = "310s")]
async fn a_failing_assert_prints_an_excerpt_block_of_five_rows_around_the_marker_before_the_rollup() {
    let output = run("cli_run", &[("CARGO_VAL_C52D_FAIL", "1")], &["--output", "agent"]).await;
    let text = stdout_of(&output);

    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|line| line.trim_end() == "EXCERPT").expect("an EXCERPT record");
    let expected = [
        "             10 |         }",
        "             11 |         if std::env::var_os(\"CARGO_VAL_C52D_FAIL\").is_some() {",
        "         >   12 |             panic!(\"requested fixture failure\");",
        "               |             ^^^^^ panicked here",
        "             13 |         }",
    ];
    assert_eq!(lines[start + 1..start + 1 + expected.len()], expected, "{text}");
    assert!(lines[start + 1 + expected.len()].starts_with("FAILED   "), "{text}");
    assert_no_escape(&output);
}
