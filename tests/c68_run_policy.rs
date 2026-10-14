use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::path::Path;

mod common;

use common::fixture_output;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Run {
    fn count(&self, label: &str) -> usize {
        self.stdout
            .lines()
            .filter(|line| line.ends_with(&format!(" {label}")))
            .count()
    }

    fn trailer_number(&self, field: &str) -> Option<usize> {
        let line = self
            .stdout
            .lines()
            .find(|line| line.starts_with("cargo-val: stopped at --max-fail"))?;
        let rest = line.split(field).nth(1)?;
        rest.split(|character: char| !character.is_ascii_digit())
            .find(|piece| !piece.is_empty())?
            .parse()
            .ok()
    }
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn probe_directory(label: &str) -> PathBuf {
    let directory = target_directory()
        .join("c68-probe")
        .join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory)
        .expect("probe directory is creatable under the target directory");
    directory
}

fn fixture_manifest(fixture: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture)
        .join("Cargo.toml")
}

fn fixture_command(
    fixture: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
    probe: Option<&PathBuf>,
) -> Command {
    let mut command = Command::new(CARGO_VAL);
    command
        .env("CARGO_VAL_OUTPUT", "rich")
        .arg("--manifest-path")
        .arg(fixture_manifest(fixture))
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env_remove("CARGO_VAL_FIXTURE_FAILING")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("c68-fixture"));
    if let Some(probe) = probe {
        command.env("CARGO_VAL_PROBE_DIR", probe);
    }
    for (key, value) in environment {
        command.env(key, value);
    }
    command
}

async fn run_fixture(
    fixture: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
    probe: Option<&PathBuf>,
) -> Run {
    fixture_output(&mut fixture_command(fixture, arguments, environment, probe)).await.into()
}

// mkfifo
#[cfg(unix)]
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

// fifo rendezvous with the fixture
#[cfg(unix)]
fn open_fifo_both_ends(path: &Path) -> fs::File {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("fifo opens for read and write without waiting for a peer")
}

// fifo rendezvous with the fixture
#[cfg(unix)]
async fn run_fixture_holding_doc_child(
    fixture: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Run {
    let directory = probe_directory("doc-hold");
    for name in ["ready", "held", "block"] {
        make_fifo(&directory.join(name));
    }
    let _ready = open_fifo_both_ends(&directory.join("ready"));
    let _held = open_fifo_both_ends(&directory.join("held"));
    fixture_output(
        fixture_command(fixture, arguments, environment, None)
            .env("CARGO_VAL_FIXTURE_BLOCK_DIR", &directory),
    ).await
    .into()
}

fn observed_overlaps(probe: &PathBuf) -> Vec<usize> {
    fs::read_dir(probe)
        .expect("probe directory is readable")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("seen-"))
        .map(|entry| {
            let text = fs::read_to_string(entry.path()).expect("observation is readable");
            text.split_whitespace()
                .next()
                .and_then(|count| count.parse().ok())
                .expect("observation starts with a live count")
        })
        .collect()
}
#[proxima::test(timeout = "310s")]
async fn c68_harness_argument_serializes_children_to_one_live_at_a_time() {
    let probe = probe_directory("harness-one");
    let run = run_stop_policy_fixture(&["--", "--test-threads=1"], &[], &probe).await;

    let (observations, maximum) = max_live(&run, &probe);

    assert_eq!(observations, TEST_COUNT);
    assert_eq!(maximum, 1);
    println!(
        "{{\"event\":\"c68_max_live\",\"source\":\"--test-threads=1\",\"children\":{observations},\"max_live\":{maximum}}}"
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_space_separated_harness_argument_is_consumed_not_forwarded() {
    let probe = probe_directory("harness-space");
    let run = run_stop_policy_fixture(&["--", "--test-threads", "1"], &[], &probe).await;

    let (observations, maximum) = max_live(&run, &probe);

    assert_eq!(observations, TEST_COUNT);
    assert_eq!(maximum, 1);
}

#[proxima::test(timeout = "310s")]
async fn c68_rust_test_threads_environment_serializes_children() {
    let probe = probe_directory("env-one");
    let run = run_stop_policy_fixture(&[], &[("RUST_TEST_THREADS", "1")], &probe).await;

    let (observations, maximum) = max_live(&run, &probe);

    assert_eq!(observations, TEST_COUNT);
    assert_eq!(maximum, 1);
    println!(
        "{{\"event\":\"c68_max_live\",\"source\":\"RUST_TEST_THREADS=1\",\"children\":{observations},\"max_live\":{maximum}}}"
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_control_four_threads_overlap_children() {
    let probe = probe_directory("control-four");
    let run = run_stop_policy_fixture(&["--", "--test-threads=4"], &[], &probe).await;

    let (observations, maximum) = max_live(&run, &probe);

    assert_eq!(observations, TEST_COUNT);
    assert!(
        maximum >= 2,
        "four threads never overlapped: max_live={maximum}"
    );
    println!(
        "{{\"event\":\"c68_max_live\",\"source\":\"--test-threads=4 control\",\"children\":{observations},\"max_live\":{maximum}}}"
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_harness_argument_beats_environment() {
    let probe = probe_directory("harness-beats-env");
    let run = run_stop_policy_fixture(
        &["--", "--test-threads=4"],
        &[("RUST_TEST_THREADS", "1")],
        &probe,
    ).await;

    let (_, maximum) = max_live(&run, &probe);

    assert!(
        maximum >= 2,
        "environment value won over the harness argument: max_live={maximum}"
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_environment_beats_configured_jobs() {
    let probe = probe_directory("env-beats-config");
    let run = run_stop_policy_fixture(
        &[],
        &[("RUST_TEST_THREADS", "1"), ("CARGO_VAL_JOBS", "4")],
        &probe,
    ).await;

    let (_, maximum) = max_live(&run, &probe);

    assert_eq!(maximum, 1);
}

type InvalidCase<'case> = (
    &'case [&'case str],
    &'case [(&'case str, &'case str)],
    &'case str,
);

#[proxima::test(timeout = "310s")]
async fn c68_invalid_values_name_the_supplying_source_and_exit_2() {
    let cases: [InvalidCase<'_>; 5] = [
        (
            &["--", "--test-threads=0"],
            &[],
            "--test-threads must be at least 1",
        ),
        (
            &["--", "--test-threads=65"],
            &[],
            "--test-threads exceeds the live child capacity",
        ),
        (
            &["--", "--test-threads=x"],
            &[],
            "--test-threads is not a positive integer",
        ),
        (
            &[],
            &[("RUST_TEST_THREADS", "0")],
            "RUST_TEST_THREADS must be at least 1",
        ),
        (
            &[],
            &[("CARGO_VAL_JOBS", "65")],
            "CARGO_VAL_JOBS exceeds the live child capacity",
        ),
    ];
    for (arguments, environment, message) in cases {
        let probe = probe_directory("invalid");
        let run = run_stop_policy_fixture(arguments, environment, &probe).await;

        assert_eq!(run.status, Some(2), "{arguments:?} {environment:?}");
        assert!(
            run.stderr.contains(message),
            "{} lacks {message}",
            run.stderr
        );
        assert_eq!(observed_overlaps(&probe), Vec::<usize>::new());
    }
}

#[proxima::test(timeout = "310s")]
async fn c68_other_libtest_flags_are_still_rejected() {
    let probe = probe_directory("rejected");
    let run = run_stop_policy_fixture(&["--", "--nocapture"], &[], &probe).await;

    assert_eq!(run.status, Some(2));
    assert!(run.stderr.contains("invalid arguments"));
}

const TEST_COUNT: usize = 17;
const STOP_POLICY: &str = "stop_policy";

async fn run_stop_policy_fixture(
    arguments: &[&str],
    environment: &[(&str, &str)],
    probe: &PathBuf,
) -> Run {
    run_fixture(STOP_POLICY, &per_test(arguments), environment, Some(probe)).await
}

fn per_test<'a>(arguments: &[&'a str]) -> Vec<&'a str> {
    ["--process", "test"]
        .into_iter()
        .chain(arguments.iter().copied())
        .collect()
}

fn max_live(run: &Run, probe: &PathBuf) -> (usize, usize) {
    assert_eq!(
        run.status,
        Some(0),
        "stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    let overlaps = observed_overlaps(probe);
    (overlaps.len(), overlaps.into_iter().max().unwrap_or(0))
}

async fn run_stopping(arguments: &[&str], environment: &[(&str, &str)]) -> Run {
    run_fixture(STOP_POLICY, &per_test(arguments), environment, None).await
}

const THREE_FAILING: (&str, &str) = ("CARGO_VAL_FIXTURE_FAILING", "case_01,case_02,case_03");
const ONE_FAILING: (&str, &str) = ("CARGO_VAL_FIXTURE_FAILING", "case_01");

#[proxima::test(timeout = "310s")]
async fn c68_default_fails_fast_launching_nothing_after_the_first_failure() {
    let run = run_stopping(&["--", "--test-threads=1"], &[ONE_FAILING]).await;

    assert_eq!(run.status, Some(1), "{}{}", run.stdout, run.stderr);
    assert_eq!(run.count("FAIL"), 1);
    assert_eq!(run.count("PASS"), 0);
    assert_eq!(run.trailer_number("launched after trip "), Some(0));
    assert_eq!(run.trailer_number("in flight at trip "), Some(0));
    assert_eq!(
        run.trailer_number("listed tests not run "),
        Some(TEST_COUNT - 1)
    );
    println!(
        "{{\"event\":\"c68_stop\",\"mode\":\"default\",\"fail\":1,\"pass\":0,\"launched_after_trip\":0,\"not_run\":{}}}",
        TEST_COUNT - 1
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_no_fail_fast_runs_every_selected_test() {
    let run = run_stopping(
        &["--no-fail-fast", "--", "--test-threads=1"],
        &[ONE_FAILING],
    ).await;

    assert_eq!(run.status, Some(1), "{}{}", run.stdout, run.stderr);
    assert_eq!(run.count("PASS") + run.count("FAIL"), TEST_COUNT);
    assert_eq!(run.count("FAIL"), 1);
    assert_eq!(run.trailer_number("launched after trip "), None);
}

#[proxima::test(timeout = "310s")]
async fn c68_max_fail_two_stops_after_the_second_of_three_failures() {
    let run = run_stopping(
        &["--max-fail", "2", "--", "--test-threads=1"],
        &[THREE_FAILING],
    ).await;

    assert_eq!(run.status, Some(1), "{}{}", run.stdout, run.stderr);
    assert_eq!(run.count("FAIL"), 2);
    assert_eq!(run.count("PASS"), 0);
    assert_eq!(run.trailer_number("launched after trip "), Some(0));
    assert_eq!(
        run.trailer_number("listed tests not run "),
        Some(TEST_COUNT - 2)
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_in_flight_children_finish_and_every_listed_test_is_accounted_for() {
    let run = run_stopping(&["--", "--test-threads=4"], &[THREE_FAILING]).await;

    assert_eq!(run.status, Some(1), "{}{}", run.stdout, run.stderr);
    let finished = run.count("PASS") + run.count("FAIL");
    let not_run = run
        .trailer_number("listed tests not run ")
        .expect("a tripped run prints its trailer");
    assert_eq!(finished + not_run, TEST_COUNT, "{}", run.stdout);
    assert_eq!(run.trailer_number("launched after trip "), Some(0));
    assert!(run.count("FAIL") >= 1);
}

#[proxima::test(timeout = "310s")]
async fn c68_stop_policy_sources_follow_precedence() {
    let environment_max_fail = run_stopping(
        &["--", "--test-threads=1"],
        &[THREE_FAILING, ("CARGO_VAL_MAX_FAIL", "2")],
    ).await;
    let cli_beats_environment = run_stopping(
        &["--no-fail-fast", "--", "--test-threads=1"],
        &[THREE_FAILING, ("CARGO_VAL_MAX_FAIL", "1")],
    ).await;
    let environment_disables = run_stopping(
        &["--", "--test-threads=1"],
        &[THREE_FAILING, ("CARGO_VAL_FAIL_FAST", "false")],
    ).await;
    let explicit_flag = run_stopping(
        &["--fail-fast", "--", "--test-threads=1"],
        &[THREE_FAILING, ("CARGO_VAL_FAIL_FAST", "false")],
    ).await;

    assert_eq!(environment_max_fail.count("FAIL"), 2);
    assert_eq!(cli_beats_environment.count("FAIL"), 3);
    assert_eq!(cli_beats_environment.count("PASS"), TEST_COUNT - 3);
    assert_eq!(environment_disables.count("FAIL"), 3);
    assert_eq!(explicit_flag.count("FAIL"), 1);
}

#[proxima::test(timeout = "310s")]
async fn c68_invalid_max_fail_values_name_the_supplying_source() {
    let zero = run_stopping(&["--max-fail", "0"], &[]).await;
    let joined = run_stopping(&["--max-fail=x"], &[]).await;
    let dangling = run_stopping(&["--max-fail"], &[]).await;
    let configured = run_stopping(&[], &[("CARGO_VAL_MAX_FAIL", "0")]).await;

    for run in [&zero, &joined, &dangling] {
        assert_eq!(run.status, Some(2));
        assert!(
            run.stderr
                .contains("--max-fail is not an integer of at least 1")
        );
    }
    assert_eq!(configured.status, Some(2));
    assert!(configured.stderr.contains("CARGO_VAL_MAX_FAIL"));
}

fn passing_count(run: &Run) -> usize {
    run.count("PASS")
}

#[proxima::test(timeout = "310s")]
async fn c68_short_feature_option_builds_with_the_feature_and_runs_the_full_set() {
    let without = run_stopping(&["--no-fail-fast"], &[]).await;
    let short = run_stopping(&["-F", "extra", "--no-fail-fast"], &[]).await;
    let long = run_stopping(&["--features=extra", "--no-fail-fast"], &[]).await;

    assert_eq!(
        passing_count(&without),
        TEST_COUNT,
        "{}{}",
        without.stdout,
        without.stderr
    );
    assert_eq!(
        passing_count(&short),
        TEST_COUNT + 3,
        "{}{}",
        short.stdout,
        short.stderr
    );
    assert_eq!(
        passing_count(&long),
        TEST_COUNT + 3,
        "{}{}",
        long.stdout,
        long.stderr
    );
    assert_eq!(short.status, Some(0));
}

#[proxima::test(timeout = "310s")]
async fn c68_short_manifest_path_is_forwarded_and_not_taken_as_the_filter() {
    let manifest = fixture_manifest(STOP_POLICY);
    let manifest = manifest.to_str().expect("fixture path is utf-8");
    let mut command = Command::new(CARGO_VAL);
    command
        .env("CARGO_VAL_OUTPUT", "rich")
        .args(["-m", manifest, "--no-fail-fast"])
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("c68-fixture"));
    let output = fixture_output(&mut command).await;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.ends_with(" PASS"))
            .count(),
        TEST_COUNT
    );
}

const ZERO_RUN_PROBES: [&[&str]; 4] = [
    &["--", "--ignored"],
    &["--lib"],
    &["--examples"],
    &["--benches"],
];

#[proxima::test(timeout = "310s")]
async fn c68_zero_test_runs_exit_four_by_default_and_zero_with_no_tests_pass() {
    for arguments in ZERO_RUN_PROBES {
        let default = run_fixture("workspace", arguments, &[], None).await;
        let mut passing = vec!["--no-tests=pass"];
        passing.extend_from_slice(arguments);
        let pass = run_fixture("workspace", &passing, &[], None).await;
        let mut warning = vec!["--no-tests", "warn"];
        warning.extend_from_slice(arguments);
        let warn = run_fixture("workspace", &warning, &[], None).await;

        let context = format!("{arguments:?}: {}{}", default.stdout, default.stderr);
        assert_eq!(
            default.count("PASS") + default.count("FAIL"),
            0,
            "{context}"
        );
        assert_eq!(default.status, Some(4), "{context}");
        assert!(default.stderr.contains("no tests to run"), "{context}");
        assert_eq!(pass.status, Some(0), "{arguments:?}: {}", pass.stderr);
        assert!(!pass.stderr.contains("no tests to run"));
        assert_eq!(warn.status, Some(0), "{arguments:?}: {}", warn.stderr);
        assert!(warn.stderr.contains("no tests to run"));
        println!(
            "{{\"event\":\"c68_no_tests\",\"selection\":\"{arguments:?}\",\"default_exit\":4,\"pass_exit\":0,\"warn_exit\":0}}"
        );
    }
}

#[proxima::test(timeout = "310s")]
async fn c68_no_tests_setting_applies_and_the_command_line_beats_it() {
    let configured = run_fixture(
        "workspace",
        &["--lib"],
        &[("CARGO_VAL_NO_TESTS", "pass")],
        None,
    ).await;
    let overridden = run_fixture(
        "workspace",
        &["--no-tests=fail", "--lib"],
        &[("CARGO_VAL_NO_TESTS", "pass")],
        None,
    ).await;
    let invalid = run_fixture("workspace", &["--no-tests=ignore", "--lib"], &[], None).await;

    assert_eq!(configured.status, Some(0));
    assert_eq!(overridden.status, Some(4));
    assert_eq!(invalid.status, Some(2));
    assert!(
        invalid
            .stderr
            .contains("--no-tests must be fail, warn, or pass")
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_a_selection_that_matches_tests_is_not_reported_as_empty() {
    let run = run_fixture(
        "workspace",
        &[
            "--exact",
            "fixture::passes",
            "--test",
            "integration_fixture",
        ],
        &[],
        None,
    ).await;

    assert_eq!(run.count("PASS"), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.status, Some(0));
    assert!(!run.stderr.contains("no tests to run"));
}

const DOC_FIXTURE: &str = "doc_tests";

struct DocReference {
    passed: usize,
    failed: usize,
    ignored: usize,
}

async fn cargo_test_doc_reference() -> DocReference {
    let output = fixture_output(
        Command::new("cargo")
            .args(["test", "--doc", "--manifest-path"])
            .arg(fixture_manifest(DOC_FIXTURE))
            .env("CARGO_TARGET_DIR", target_directory().join("c68-reference")),
    )
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let summary = stdout
        .lines()
        .find(|line| line.starts_with("test result:"))
        .expect("cargo test --doc prints a summary")
        .to_owned();
    let number_before = |word: &str| -> usize {
        let head = summary.split(word).next().expect("summary has the word");
        head.split(|character: char| !character.is_ascii_digit())
            .rfind(|piece| !piece.is_empty())
            .and_then(|piece| piece.parse().ok())
            .expect("a count precedes the word")
    };
    DocReference {
        passed: number_before(" passed"),
        failed: number_before(" failed"),
        ignored: number_before(" ignored"),
    }
}

fn doc_lines(run: &Run, label: &str) -> usize {
    run.stdout
        .lines()
        .filter(|line| line.replace('\\', "/").starts_with("src/lib.rs - ") && line.ends_with(&format!(" {label}")))
        .count()
}

#[proxima::test(timeout = "310s")]
async fn c68_doc_test_counts_equal_cargo_test_doc_for_the_same_fixture() {
    let reference = cargo_test_doc_reference().await;
    let only_docs = run_fixture(DOC_FIXTURE, &["--doc"], &[], None).await;

    assert_eq!(
        doc_lines(&only_docs, "PASS"),
        reference.passed,
        "{}{}",
        only_docs.stdout,
        only_docs.stderr
    );
    assert_eq!(doc_lines(&only_docs, "FAIL"), reference.failed);
    assert_eq!(doc_lines(&only_docs, "IGNORED"), reference.ignored);
    assert_eq!(
        only_docs.count("PASS") + only_docs.count("FAIL") + only_docs.count("IGNORED"),
        3
    );
    assert_eq!(only_docs.status, Some(1));
    println!(
        "{{\"event\":\"c68_doc_parity\",\"cargo_test_doc\":[{},{},{}],\"cargo_val_doc\":[{},{},{}]}}",
        reference.passed,
        reference.failed,
        reference.ignored,
        doc_lines(&only_docs, "PASS"),
        doc_lines(&only_docs, "FAIL"),
        doc_lines(&only_docs, "IGNORED")
    );
}

#[proxima::test(timeout = "310s")]
async fn c68_failing_doc_test_output_is_shown_like_other_failures() {
    let run = run_fixture(DOC_FIXTURE, &["--doc"], &[], None).await;

    let stderr = run.stderr.replace('\\', "/");
    assert!(
        stderr.contains("---- src/lib.rs - subtract (line 12) stdout ----"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("left: 2"), "{}", run.stderr);
    assert!(run.stderr.contains("right: 3"));
}

#[proxima::test(timeout = "310s")]
async fn c68_default_selection_runs_unit_tests_and_doc_tests_together_and_counts_both() {
    let run = run_fixture(DOC_FIXTURE, &["--no-fail-fast"], &[], None).await;

    assert_eq!(run.count("PASS"), 3, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.count("FAIL"), 1);
    assert_eq!(run.count("IGNORED"), 1);
    assert_eq!(doc_lines(&run, "PASS"), 1);
    assert_eq!(run.status, Some(1));
}

#[proxima::test(timeout = "310s")]
async fn c68_default_fail_fast_still_reports_the_doc_failure_and_exits_one() {
    let run = run_fixture(DOC_FIXTURE, &[], &[], None).await;

    assert_eq!(run.status, Some(1), "{}{}", run.stdout, run.stderr);
    assert_eq!(doc_lines(&run, "FAIL"), 1);
}

#[proxima::test(timeout = "310s")]
async fn c68_target_selection_flags_exclude_doc_tests_as_cargo_does() {
    let library_only = run_fixture(DOC_FIXTURE, &["--lib"], &[], None).await;

    assert_eq!(
        library_only.count("PASS"),
        2,
        "{}{}",
        library_only.stdout,
        library_only.stderr
    );
    assert_eq!(
        doc_lines(&library_only, "PASS") + doc_lines(&library_only, "FAIL"),
        0
    );
    assert!(!library_only.stdout.contains("doc tests"));
    assert_eq!(library_only.status, Some(0));
}

#[proxima::test(timeout = "310s")]
async fn c68_a_library_with_no_doctests_is_reported_not_passed_silently() {
    let default_policy = run_fixture("workspace", &["--doc"], &[], None).await;
    let passing = run_fixture("workspace", &["--doc", "--no-tests=pass"], &[], None).await;

    assert!(
        default_policy
            .stdout
            .contains("cargo-val: doc tests: running 0 tests"),
        "{}{}",
        default_policy.stdout,
        default_policy.stderr
    );
    assert_eq!(default_policy.status, Some(4));
    assert!(passing.stdout.contains("doc tests: running 0 tests"));
    assert_eq!(passing.status, Some(0));
}

#[proxima::test(timeout = "310s")]
async fn c68_package_without_a_library_skips_doc_tests_and_says_so() {
    let run = run_fixture(STOP_POLICY, &[], &[], None).await;

    assert_eq!(
        run.count("PASS"),
        TEST_COUNT,
        "{}{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout
            .contains("cargo-val: doc tests: no library target to test")
    );
    assert!(!run.stderr.contains("no library targets found"));
    assert_eq!(run.status, Some(0));
}

#[proxima::test(timeout = "310s")]
async fn c68_explicit_doc_selection_on_a_package_without_a_library_fails_loudly() {
    let run = run_fixture(STOP_POLICY, &["--doc"], &[], None).await;

    assert_eq!(run.status, Some(2), "{}{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("no library targets found"));
}

// holds the doc child on a fifo
#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn c68_a_trip_before_doc_tests_execute_interrupts_the_doc_child_and_says_so() {
    let run = run_fixture_holding_doc_child(
        DOC_FIXTURE,
        &["--", "--test-threads=1"],
        &[("CARGO_VAL_FIXTURE_FAILING", "unit_adds")],
    ).await;

    assert_eq!(run.status, Some(1), "{}{}", run.stdout, run.stderr);
    assert!(
        run.stdout
            .contains("doc tests at trip: interrupted before executing"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("cargo-val: doc tests: stopped before executing")
    );
    assert_eq!(
        doc_lines(&run, "PASS") + doc_lines(&run, "FAIL") + doc_lines(&run, "IGNORED"),
        0
    );
    assert_eq!(run.count("FAIL"), 1);
}

#[proxima::test(timeout = "310s")]
async fn c97_one_live_child_drains_every_listed_test_and_ends_the_listing() {
    let run = run_stopping(&["--no-fail-fast", "--", "--test-threads=1"], &[]).await;

    assert_eq!(run.status, Some(0), "{}{}", run.stdout, run.stderr);
    assert_eq!(run.count("PASS"), TEST_COUNT, "{}", run.stdout);
    assert!(
        run.stdout.contains(&format!(
            "total: discovered {TEST_COUNT}, passed {TEST_COUNT}, failed 0, ignored 0"
        )),
        "{}",
        run.stdout
    );
}

#[proxima::test(timeout = "310s")]
async fn c97_one_live_child_still_drains_the_listing_and_the_doc_group_together() {
    let run = run_fixture(
        DOC_FIXTURE,
        &["--no-fail-fast", "--", "--test-threads=1"],
        &[],
        None,
    ).await;

    assert_eq!(run.count("PASS"), 3, "{}{}", run.stdout, run.stderr);
    assert_eq!(run.count("FAIL"), 1);
    assert_eq!(run.count("IGNORED"), 1);
    assert_eq!(doc_lines(&run, "PASS"), 1);
    assert_eq!(run.status, Some(1));
}
