use std::path::PathBuf;
use std::process::Command;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn lines_with(&self, prefix: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with(prefix))
            .collect()
    }

    fn total_without_wall(&self) -> String {
        let total = self.lines_with("total: ");
        assert_eq!(total.len(), 1, "{}{}", self.stdout, self.stderr);
        let (counts, wall) = total[0].split_once(", wall ").expect("total carries wall time");
        assert!(
            wall.ends_with('s') && wall.contains('.'),
            "wall time shape: {wall}"
        );
        counts.to_string()
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

fn run(manifest_path: &std::path::Path, arguments: &[&str], environment: &[(&str, &str)]) -> Run {
    let mut command = Command::new(CARGO_VAL);
    command
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_IGNORED")
        .env_remove("CARGO_VAL_FILTERSET")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("ignore-modes-fixture"));
    for (key, value) in environment {
        command.env(key, value);
    }
    let output = command.output().expect("cargo-val binary is spawnable");
    Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn fixture(arguments: &[&str]) -> Run {
    with_environment(arguments, &[])
}

fn with_environment(arguments: &[&str], environment: &[(&str, &str)]) -> Run {
    let mut full = vec!["--lib"];
    full.extend_from_slice(arguments);
    run(&manifest("ignore_modes"), &full, environment)
}

const BINARY: &str = "ignore_modes: discovered";

#[test]
fn default_reports_the_ignored_test_as_ignored_and_never_launches_it() {
    let result = fixture(&[]);
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(1), "{context}");
    assert!(result.stdout.contains("fixture::passes PASS"), "{context}");
    assert!(result.stdout.contains("fixture::fails FAIL"), "{context}");
    assert!(
        result.stdout.contains("fixture::ignored_with_reason IGNORED"),
        "{context}"
    );
    assert!(!result.stdout.contains("fixture::ignored_with_reason PASS"), "{context}");
    assert_eq!(
        result.lines_with(BINARY),
        ["ignore_modes: discovered 3, passed 1, failed 1, ignored 1, filtered 0"],
        "{context}"
    );
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 1, failed 1, ignored 1, filtered 0"
    );
    assert!(!result.stdout.contains('\u{1b}'), "NO_COLOR is respected");
}

#[test]
fn ignored_flag_runs_only_the_ignored_test_and_filters_the_rest() {
    let result = fixture(&["--", "--ignored"]);
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(0), "{context}");
    assert!(
        result.stdout.contains("fixture::ignored_with_reason PASS"),
        "{context}"
    );
    assert_eq!(
        result.lines_with(BINARY),
        ["ignore_modes: discovered 3, passed 1, failed 0, ignored 0, filtered 2"],
        "{context}"
    );
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 1, failed 0, ignored 0, filtered 2"
    );
}

#[test]
fn include_ignored_flag_runs_every_test() {
    let result = fixture(&["--", "--include-ignored"]);
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(1), "{context}");
    assert_eq!(
        result.lines_with(BINARY),
        ["ignore_modes: discovered 3, passed 2, failed 1, ignored 0, filtered 0"],
        "{context}"
    );
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 2, failed 1, ignored 0, filtered 0"
    );
}

#[test]
fn ignore_mode_setting_equals_the_libtest_flag() {
    let flag = fixture(&["--", "--include-ignored"]);
    let setting = with_environment(&[], &[("CARGO_VAL_IGNORED", "include")]);

    assert_eq!(setting.total_without_wall(), flag.total_without_wall());
    assert_eq!(setting.status, flag.status);
}

#[test]
fn both_ignore_flags_together_are_refused_as_in_libtest() {
    let result = fixture(&["--", "--ignored", "--include-ignored"]);

    assert_eq!(result.status, Some(2), "{}{}", result.stdout, result.stderr);
    assert!(result.stderr.contains("invalid arguments"), "{}", result.stderr);
}

#[test]
fn only_ignored_selected_and_nothing_executed_is_red_by_the_no_tests_policy() {
    let result = fixture(&["-E", "test(ignored_with_reason)"]);
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(4), "{context}");
    assert_eq!(
        result.total_without_wall(),
        "total: discovered 3, passed 0, failed 0, ignored 1, filtered 2"
    );
    assert!(result.stderr.contains("no tests to run"), "{context}");
}

#[test]
fn filterset_excludes_before_launch_and_counts_filtered() {
    let cases: [(&str, i32, &str); 5] = [
        ("test(=fixture::passes)", 0, "total: discovered 3, passed 1, failed 0, ignored 0, filtered 2"),
        ("not test(fails)", 0, "total: discovered 3, passed 1, failed 0, ignored 1, filtered 1"),
        ("test(#fixture::[pf]*) - test(fails)", 0, "total: discovered 3, passed 1, failed 0, ignored 0, filtered 2"),
        ("package(ignore_*) & binary(#ignore_modes) & test(#*::fails)", 1, "total: discovered 3, passed 0, failed 1, ignored 0, filtered 2"),
        ("package(other) | none()", 4, "total: discovered 3, passed 0, failed 0, ignored 0, filtered 3"),
    ];
    for (expression, status, total) in cases {
        let result = fixture(&["-E", expression]);
        let context = format!("{expression}: {}{}", result.stdout, result.stderr);
        assert_eq!(result.status, Some(status), "{context}");
        assert_eq!(result.total_without_wall(), total, "{context}");
    }
}

#[test]
fn filterset_setting_equals_the_flag_and_the_flag_wins() {
    let flag = fixture(&["--filterset", "test(passes)"]);
    let setting = with_environment(&[], &[("CARGO_VAL_FILTERSET", "test(passes)")]);
    let overridden = with_environment(&["-E", "test(passes)"], &[("CARGO_VAL_FILTERSET", "none()")]);

    assert_eq!(setting.total_without_wall(), flag.total_without_wall());
    assert_eq!(overridden.total_without_wall(), flag.total_without_wall());
}

#[test]
fn malformed_filterset_fails_with_a_caret_before_any_build() {
    let missing = PathBuf::from("/nonexistent/cargo-val-fixture/Cargo.toml");
    let result = run(&missing, &["--lib", "-E", "test(a) & & test(b)"], &[]);

    assert_eq!(result.status, Some(2), "{}", result.stderr);
    assert_eq!(
        result.stderr,
        "cargo-val: invalid filterset: expected a predicate, `(`, `not` or `!`\n  test(a) & & test(b)\n            ^\n"
    );
    assert_eq!(result.stdout, "");
}

#[test]
fn an_exact_launch_that_ran_nothing_is_an_error_naming_the_test() {
    let result = run(&manifest("ghost_harness"), &["--test", "ghost"], &[]);

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
    use std::fs::{File, OpenOptions};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;

    use super::{CARGO_VAL, Run, manifest, target_directory};

    const TEST_FAILED_LINE: &str = "the_bin_target_exists_when_the_test_starts FAIL";

    /// What proves the bin-less window was observed, so the bin build may be released.
    #[derive(Clone, Copy)]
    pub enum Release {
        /// gate off: the test ran with no bin and said so.
        AfterTestFails,
        /// gate on: the test executable is linked and cargo-val is holding it for the bin.
        AfterTestArtifactLinks,
    }

    fn make_fifo(path: &Path) {
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul");
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
    }

    fn test_artifact_linked(deps: &Path) -> bool {
        std::fs::read_dir(deps).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("uses_bin-") && !name.contains('.')
            })
        })
    }

    fn observed(release: Release, deps: &Path, stdout: &Path) -> bool {
        match release {
            Release::AfterTestFails => std::fs::read_to_string(stdout)
                .is_ok_and(|text| text.contains(TEST_FAILED_LINE)),
            Release::AfterTestArtifactLinks => test_artifact_linked(deps),
        }
    }

    // rustc may open the fifo more than once (sccache runs a dep-info pass first), so one write-open would strand a later reader; the loop ends when the target directory is removed
    fn release_bin_build(fifo: &Path) {
        let fifo = fifo.to_path_buf();
        thread::spawn(move || {
            while fifo.exists() {
                drop(OpenOptions::new().write(true).open(&fifo));
            }
        });
    }

    fn output_file(directory: &Path, name: &str) -> (PathBuf, File) {
        let path = directory.join(name);
        let file = File::create(&path).expect("output file is creatable");
        (path, file)
    }

    pub fn cold_run(label: &str, environment: &[(&str, &str)], release: Release) -> Run {
        let target = target_directory().join(format!("bin-gate-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&target);
        let outputs = target.join("outputs");
        std::fs::create_dir_all(&outputs).expect("target directory is creatable");
        let fifo = target.join("bin-hold");
        make_fifo(&fifo);
        let (stdout_path, stdout_file) = output_file(&outputs, "stdout");
        let (stderr_path, stderr_file) = output_file(&outputs, "stderr");
        let mut command = Command::new(CARGO_VAL);
        command
            .arg("--manifest-path")
            .arg(manifest("bin_gate"))
            .env_remove("CARGO_VAL_BIN_GATE")
            .env("NO_COLOR", "1")
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_VAL_TARGET_DIR", "shared")
            .env("CARGO_VAL_BIN_GATE_HOLD", &fifo)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("cargo-val binary is spawnable");
        let deps = target.join("debug/deps");
        let status = loop {
            if let Some(status) = child.try_wait().expect("cargo-val is pollable") {
                break status;
            }
            if observed(release, &deps, &stdout_path) {
                release_bin_build(&fifo);
                break child.wait().expect("cargo-val is waitable");
            }
            thread::yield_now();
        };
        let run = Run {
            status: status.code(),
            stdout: std::fs::read_to_string(&stdout_path).expect("stdout is readable"),
            stderr: std::fs::read_to_string(&stderr_path).expect("stderr is readable"),
        };
        let _ = std::fs::remove_dir_all(&target);
        run
    }
}

#[cfg(unix)]
#[test]
fn an_integration_test_waits_for_its_package_bin_on_a_cold_target_dir() {
    let result = bin_gate_hold::cold_run("on", &[], bin_gate_hold::Release::AfterTestArtifactLinks);
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(0), "{context}");
    assert!(
        result
            .stdout
            .contains("the_bin_target_exists_when_the_test_starts PASS"),
        "{context}"
    );
}

#[cfg(unix)]
#[test]
fn an_integration_test_is_not_launched_when_its_package_bin_fails_to_build() {
    let result = bin_gate_hold::cold_run(
        "broken",
        &[("RUSTFLAGS", "--cfg broken_bin"), ("CARGO_VAL_LOG", "debug")],
        bin_gate_hold::Release::AfterTestArtifactLinks,
    );
    let context = format!("{}{}", result.stdout, result.stderr);

    assert_eq!(result.status, Some(2), "{context}");
    assert!(result.stderr.contains("cargo test build failed"), "{context}");
    assert!(
        result.stderr.contains("cargo-val: debug: gate park kind=test target=uses_bin"),
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
#[test]
fn the_same_fixture_fails_with_the_bin_gate_off() {
    let result = bin_gate_hold::cold_run("off", &[("CARGO_VAL_BIN_GATE", "off")], bin_gate_hold::Release::AfterTestFails);
    let failed = result
        .stdout
        .contains("the_bin_target_exists_when_the_test_starts FAIL");
    println!(
        "{{\"event\":\"bin_gate_off\",\"status\":{:?},\"test_failed_without_the_rule\":{failed}}}",
        result.status
    );
    assert!(failed, "{}{}", result.stdout, result.stderr);
    assert_eq!(result.status, Some(1));
}

#[test]
fn help_prints_the_banner_file_and_documents_the_new_options() {
    let output = Command::new(CARGO_VAL)
        .arg("--help")
        .output()
        .expect("cargo-val binary is spawnable");
    let banner = include_str!("fixtures/help-banner.txt");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), banner);
    for needle in ["-E, --filterset <EXPR>", "--include-ignored", "CARGO_VAL_IGNORED", "total: discovered", "CARGO_VAL_BIN_GATE"] {
        assert!(banner.contains(needle), "{needle}");
    }
}
