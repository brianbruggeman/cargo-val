use std::fs;
use std::path::{Path, PathBuf};

// each file below sets a cargo environment variable by hand or runs a command unbounded, and the reason is why the isolated helper or the bounded wait cannot serve it
const ENVIRONMENT_KEY_ALLOWED: [(&str, &str); 8] = [
    ("common/isolation.rs", "the helper itself names the keys it sets"),
    ("common/lock_report.rs", "reads the keys from the command a deadline killed"),
    ("common/lock_holder.rs", "the holder is a second cargo that must share the build directory under test"),
    ("isolated_target.rs", "holds a shared build lock on purpose and measures the wait"),
    ("lock_wait.rs", "holds a shared build lock on purpose and classifies the wait"),
    ("fixture_panic.rs", "a sleep child whose deadline report must read its target and home"),
    ("ignore_modes.rs", "a unit test asserting the keys the bin-gate command carries"),
    ("output_backpressure.rs", "makes the store directory under the target read-only before the run"),
];

const MAIN_THREAD_STACK: (&str, &str) = ("main_thread_stack.rs", "drive runs cargo in this process, so CARGO_HOME is scoped with temp_env");

const UNBOUNDED_OUTPUT_ALLOWED: [(&str, &str); 5] = [
    ("child_capture.rs", "the std reference run of echo, --version and --help that the bounded capture is compared against"),
    ("ignore_modes.rs", "runs the fd_scrub_rustc helper binary, which must inherit the test's own leaked pipes"),
    ("common/deadline_report.rs", "ps and sccache reads that run after a deadline already fired"),
    ("common/lock_report.rs", "lsof and ps reads that run after a deadline already fired"),
    ("rich_pty.rs", "pgrep of a finished session"),
];

fn sources(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).expect("the directory is readable") {
        let path = entry.expect("the entry is readable").path();
        if path.is_dir() && path.file_name().is_some_and(|name| name != "fixtures" && name != "golden") {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found
}

fn relative(path: &Path) -> String {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    path.strip_prefix(tests).expect("the source is under tests").to_string_lossy().replace('\\', "/")
}

fn hand_set_environment_key(text: &str) -> Vec<usize> {
    text.lines().enumerate().filter(|(_, line)| line.contains("\"CARGO_TARGET_DIR\"") || line.contains("\"CARGO_HOME\"")).map(|(index, _)| index + 1).collect()
}

fn unbounded_output(text: &str) -> Vec<usize> {
    text.lines().enumerate().filter(|(_, line)| line.contains(".output()")).map(|(index, _)| index + 1).collect()
}

fn offenders(find: fn(&str) -> Vec<usize>, allowed: &[(&str, &str)]) -> (usize, Vec<String>) {
    let scanned = sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"));
    let found = scanned
        .iter()
        .filter(|path| !relative(path).ends_with("isolation_lint.rs") && !allowed.iter().any(|(name, _)| *name == relative(path)))
        .flat_map(|path| find(&fs::read_to_string(path).expect("the source is readable")).into_iter().map(|line| format!("{}:{line}", relative(path))).collect::<Vec<_>>())
        .collect();
    (scanned.len(), found)
}

#[test]
fn no_test_sets_cargo_target_dir_or_cargo_home_by_hand_outside_the_allowed_files() {
    let mut allowed = ENVIRONMENT_KEY_ALLOWED.to_vec();
    allowed.push(MAIN_THREAD_STACK);

    let (scanned, found) = offenders(hand_set_environment_key, &allowed);

    assert!(scanned > 40, "the scan saw {scanned} test sources");
    assert!(found.is_empty(), "these set CARGO_TARGET_DIR or CARGO_HOME without common/isolation.rs, which makes a target and a package-cache lock shared between tests: {found:?}");
}

#[test]
fn no_test_runs_a_command_to_completion_without_the_bounded_wait_outside_the_allowed_files() {
    let (scanned, found) = offenders(unbounded_output, &UNBOUNDED_OUTPUT_ALLOWED);

    assert!(scanned > 40, "the scan saw {scanned} test sources");
    assert!(found.is_empty(), "these call .output() with no deadline, so a hang waits forever instead of naming the cargo lock holders: {found:?}");
}

#[test]
fn the_scan_flags_a_hand_set_target_and_an_unbounded_output() {
    let violation = "command.env(\"CARGO_TARGET_DIR\", target).output().expect(\"runs\");";

    assert_eq!(hand_set_environment_key(violation), [1]);
    assert_eq!(unbounded_output(violation), [1]);
    assert_eq!(hand_set_environment_key("command.envs(isolation::environment(\"label\"));"), Vec::<usize>::new());
}
