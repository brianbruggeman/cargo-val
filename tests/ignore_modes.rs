#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/plain.rs"]
mod plain;

use cargo_val::render::Outcome;
use common::output_within;
use fixture::fixture_output;

#[cfg(unix)]
#[path = "common/supervise.rs"]
mod supervise;


// the help screen is printed by the already-built binary with no build and no wait; 30 s stays inside the 60 s body timeout

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn has(&self, test: &str, outcome: Outcome) -> bool {
        plain::names(&self.stdout, outcome).iter().any(|name| name == test)
    }

    fn lines_with(&self, prefix: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with(prefix))
            .collect()
    }

    fn total_without_wall(&self) -> String {
        let summary = self.lines_with("SUMMARY  ");
        assert_eq!(summary.len(), 1, "{}{}", self.stdout, self.stderr);
        let field = |key: &str| -> usize {
            let prefix = format!("{key}=");
            summary[0].split_whitespace().find_map(|word| word.strip_prefix(prefix.as_str())).map_or(0, |value| value.parse().unwrap_or_else(|_| panic!("{key} is a number in {}", summary[0])))
        };
        let wall = summary[0].split("wall=").nth(1).and_then(|rest| rest.split_whitespace().next()).expect("summary carries wall time");
        assert!(wall.ends_with('s') && wall.contains('.'), "wall time shape: {wall}");
        let (passed, failed, ignored, filtered) = (field("passed"), field("failed"), field("ignored"), field("filtered"));
        let discovered = passed + failed + ignored + filtered + field("not_run");
        format!("total: discovered {discovered}, passed {passed}, failed {failed}, ignored {ignored}, filtered {filtered}")
    }
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn manifest(fixture: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture)
        .join("Cargo.toml")
}

async fn run(manifest_path: &std::path::Path, arguments: &[&str], environment: &[(&str, &str)]) -> Run {
    let mut command = crate::binary::command();
    command
        .env("CARGO_VAL_OUTPUT", "plain")
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_IGNORED")
        .env_remove("CARGO_VAL_FILTERSET")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("ignore-modes-fixture"));
    for (key, value) in environment {
        command.env(key, value);
    }
    let output = fixture_output(&mut command).await;
    Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

const MODES: [&str; 2] = ["test", "shard"];

async fn fixture(mode: &str, arguments: &[&str]) -> Run {
    with_environment(mode, arguments, &[]).await
}

async fn with_environment(mode: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Run {
    let mut full = vec!["--lib", "--no-fail-fast"];
    full.extend_from_slice(arguments);
    let mut with_mode = vec![("CARGO_VAL_PROCESS", mode)];
    with_mode.extend_from_slice(environment);
    run(&manifest("ignore_modes"), &full, &with_mode).await
}

#[cfg(unix)]
const TEST_NAME: &str = "the_bin_target_exists_when_the_test_starts";

#[proxima::test(timeout = "310s")]
async fn default_reports_the_ignored_test_as_ignored_and_never_launches_it() {
    for mode in MODES {
        default_reports_the_ignored_test_as_ignored_and_never_launches_it_in(mode).await;
    }
}

async fn default_reports_the_ignored_test_as_ignored_and_never_launches_it_in(mode: &str) {
    let result = fixture(mode, &[]).await;
    let context = format!("{mode}: {}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(1), "{context}");
    assert!(result.has("fixture::passes", Outcome::Passed), "{context}");
    assert!(result.has("fixture::fails", Outcome::Failed), "{context}");
    assert!(
        result.has("fixture::ignored_with_reason", Outcome::Ignored),
        "{context}"
    );
    assert!(!result.has("fixture::ignored_with_reason", Outcome::Passed), "{context}");
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 1, failed 1, ignored 1, filtered 0"
    );
    assert!(!result.stdout.contains('\u{1b}'), "NO_COLOR is respected");
}

#[proxima::test(timeout = "310s")]
async fn ignored_flag_runs_only_the_ignored_test_and_filters_the_rest() {
    for mode in MODES {
        ignored_flag_runs_only_the_ignored_test_and_filters_the_rest_in(mode).await;
    }
}

async fn ignored_flag_runs_only_the_ignored_test_and_filters_the_rest_in(mode: &str) {
    let result = fixture(mode, &["--", "--ignored"]).await;
    let context = format!("{mode}: {}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(0), "{context}");
    assert!(
        result.has("fixture::ignored_with_reason", Outcome::Passed),
        "{context}"
    );
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 1, failed 0, ignored 0, filtered 2"
    );
}

#[proxima::test(timeout = "310s")]
async fn include_ignored_flag_runs_every_test() {
    for mode in MODES {
        include_ignored_flag_runs_every_test_in(mode).await;
    }
}

async fn include_ignored_flag_runs_every_test_in(mode: &str) {
    let result = fixture(mode, &["--", "--include-ignored"]).await;
    let context = format!("{mode}: {}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(1), "{context}");
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 2, failed 1, ignored 0, filtered 0"
    );
}

#[proxima::test(timeout = "310s")]
async fn ignore_mode_setting_equals_the_libtest_flag() {
    for mode in MODES {
        ignore_mode_setting_equals_the_libtest_flag_in(mode).await;
    }
}

async fn ignore_mode_setting_equals_the_libtest_flag_in(mode: &str) {
    let flag = fixture(mode, &["--", "--include-ignored"]).await;
    let setting = with_environment(mode, &[], &[("CARGO_VAL_IGNORED", "include")]).await;

    assert_eq!(setting.total_without_wall(), flag.total_without_wall());
    assert_eq!(setting.status, flag.status);
}

#[proxima::test(timeout = "310s")]
async fn both_ignore_flags_together_are_refused_as_in_libtest() {
    for mode in MODES {
        both_ignore_flags_together_are_refused_as_in_libtest_in(mode).await;
    }
}

async fn both_ignore_flags_together_are_refused_as_in_libtest_in(mode: &str) {
    let result = fixture(mode, &["--", "--ignored", "--include-ignored"]).await;

    assert_eq!(result.status, Some(2), "{}{}", result.stdout, result.stderr);
    assert!(result.stderr.contains("invalid arguments"), "{}", result.stderr);
}

#[proxima::test(timeout = "310s")]
async fn only_ignored_selected_and_nothing_executed_is_red_by_the_no_tests_policy() {
    for mode in MODES {
        only_ignored_selected_and_nothing_executed_is_red_by_the_no_tests_policy_in(mode).await;
    }
}

async fn only_ignored_selected_and_nothing_executed_is_red_by_the_no_tests_policy_in(mode: &str) {
    let result = fixture(mode, &["-E", "test(ignored_with_reason)"]).await;
    let context = format!("{mode}: {}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(4), "{context}");
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 0, failed 0, ignored 1, filtered 2"
    );
    assert!(result.stderr.contains("no tests to run"), "{context}");
}

#[proxima::test(timeout = "310s")]
async fn filterset_excludes_before_launch_and_counts_filtered() {
    for mode in MODES {
        filterset_excludes_before_launch_and_counts_filtered_in(mode).await;
    }
}

async fn filterset_excludes_before_launch_and_counts_filtered_in(mode: &str) {
    let cases: [(&str, i32, &str); 5] = [
        ("test(=fixture::passes)", 0, "total: discovered 3, passed 1, failed 0, ignored 0, filtered 2"),
        ("not test(fails)", 0, "total: discovered 3, passed 1, failed 0, ignored 1, filtered 1"),
        ("test(#fixture::[pf]*) - test(fails)", 0, "total: discovered 3, passed 1, failed 0, ignored 0, filtered 2"),
        ("package(ignore_*) & binary(#ignore_modes) & test(#*::fails)", 1, "total: discovered 3, passed 0, failed 1, ignored 0, filtered 2"),
        ("package(other) | none()", 4, "total: discovered 3, passed 0, failed 0, ignored 0, filtered 3"),
    ];
    for (expression, status, total) in cases {
        let result = fixture(mode, &["-E", expression]).await;
        let context = format!("{mode} {expression}: {}{}", result.stdout, result.stderr);
        assert_eq!(result.status, Some(status), "{context}");
        assert_eq!(result.total_without_wall(), total, "{context}");
    }
}

#[proxima::test(timeout = "310s")]
async fn filterset_setting_equals_the_flag_and_the_flag_wins() {
    for mode in MODES {
        filterset_setting_equals_the_flag_and_the_flag_wins_in(mode).await;
    }
}

async fn filterset_setting_equals_the_flag_and_the_flag_wins_in(mode: &str) {
    let flag = fixture(mode, &["--filterset", "test(passes)"]).await;
    let setting = with_environment(mode, &[], &[("CARGO_VAL_FILTERSET", "test(passes)")]).await;
    let overridden = with_environment(mode, &["-E", "test(passes)"], &[("CARGO_VAL_FILTERSET", "none()")]).await;

    assert_eq!(setting.total_without_wall(), flag.total_without_wall());
    assert_eq!(overridden.total_without_wall(), flag.total_without_wall());
}

#[proxima::test(timeout = "310s")]
async fn malformed_filterset_fails_with_a_caret_before_any_build() {
    let missing = PathBuf::from("/nonexistent/cargo-val-fixture/Cargo.toml");
    let result = run(&missing, &["--lib", "-E", "test(a) & & test(b)"], &[]).await;

    assert_eq!(result.status, Some(2), "{}", result.stderr);
    assert_eq!(
        result.stderr,
        "cargo-val: invalid filterset: expected a predicate, `(`, `not` or `!`\n  test(a) & & test(b)\n            ^\n"
    );
    assert_eq!(result.stdout, "");
}

#[proxima::test(timeout = "310s")]
async fn an_exact_launch_that_ran_nothing_is_an_error_naming_the_test() {
    for mode in MODES {
        an_exact_launch_that_ran_nothing_is_an_error_naming_the_test_in(mode).await;
    }
}

async fn an_exact_launch_that_ran_nothing_is_an_error_naming_the_test_in(mode: &str) {
    let result = run(&manifest("ghost_harness"), &["--test", "ghost"], &[("CARGO_VAL_PROCESS", mode)]).await;

    assert_eq!(result.status, Some(2), "{}{}", result.stdout, result.stderr);
    assert!(
        result.stderr.contains("test ghost::vanishes in ghost_harness::ghost exited 0 but libtest reported 0 passed, 0 failed, 0 ignored"),
        "{}",
        result.stderr
    );
}

#[cfg(unix)]
mod bin_gate_hold {
    use std::ffi::CString;
    use std::fs::OpenOptions;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    use futures::future::join3;
    use proxima_primitives::pipe::Pipe;

    use super::{Run, TEST_NAME, manifest, target_directory};
    use crate::common::{ChildStream, drain, drain_until};
    use crate::fixture::fixture_deadline;
    use crate::supervise::supervised;

    const FIFO_HELD_UNITS: usize = 2;
    // the bin and its unit-test target both block on the fifo, each holding a job slot, so the integration test unit needs one more or it never links and the park line never comes; an ambient build.jobs, CARGO_BUILD_JOBS or jobserver of FIFO_HELD_UNITS or fewer would stall the run until its deadline
    const FIXTURE_BUILD_JOBS: usize = FIFO_HELD_UNITS + 1;
    const GATE_PARKED_LINE: &str = "cargo-val: debug: gate park why=bin-gate kind=test target=uses_bin";
    const GATE_DROPPED_LINE: &str = "cargo-val: debug: gate drop reason=build-failed target=uses_bin";

    /// What proves the bin-less window was observed, so the bin build may be released.
    #[derive(Clone, Copy)]
    pub enum Release {
        /// gate off: the test settled with no bin; the wait ends on any settle of it, so a mismatch fails at once with what stdout printed.
        AfterTestFails,
        /// gate on: cargo-val is holding the linked test executable for the bin and says so on stderr.
        AfterTestArtifactLinks,
    }

    // releases on any verdict the plain grammar prints for the test, so the wait never runs to its deadline when the verdict is not a failure
    fn test_settled(text: &str) -> bool {
        crate::plain::settled(text).iter().any(|(_, name, _)| name == TEST_NAME)
    }

    fn make_fifo(path: &Path) {
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul");
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
    }

    // rustc may open the fifo more than once (sccache runs a dep-info pass first), so one write-open would strand a later reader; the loop ends when the target directory is removed
    // a fifo write-open blocks in the kernel until a reader arrives and has no readiness event, so it cannot run on the prime worker that drives the child
    fn release_bin_build(fifo: &Path) {
        let fifo = fifo.to_path_buf();
        thread::spawn(move || {
            while fifo.exists() {
                drop(OpenOptions::new().write(true).open(&fifo));
            }
        });
    }

    // reads `stream` to EOF, releasing the bin build the moment `proves_window` accepts what was read when this stream is the one that proves the window
    async fn watched(stream: &ChildStream, proves_window: Option<fn(&str) -> bool>, fifo: &Path) -> Vec<u8> {
        let Some(proves_window) = proves_window else {
            return drain(stream).await.0;
        };
        let (seen, _) = drain_until(stream, |bytes| proves_window(&String::from_utf8_lossy(bytes))).await;
        release_bin_build(fifo);
        [seen, drain(stream).await.0].concat()
    }

    // the fixture has no dependencies, so a private CARGO_HOME costs nothing and takes it off the package-cache lock every other cargo on the box takes
    // RUSTC_WRAPPER closes the cargo pipes a rustc inherits when cargo forks it between pipe() and CLOEXEC; left open, the rustc held on the fifo keeps a sibling's EOF from arriving and cargo never sees the bin artifact
    pub fn command(target: &Path, fifo: &Path, wrapper: &Path) -> Command {
        let mut command = crate::binary::command();
        command
            .env("CARGO_VAL_OUTPUT", "plain")
            .arg("--manifest-path")
            .arg(manifest("bin_gate"))
            .env_remove("CARGO_VAL_BIN_GATE")
            .env("NO_COLOR", "1")
            .env("CARGO_VAL_LOG", "debug")
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_HOME", target.join("cargo-home"))
            .env("CARGO_VAL_TARGET_DIR", "shared")
            .env("CARGO_VAL_BIN_GATE_HOLD", fifo)
            .env("CARGO_BUILD_JOBS", FIXTURE_BUILD_JOBS.to_string())
            .env("RUSTC_WRAPPER", wrapper)
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("MAKEFLAGS")
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("MFLAGS");
        command
    }

    pub async fn cold_run(label: &str, environment: &[(&str, &str)], release: Release) -> Run {
        cold_run_within(label, environment, release, fixture_deadline()).await
    }

    pub async fn cold_run_within(label: &str, environment: &[(&str, &str)], release: Release, deadline: Duration) -> Run {
        let target = target_directory().join(format!("bin-gate-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir_all(&target).expect("target directory is creatable");
        let fifo = target.join("bin-hold");
        make_fifo(&fifo);
        let wrapper = crate::fd_scrub::built();
        let mut command = command(&target, &fifo, &wrapper);
        for (key, value) in environment {
            command.env(key, value);
        }
        let (status, stdout, stderr) = supervised(&mut command, deadline, async |_, stdout, stderr, exit| {
            let settled: fn(&str) -> bool = test_settled;
            let parked: fn(&str) -> bool = |text| text.contains(GATE_PARKED_LINE) || text.contains(GATE_DROPPED_LINE);
            let (stdout_proof, stderr_proof) = match release {
                Release::AfterTestFails => (Some(settled), None),
                Release::AfterTestArtifactLinks => (None, Some(parked)),
            };
            let (stdout, stderr, exit) = join3(watched(stdout, stdout_proof, &fifo), watched(stderr, stderr_proof, &fifo), exit.call(())).await;
            let status = exit.expect("cargo-val exit is observed once").expect("cargo-val is waitable");
            (status, String::from_utf8_lossy(&stdout).into_owned(), String::from_utf8_lossy(&stderr).into_owned())
        })
        .await;
        let run = Run { status: status.code(), stdout, stderr };
        let _ = std::fs::remove_dir_all(&target);
        run
    }
}

#[cfg(unix)]
#[test]
fn the_bin_gate_fixture_builds_with_a_private_cargo_home_and_target_dir() {
    let target = Path::new("/scratch/bin-gate-on-4242");
    let command = bin_gate_hold::command(target, &target.join("bin-hold"), Path::new("/scratch/fd-scrub-rustc"));

    let variable = |name: &str| command.get_envs().find(|(key, _)| *key == name).and_then(|(_, value)| value).map(Path::new);

    assert_eq!(variable("CARGO_TARGET_DIR"), Some(target));
    assert_eq!(variable("CARGO_HOME"), Some(target.join("cargo-home").as_path()));
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn an_integration_test_waits_for_its_package_bin_on_a_cold_target_dir() {
    let result = bin_gate_hold::cold_run("on", &[], bin_gate_hold::Release::AfterTestArtifactLinks).await;
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(0), "{context}");
    assert!(
        result
            .has(TEST_NAME, Outcome::Passed),
        "{context}"
    );
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn an_integration_test_is_not_launched_when_its_package_bin_fails_to_build() {
    let result = bin_gate_hold::cold_run(
        "broken",
        &[("RUSTFLAGS", "--cfg broken_bin"), ("CARGO_VAL_LOG", "debug")],
        bin_gate_hold::Release::AfterTestArtifactLinks,
    ).await;
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(3), "{context}");
    assert!(result.stdout.contains("BUILD    FAILED"), "{context}");
    assert!(
        result.stderr.contains("cargo-val: debug: gate park why=bin-gate kind=test target=uses_bin"),
        "{context}"
    );
    assert!(
        result.stderr.contains("cargo-val: debug: gate drop reason=build-failed target=uses_bin"),
        "{context}"
    );
    assert!(
        !result
            .stdout
            .contains("the_bin_target_exists_when_the_test_starts"),
        "{context}"
    );
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn a_run_whose_bin_fails_before_any_hold_ends_on_its_own_exit_within_seconds() {
    let started = std::time::Instant::now();
    let result = bin_gate_hold::cold_run_within(
        "unreadable-hold",
        &[("CARGO_VAL_BIN_GATE_HOLD", "/nonexistent/bin-hold")],
        bin_gate_hold::Release::AfterTestArtifactLinks,
        fixture::fixture_deadline(),
    ).await;
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(3), "{context}");
    assert!(result.stdout.contains("BUILD    FAILED"), "{context}");
    assert!(started.elapsed() < core::time::Duration::from_secs(30), "the run took {:?}: {context}", started.elapsed());
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn the_same_fixture_fails_with_the_bin_gate_off() {
    let result = bin_gate_hold::cold_run("off", &[("CARGO_VAL_BIN_GATE", "off")], bin_gate_hold::Release::AfterTestFails).await;
    let failed = result.has(TEST_NAME, Outcome::Failed);
    println!(
        "{{\"event\":\"bin_gate_off\",\"status\":{:?},\"test_failed_without_the_rule\":{failed}}}",
        result.status
    );
    assert!(failed, "{}{}", result.stdout, result.stderr);
    assert_eq!(result.status, Some(1));
}

#[proxima::test]
async fn help_prints_the_banner_file_and_documents_the_new_options() {
    let output = output_within(binary::command().arg("--help"), common::settings::bound("HELP_DEADLINE"), "HELP_DEADLINE").await.unwrap_or_else(|failure| panic!("{failure}"));
    let banner = include_str!("fixtures/help-banner.txt");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), banner);
    for needle in ["-E, --filterset <EXPR>", "--include-ignored", "CARGO_VAL_IGNORED", "SUMMARY  passed=P", "CARGO_VAL_BIN_GATE"] {
        assert!(banner.contains(needle), "{needle}");
    }
}

#[cfg(unix)]
fn gate_lines(stderr: &str) -> Vec<String> {
    let fixture = format!(
        "path+file://{}/tests/fixtures/bin_gate",
        env!("CARGO_MANIFEST_DIR")
    );
    stderr
        // cargo orders the bin artifact record against its bin-built message by timing
        .lines()
        .filter(|line| line.starts_with("cargo-val: debug: gate") && !line.contains(" launch kind=bin "))
        .map(|line| {
            let line = line.replace(&fixture, "<fixture>");
            match line.split_once("executable=") {
                Some((head, tail)) => {
                    let (path, rest) = tail.split_once(" package=").expect("package follows executable");
                    let (_, built) = path.split_once("/debug/").expect("bin lives under debug");
                    format!("{head}executable=<target>/debug/{built} package={rest}")
                }
                None => line,
            }
        })
        .collect()
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn gate_debug_lines_for_park_then_bin_built_then_release_are_byte_exact() {
    let result = bin_gate_hold::cold_run(
        "lines",
        &[("CARGO_VAL_LOG", "debug")],
        bin_gate_hold::Release::AfterTestArtifactLinks,
    ).await;

    assert_eq!(
        gate_lines(&result.stderr),
        [
            "cargo-val: debug: gate metadata spawned=yes bin-packages=1",
            "cargo-val: debug: gate park why=bin-gate kind=test target=uses_bin gate=metadata package=<fixture>#0.1.0",
            "cargo-val: debug: gate bin-built on-disk=yes executable=<target>/debug/gate-bin package=<fixture>#0.1.0",
            "cargo-val: debug: gate release reason=bin-built target=uses_bin package=<fixture>#0.1.0",
            "cargo-val: debug: gate build-finished success=true parked=0",
            "cargo-val: debug: gate stdout-eof parked=0",
        ],
        "{}",
        result.stderr
    );
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn gate_debug_lines_for_park_then_failed_build_then_drop_are_byte_exact() {
    let result = bin_gate_hold::cold_run(
        "drop-lines",
        &[("RUSTFLAGS", "--cfg broken_bin"), ("CARGO_VAL_LOG", "debug")],
        bin_gate_hold::Release::AfterTestArtifactLinks,
    ).await;

    assert_eq!(
        gate_lines(&result.stderr),
        [
            "cargo-val: debug: gate metadata spawned=yes bin-packages=1",
            "cargo-val: debug: gate park why=bin-gate kind=test target=uses_bin gate=metadata package=<fixture>#0.1.0",
            "cargo-val: debug: gate build-finished success=false parked=1",
            "cargo-val: debug: gate drop reason=build-failed target=uses_bin package=<fixture>#0.1.0",
            "cargo-val: debug: gate stdout-eof parked=0",
        ],
        "{}",
        result.stderr
    );
}

#[cfg(unix)]
mod fd_scrub {
    use std::os::fd::RawFd;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::OnceLock;

    use super::{manifest, target_directory};

    const JOBSERVER_PAIR_FLAG: &str = "--jobserver-auth";

    // one build per test process into a pid-named target dir: a second cargo uplifting the same bin removes and relinks it, which a concurrent exec of the first sees as ENOENT; get_or_init blocks the other threads of this process until the build is done
    pub fn built() -> PathBuf {
        static HELPER: OnceLock<PathBuf> = OnceLock::new();
        HELPER
            .get_or_init(|| {
                let target = target_directory().join(format!("fd-scrub-rustc-{}", std::process::id()));
                let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                    .args(["build", "--locked", "--offline", "--manifest-path"])
                    .arg(manifest("fd_scrub_rustc"))
                    .env("CARGO_TARGET_DIR", &target)
                    .env_remove("RUSTC_WRAPPER")
                    .env_remove("MAKEFLAGS")
                    .env_remove("CARGO_MAKEFLAGS")
                    .output()
                    .expect("cargo runs to build the fd_scrub_rustc fixture");
                assert!(output.status.success(), "building the fd_scrub_rustc fixture: {}", String::from_utf8_lossy(&output.stderr));
                target.join("debug/fd_scrub_rustc")
            })
            .clone()
    }

    // pipes made with pipe(2) and no fcntl carry no CLOEXEC, the state cargo's rustc children hold between pipe() and the flag being set
    fn leaked_pipe() -> [RawFd; 2] {
        let mut descriptors = [0; 2];
        let made = unsafe { libc::pipe(descriptors.as_mut_ptr()) };
        assert_eq!(made, 0, "pipe: {}", std::io::Error::last_os_error());
        descriptors
    }

    fn listed_fds(program: &std::path::Path, scrubbed: bool, makeflags: Option<String>) -> Vec<RawFd> {
        let mut command = Command::new(program);
        if scrubbed {
            command.arg(program);
        }
        command.arg("--list-fds").env_remove("MAKEFLAGS").env_remove("CARGO_MAKEFLAGS");
        if let Some(flags) = makeflags {
            command.env("CARGO_MAKEFLAGS", flags);
        }
        let output = command.output().expect("the fd_scrub_rustc fixture runs");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).lines().map(|line| line.parse().expect("one fd number per line")).collect()
    }

    #[proxima::test(timeout = "310s")]
    async fn an_unscrubbed_child_inherits_every_pipe_made_without_cloexec() {
        let program = built();
        let [extra_read, extra_write] = leaked_pipe();

        let listed = listed_fds(&program, false, None);

        assert!(listed.contains(&extra_read) && listed.contains(&extra_write), "{listed:?}: the control must see the leaked pipe or the scrub proof below proves nothing");
    }

    #[proxima::test(timeout = "310s")]
    async fn the_scrub_drops_the_leaked_pipe_and_keeps_both_jobserver_fds_named_in_cargo_makeflags() {
        let program = built();
        let [extra_read, extra_write] = leaked_pipe();
        let [jobserver_read, jobserver_write] = leaked_pipe();
        let flags = format!("-j {JOBSERVER_PAIR_FLAG}={jobserver_read},{jobserver_write}");

        let listed = listed_fds(&program, true, Some(flags));

        assert!(!listed.contains(&extra_read) && !listed.contains(&extra_write), "leaked pipe {extra_read},{extra_write} survived in {listed:?}");
        assert_eq!(listed, [0, 1, 2, jobserver_read, jobserver_write]);
    }

    #[proxima::test(timeout = "310s")]
    async fn the_scrub_leaves_only_stdio_without_jobserver_flags_or_with_the_fifo_form() {
        let program = built();
        let [_, _] = leaked_pipe();
        let cases: [Option<String>; 2] = [None, Some("-j --jobserver-auth=fifo:/tmp/GMfifo1234".to_owned())];

        for flags in cases {
            let listed = listed_fds(&program, true, flags.clone());

            assert_eq!(listed, [0, 1, 2], "flags {flags:?}");
        }
    }
}
