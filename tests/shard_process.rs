use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Output;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

mod common;
#[path = "common/binary.rs"]
mod binary;

use common::fixture_output;
#[cfg(unix)]
use common::{ChildStream, drain, drain_until};
#[cfg(unix)]
use futures::future::join3;
#[cfg(unix)]
use proxima_primitives::pipe::Pipe;
#[cfg(unix)]
#[path = "common/supervise.rs"]
mod supervise;
#[cfg(unix)]
use supervise::supervised;

const SHARD: &str = "shard";
const SHARD_LONG: &str = "shard_long";
#[cfg(unix)]
const DEADLINE: Duration = Duration::from_secs(120);
#[cfg(unix)]
const INTERRUPT_STATUS: i32 = 130;

struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    pids: BTreeSet<u32>,
    log: String,
}

impl Run {
    fn lines_ending(&self, label: &str) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.ends_with(&format!(" {label}")))
            .collect()
    }
}

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from);
    let directory = base.join("shard-process").join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("scratch directory is creatable under the target directory");
    directory
}

fn fixture_target() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fixture-target")
}

async fn run_fixture(fixture: &str, label: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Run {
    let directory = scratch(label);
    let log = directory.join("pids.log");
    let output: Output = fixture_output(
        binary::command()
        .env("CARGO_VAL_OUTPUT", "rich")
        .arg("--manifest-path")
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture)
                .join("Cargo.toml"),
        )
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_PROCESS")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_SHARD_LOG", &log)
        .env("CARGO_TARGET_DIR", fixture_target())
        .envs(environment.iter().copied()),
    ).await;
    let log = fs::read_to_string(&log).unwrap_or_default();
    let pids = log
        .lines()
        .filter_map(|line| line.parse().ok())
        .collect();
    Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        pids,
        log,
    }
}

// mkfifo
#[cfg(unix)]
fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo {}", path.display());
}

// kill(pid, 0) liveness probe
#[cfg(unix)]
fn alive(target: libc::pid_t) -> bool {
    unsafe { libc::kill(target, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
}

#[proxima::test(timeout = "310s")]
async fn shard_mode_runs_a_binary_in_about_jobs_children_and_reports_every_test() {
    let run = run_fixture(
        SHARD,
        "children",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "2")],
    ).await;

    assert_eq!(run.status, Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.lines_ending("PASS").len(), 5, "{}", run.stdout);
    assert_eq!(run.lines_ending("IGNORED").len(), 1, "{}", run.stdout);
    assert_eq!(run.pids.len(), 2, "5 tests over 2 jobs is ceil(5/2) = 3 names per shard, so 2 children");
}

#[proxima::test(timeout = "310s")]
async fn test_mode_still_runs_each_test_in_its_own_child() {
    let run = run_fixture(
        SHARD,
        "per-test",
        &["--process", "test", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "2")],
    ).await;

    assert_eq!(run.status, Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.lines_ending("PASS").len(), 5, "{}", run.stdout);
    assert_eq!(run.pids.len(), 5);
}

#[proxima::test(timeout = "310s")]
async fn environment_selects_the_mode_and_the_command_line_overrides_it() {
    let from_environment = run_fixture(
        SHARD,
        "env-mode",
        &["--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_PROCESS", "shard")],
    ).await;
    let overridden = run_fixture(
        SHARD,
        "cli-over-env",
        &["--process=test", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_PROCESS", "shard")],
    ).await;

    assert_eq!((from_environment.pids.len(), overridden.pids.len()), (1, 5));
}

#[proxima::test(timeout = "310s")]
async fn an_unknown_process_mode_is_refused_with_its_source() {
    let run = run_fixture(SHARD, "bad-mode", &["--process", "batch"], &[]).await;

    assert_eq!(run.status, Some(2));
    assert!(run.stderr.contains("--process must be auto, shard, or test"), "{}", run.stderr);
}

#[proxima::test(timeout = "310s")]
async fn a_test_that_aborts_inside_a_shard_fails_alone_and_every_unsettled_name_runs_solo() {
    let run = run_fixture(
        SHARD,
        "abort",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_SHARD_ABORT", "1")],
    ).await;

    assert_eq!(run.status, Some(1), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    assert_eq!(run.lines_ending("FAIL"), ["suite::b_aborts_the_process FAIL"], "{}", run.stdout);
    let passed: BTreeSet<&str> = run.lines_ending("PASS").into_iter().collect();
    let expected: BTreeSet<&str> = [
        "suite::a_passes PASS",
        "suite::c_passes_after_the_crash PASS",
        "suite::d_panics_on_request PASS",
        "suite::e_passes_last PASS",
    ]
    .into();
    assert_eq!(passed, expected);
    assert!(run.stderr.contains("test suite::b_aborts_the_process"), "solo run prints its own output: {}", run.stderr);
    assert_eq!(run.pids.len(), 5, "one shard child, then one solo child for each of the four names it did not settle");
}

#[proxima::test(timeout = "310s")]
async fn a_failing_test_prints_its_own_libtest_block_and_no_other_name_does() {
    let run = run_fixture(
        SHARD,
        "failure-block",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_SHARD_FAIL", "1")],
    ).await;

    assert_eq!(run.status, Some(1));
    assert_eq!(run.lines_ending("FAIL"), ["suite::d_panics_on_request FAIL"]);
    assert!(run.stderr.contains("---- suite::d_panics_on_request stdout ----"), "{}", run.stderr);
    assert!(run.stderr.contains("requested shard failure in d"), "{}", run.stderr);
    assert_eq!(run.stderr.matches("---- suite::").count(), 1, "{}", run.stderr);
}

// the held test blocks on a fifo open
#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn fail_fast_ends_the_shard_and_leaves_its_unreported_names_uncounted() {
    let directory = scratch("hold");
    let hold = directory.join("hold");
    make_fifo(&hold);

    let run = run_fixture(
        SHARD,
        "fail-fast",
        &["--process", "shard"],
        &[
            ("CARGO_VAL_JOBS", "1"),
            ("CARGO_VAL_SHARD_FAIL", "1"),
            ("CARGO_VAL_SHARD_HOLD", hold.to_str().expect("fifo path is UTF-8")),
        ],
    ).await;

    assert_eq!(run.status, Some(1), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    assert!(run.stdout.contains("suite::d_panics_on_request FAIL"), "{}", run.stdout);
    assert!(!run.stdout.contains("e_passes_last"), "the held test never reported: {}", run.stdout);
    assert!(run.stdout.contains("listed tests not run 1"), "{}", run.stdout);
}

#[proxima::test(timeout = "310s")]
async fn names_past_the_command_line_budget_split_into_more_shards_than_jobs() {
    let run = run_fixture(
        SHARD_LONG,
        "long-names",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1")],
    ).await;

    assert_eq!(run.status, Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.lines_ending("PASS").len(), 200, "every long name reported once");
    assert_eq!(run.pids.len(), 2, "200 names of ~210 units each need two 32,767-unit command lines");
}

const SCHED: &str = "sched";

fn launches(run: &Run) -> Vec<&str> {
    run.stderr
        .lines()
        .filter(|line| line.starts_with("cargo-val: debug: shard launch"))
        .collect()
}

fn field<'line>(line: &'line str, key: &str) -> &'line str {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(key))
        .unwrap_or_else(|| panic!("{key} is in {line}"))
}

#[proxima::test(timeout = "310s")]
async fn declared_tests_share_processes_by_group_and_serial_tests_run_with_nothing_else_in_flight() {
    let run = run_fixture(
        SCHED,
        "sched",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "4"), ("CARGO_VAL_LOG", "debug")],
    ).await;

    assert_eq!(run.status, Some(0), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    assert_eq!(run.lines_ending("PASS").len(), 8, "{}", run.stdout);
    let launched = launches(&run);
    let by_class = |class: &str| launched.iter().filter(|line| field(line, "class=") == class).count();
    assert_eq!(
        (by_class("plain"), by_class("shared"), by_class("grouped"), by_class("serial-shared"), by_class("serial")),
        (1, 2, 2, 1, 1),
        "plain: declares_nothing; shared: binary group and gpu group; grouped: the two real-checkpoint cases; \
         serial-shared: runs_alone; serial: loads_whole_checkpoint\n{launched:#?}"
    );
    let shared_names: Vec<&str> = launched
        .iter()
        .filter(|line| field(line, "class=") == "shared")
        .map(|line| field(line, "names="))
        .collect();
    assert!(shared_names.contains(&"2"), "the two gpu tests share one process: {shared_names:?}");
    for line in launched.iter().filter(|line| field(line, "class=").starts_with("serial")) {
        assert_eq!(field(line, "in_flight="), "0", "a serial launch starts with nothing in flight: {line}");
    }
    let order: Vec<&str> = launched.iter().map(|line| field(line, "class=")).collect();
    let first_serial = order.iter().position(|class| class.starts_with("serial")).expect("serial shards launched");
    assert!(order[first_serial..].iter().all(|class| class.starts_with("serial")), "serial classes launch last: {order:?}");
    assert_eq!(run.pids.len(), 7, "one child per launched shard");
}

#[proxima::test(timeout = "310s")]
async fn a_group_runs_one_child_at_a_time_even_with_free_jobs() {
    let run = run_fixture(
        SCHED,
        "group-cap",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "8"), ("CARGO_VAL_LOG", "debug")],
    ).await;

    let launched = launches(&run);
    let capped: Vec<&str> = launched
        .iter()
        .filter(|line| matches!(field(line, "class="), "grouped" | "shared"))
        .map(|line| field(line, "group_in_flight="))
        .collect();

    assert_eq!(run.status, Some(0));
    assert_eq!(capped.len(), 4, "{launched:#?}");
    assert!(capped.iter().all(|count| *count == "0"), "no group had two children at launch: {launched:#?}");
}

const OVERLAP: &str = "sched_overlap";

struct Interval {
    name: String,
    start: u128,
    end: u128,
}

fn intervals(run: &Run) -> Vec<Interval> {
    let record = |line: &str| -> (String, String, u128) {
        let mut parts = line.split_whitespace();
        let kind = parts.next().expect("record has a kind").to_owned();
        let name = parts.next().expect("record has a name").to_owned();
        let nanoseconds = parts.next().expect("record has a time").parse().expect("time is a number");
        (kind, name, nanoseconds)
    };
    let records: Vec<_> = run.log.lines().map(record).collect();
    records
        .iter()
        .filter(|(kind, ..)| kind == "start")
        .map(|(_, name, start)| Interval {
            name: name.clone(),
            start: *start,
            end: records
                .iter()
                .find(|(kind, other, _)| kind == "end" && other == name)
                .map(|(.., end)| *end)
                .expect("every started test ended"),
        })
        .collect()
}

fn overlaps(first: &Interval, second: &Interval) -> bool {
    first.start < second.end && second.start < first.end
}

#[proxima::test(timeout = "310s")]
async fn per_test_mode_runs_serial_alone_and_one_child_per_group_at_a_time() {
    let run = run_fixture(
        OVERLAP,
        "overlap",
        &["--process", "test", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "6")],
    ).await;

    assert_eq!(run.status, Some(0), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    let all = intervals(&run);
    assert_eq!(all.len(), 7, "every name ran exactly once: {}", run.log);
    assert_eq!(run.lines_ending("PASS").len(), 7, "{}", run.stdout);
    let serial = all.iter().find(|interval| interval.name == "loads_whole_checkpoint").expect("the serial test ran");
    let crossings: Vec<&str> = all
        .iter()
        .filter(|other| other.name != serial.name && overlaps(serial, other))
        .map(|other| other.name.as_str())
        .collect();
    assert!(crossings.is_empty(), "the serial test overlapped {crossings:?}\n{}", run.log);
    let group: Vec<&Interval> = all.iter().filter(|interval| interval.name.starts_with("decodes_each_size_")).collect();
    assert_eq!(group.len(), 3, "{}", run.log);
    let crossing = group.iter().enumerate().any(|(index, first)| group[index + 1..].iter().any(|second| overlaps(first, second)));
    assert!(!crossing, "two real-checkpoint cases ran at once\n{}", run.log);
}

const QUIRKS: &str = "shard_quirks";
const QUIRK_NAMES: [&str; 5] = [
    "suite::panics_plain PASS",
    "suite::panics_with_expected_message PASS",
    "suite::panics_then_aborts PASS",
    "suite::child_process_writes_to_stdout PASS",
    "suite::passes_after_the_stray_output PASS",
];

fn statuses(run: &Run) -> BTreeSet<String> {
    run.stdout
        .lines()
        .filter(|line| line.ends_with(" PASS") || line.ends_with(" FAIL") || line.ends_with(" IGNORED"))
        .map(str::to_owned)
        .collect()
}

#[proxima::test(timeout = "310s")]
async fn should_panic_and_stray_child_output_report_the_same_as_per_test_mode() {
    let shard = run_fixture(
        QUIRKS,
        "quirks-shard",
        &["--process", "shard", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1")],
    ).await;
    let per_test = run_fixture(
        QUIRKS,
        "quirks-test",
        &["--process", "test", "--no-fail-fast"],
        &[("CARGO_VAL_JOBS", "1")],
    ).await;

    assert_eq!(shard.status, Some(0), "stdout: {}\nstderr: {}", shard.stdout, shard.stderr);
    assert_eq!(statuses(&shard), statuses(&per_test), "shard:\n{}\nper-test:\n{}", shard.stdout, per_test.stdout);
    let expected: BTreeSet<String> = QUIRK_NAMES.iter().map(|line| (*line).to_owned()).collect();
    assert_eq!(statuses(&shard), expected, "{}", shard.stdout);
    assert!(!shard.stderr.contains("ended without a result line"), "{}", shard.stderr);
}

#[proxima::test(timeout = "310s")]
async fn a_crash_inside_a_should_panic_test_fails_that_test_alone_like_per_test_mode() {
    let environment = [("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_QUIRK_ABORT", "suite::panics_then_aborts")];
    let shard = run_fixture(QUIRKS, "quirks-abort-shard", &["--process", "shard", "--no-fail-fast"], &environment).await;
    let per_test = run_fixture(QUIRKS, "quirks-abort-test", &["--process", "test", "--no-fail-fast"], &environment).await;

    assert_eq!(statuses(&shard), statuses(&per_test), "shard:\n{}\nper-test:\n{}", shard.stdout, per_test.stdout);
    assert!(shard.stdout.contains("suite::panics_then_aborts FAIL"), "{}", shard.stdout);
    assert!(shard.stderr.contains("test suite::panics_then_aborts"), "solo run prints its own output: {}", shard.stderr);
    assert!(!shard.stderr.contains("ended without a result line"), "{}", shard.stderr);
    assert_eq!(shard.lines_ending("PASS").len(), 4, "{}", shard.stdout);
}

const INIT: &str = "shard_init";
const STRAY: &str = "shard_stray";
const MANY: &str = "shard_many";
const MANY_NAMES: usize = 4096;

#[proxima::test(timeout = "310s")]
async fn an_abort_before_any_test_runs_each_name_alone_after_one_shard_child_per_shard() {
    let one = run_fixture(INIT, "init-one", &["--process", "shard", "--no-fail-fast"], &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_INIT_ABORT", "1")]).await;
    let two = run_fixture(INIT, "init-two", &["--process", "shard", "--no-fail-fast"], &[("CARGO_VAL_JOBS", "2"), ("CARGO_VAL_INIT_ABORT", "1")]).await;

    for (run, children) in [(&one, 6), (&two, 7)] {
        assert_eq!(run.status, Some(1), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
        assert_eq!(run.lines_ending("FAIL").len(), 5, "every name is failed exactly once: {}", run.stdout);
        assert_eq!(run.lines_ending("PASS").len(), 0, "{}", run.stdout);
        assert_eq!(run.pids.len(), children, "one shard child per shard (1 at one job, 2 at two), then one solo child per name (5): {}", run.stderr);
    }
}

#[proxima::test(timeout = "310s")]
async fn a_full_shard_that_aborts_at_its_first_test_gives_every_name_exactly_one_solo_run() {
    let run = run_fixture(MANY, "many", &["--process", "shard", "--no-fail-fast"], &[("CARGO_VAL_JOBS", "1")]).await;

    assert_eq!(run.status, Some(0), "stderr tail: {}", run.stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n"));
    assert_eq!(run.lines_ending("PASS").len(), MANY_NAMES, "every name passes in its solo run, counted once");
    assert_eq!(run.lines_ending("FAIL").len(), 0, "{}", run.stdout.lines().take(5).collect::<Vec<_>>().join("\n"));
    assert_eq!(run.log.lines().count(), MANY_NAMES + 1, "one shard child, then exactly one solo child per name; windows reuses pids, so the log lines are counted, not the distinct pids ({})", run.pids.len());
    assert!(!run.stderr.contains("could not queue"), "{}", run.stderr.lines().take(5).collect::<Vec<_>>().join("\n"));
}

#[proxima::test(timeout = "310s")]
async fn a_failing_test_in_fail_fast_still_prints_its_libtest_block() {
    let run = run_fixture(SHARD, "fail-fast-block", &["--process", "shard"], &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_SHARD_FAIL", "1")]).await;

    assert_eq!(run.status, Some(1), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    assert!(run.stderr.contains("---- suite::d_panics_on_request stdout ----"), "{}", run.stderr);
    assert!(run.stderr.contains("requested shard failure in d"), "{}", run.stderr);
}

#[proxima::test(timeout = "310s")]
async fn fail_fast_after_a_crash_keeps_the_crash_as_a_failure_and_withdraws_only_the_rest() {
    let run = run_fixture(
        SHARD,
        "fail-fast-crash",
        &["--process", "shard"],
        &[("CARGO_VAL_JOBS", "1"), ("CARGO_VAL_SHARD_ABORT", "1"), ("CARGO_VAL_SHARD_FAIL", "1")],
    ).await;

    assert_eq!(run.status, Some(1), "stdout: {}\nstderr: {}", run.stdout, run.stderr);
    let full = format!("stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(run.lines_ending("FAIL"), ["suite::b_aborts_the_process FAIL"], "{full}");
    assert_eq!(run.lines_ending("PASS"), ["suite::a_passes PASS"], "{full}");
    assert!(run.stdout.contains("listed tests not run 3"), "{full}");
}

#[proxima::test(timeout = "310s")]
async fn a_stray_ok_line_before_the_verdict_is_decided_by_running_the_test_alone() {
    let shard = run_fixture(STRAY, "stray-shard", &["--process", "shard", "--no-fail-fast"], &[("CARGO_VAL_JOBS", "1")]).await;
    let per_test = run_fixture(STRAY, "stray-test", &["--process", "test", "--no-fail-fast"], &[("CARGO_VAL_JOBS", "1")]).await;

    assert_eq!(shard.status, Some(1), "stdout: {}\nstderr: {}", shard.stdout, shard.stderr);
    assert_eq!(statuses(&shard), statuses(&per_test), "shard:\n{}\nper-test:\n{}", shard.stdout, per_test.stdout);
    assert_eq!(shard.lines_ending("FAIL"), ["suite::stray_ok_then_fails FAIL"], "{}", shard.stdout);
    let passes: BTreeSet<&str> = shard.lines_ending("PASS").into_iter().collect();
    assert_eq!(
        passes,
        BTreeSet::from(["suite::nested_cargo_output_passes PASS", "suite::plain_passes PASS", "suite::stray_ok_then_passes PASS"]),
        "{}",
        shard.stdout
    );
}

#[proxima::test(timeout = "310s")]
async fn a_stray_ok_line_is_settled_once_at_two_jobs_too() {
    let shard = run_fixture(STRAY, "stray-two", &["--process", "shard", "--no-fail-fast"], &[("CARGO_VAL_JOBS", "2")]).await;

    assert_eq!(shard.status, Some(1), "stdout: {}\nstderr: {}", shard.stdout, shard.stderr);
    assert_eq!(shard.lines_ending("FAIL"), ["suite::stray_ok_then_fails FAIL"], "{}", shard.stdout);
    assert_eq!(shard.lines_ending("PASS").len(), 3, "{}", shard.stdout);
}

// sigint delivery and fifo rendezvous
#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn an_interrupt_ends_a_blocked_shard_child_and_cargo_val_exits_promptly() {
    interrupt_ends_blocked_child("shard", "interrupt-shard").await;
}

// sigint delivery and fifo rendezvous
#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly() {
    interrupt_ends_blocked_child("test", "interrupt-test").await;
}

// libc::kill and fifos
#[cfg(unix)]
async fn interrupt_ends_blocked_child(process: &str, label: &str) {
    // the shared target queues this build behind other fixtures' dependency builds, past the pid deadline
    let interrupt_target = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-target"));
    let directory = scratch(label);
    let ready = directory.join("ready");
    let hold = directory.join("hold");
    let log = directory.join("pids.log");
    make_fifo(&ready);
    make_fifo(&hold);
    // boxed: the stream owns ~20 KB of capture window, which would trip clippy::large_futures at both awaiting tests
    let ready_stream = Box::new(ChildStream::new(OwnedFd::from(open_ready(&ready))));
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/shard/Cargo.toml");
    let mut command = binary::command();
    command
        .env("CARGO_VAL_OUTPUT", "rich")
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--process", process, "--no-fail-fast"])
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_PROCESS")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_JOBS", "1")
        .env("CARGO_VAL_SHARD_LOG", &log)
        .env("CARGO_VAL_SHARD_READY", &ready)
        .env("CARGO_VAL_SHARD_HOLD", &hold)
        .env("CARGO_VAL_SHARD_IGNORE_INT", "1")
        .env("CARGO_TARGET_DIR", &interrupt_target);
    let (shard_pid, signalled, exit, stderr) = supervised(&mut command, DEADLINE * 2, async |leader, stdout, stderr, exit| {
        let cargo_val = libc::pid_t::try_from(leader).expect("pid fits pid_t");
        let (reported, _) = drain_until(&ready_stream, |bytes| bytes.contains(&b'\n')).await;
        let shard_pid = String::from_utf8_lossy(&reported).trim().parse::<libc::pid_t>().ok();
        let signalled = shard_pid.map(|_| unsafe { libc::kill(cargo_val, libc::SIGINT) });
        let ((_, _), (stderr, _), exit) = join3(drain(stdout), drain(stderr), exit.call(())).await;
        if let Some(pid) = shard_pid.filter(|pid| alive(*pid)) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        (shard_pid, signalled, exit, String::from_utf8_lossy(&stderr).into_owned())
    })
    .await;

    let shard_pid = shard_pid.expect("the shard child reported its pid before the deadline");
    assert_eq!(signalled, Some(0), "sigint reached cargo-val");
    let status = exit
        .expect("cargo-val exit is observed once")
        .expect("cargo-val exit status is collectable");
    assert_eq!(status.code(), Some(INTERRUPT_STATUS), "stderr: {stderr}");
    let logged = fs::read_to_string(&log).unwrap_or_default();
    assert!(logged.lines().any(|line| line == shard_pid.to_string()), "the logged shard pid is the one that reported: {logged}");
    assert!(!alive(shard_pid), "no shard child survives the interrupt");
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
#[cfg(unix)]
fn open_ready(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .expect("a fifo opens for nonblocking read-write")
}
