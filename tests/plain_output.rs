use std::path::PathBuf;
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::Stdio;

mod common;

use common::fixture_output;
#[cfg(unix)]
use common::{EXIT_WITH_PARENT, FIXTURE_DEADLINE, kill_group};

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const ESCAPE: u8 = 0x1b;
const FAILING_TEST: &str = "fixture::fails_when_requested";
const SOLO_MARK: &str = "  (solo rerun)";

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_run/Cargo.toml")
}

fn command(environment: &[(&str, &str)], arguments: &[&str]) -> Command {
    let mut command = Command::new(CARGO_VAL);
    command
        .arg("--manifest-path")
        .arg(manifest())
        .args(["--tests", "--no-fail-fast"])
        .args(arguments)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_VAL_C52D_FAIL", "1")
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fixture-target"))
        .envs(environment.iter().copied());
    command
}

async fn run(environment: &[(&str, &str)], arguments: &[&str]) -> Output {
    fixture_output(&mut command(environment, arguments)).await
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("plain output is UTF-8")
}

fn line_starting<'text>(text: &'text str, prefix: &str) -> &'text str {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starts with {prefix:?} in:\n{text}"))
}

fn has_three_decimal_seconds(token: &str) -> bool {
    let Some(number) = token.strip_suffix('s') else {
        return false;
    };
    number.split_once('.').is_some_and(|(whole, fraction)| {
        !whole.is_empty()
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.len() == 3
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[proxima::test(timeout = "310s")]
async fn a_piped_run_prints_one_plain_line_per_event_with_stable_columns() {
    let output = run(&[], &[]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(1), "a failing test exits 1:\n{text}");
    let header = text.lines().next().expect("a header line");
    assert!(
        header.starts_with(&format!("cargo-val {}  process=", env!("CARGO_PKG_VERSION"))) && header.contains("  jobs="),
        "header: {header}"
    );
    assert!(text.lines().any(|line| line == "START    cli_run  fixture::passes"));
    assert!(text.lines().any(|line| line == format!("START    cli_run  {FAILING_TEST}")));
    assert!(text.lines().any(|line| line == "IGNORED  cli_run  fixture::ignored_by_default"));
    let pass = line_starting(&text, "PASS     cli_run  fixture::passes  ");
    assert!(has_three_decimal_seconds(pass.rsplit("  ").next().expect("seconds column")), "pass: {pass}");
    let fail = line_starting(&text, "FAIL     ");
    assert!(fail.starts_with(&format!("FAIL     cli_run  {FAILING_TEST}  ")), "fail: {fail}");
    let counted = fail.strip_suffix(SOLO_MARK).unwrap_or(fail);
    assert!(has_three_decimal_seconds(counted.rsplit("  ").next().expect("seconds column")), "fail: {fail}");
}

#[proxima::test(timeout = "310s")]
async fn a_failure_prints_its_captured_output_between_delimiters_right_after_its_fail_line() {
    let text = stdout_of(&run(&[], &[]).await);
    let lines: Vec<&str> = text.lines().collect();

    let fail = lines.iter().position(|line| line.starts_with("FAIL     ")).expect("a FAIL line");
    assert_eq!(lines[fail + 1], format!("------ stdout: cli_run {FAILING_TEST} ------"));
    let end = lines[fail + 1..]
        .iter()
        .position(|line| *line == "------ end stdout ------")
        .map(|offset| fail + 1 + offset)
        .expect("the block closes");
    let block = lines[fail + 2..end].join("\n");
    assert!(block.contains("requested fixture failure"), "panic message is verbatim in the block:\n{block}");
    assert!(block.contains("test result: FAILED. 0 passed; 1 failed"), "libtest summary is verbatim:\n{block}");
}

#[proxima::test(timeout = "310s")]
async fn the_closing_lines_count_list_every_failure_and_give_the_command_that_reruns_only_them() {
    let text = stdout_of(&run(&[], &[]).await);

    let summary = line_starting(&text, "SUMMARY  ");
    assert!(summary.starts_with("SUMMARY  passed=1 failed=1 ignored=1 not_run=0  wall="), "summary: {summary}");
    assert!(has_three_decimal_seconds(summary.rsplit("wall=").next().expect("wall column")), "summary: {summary}");
    assert_eq!(line_starting(&text, "FAILED   "), format!("FAILED   cli_run {FAILING_TEST}"));
    let rerun = line_starting(&text, "RERUN    ");
    assert_eq!(
        rerun,
        format!(
            "RERUN    cargo val --manifest-path '{}' --no-fail-fast -E '(binary(=cli_run) & test(={FAILING_TEST}))'",
            manifest().display()
        )
    );
    assert!(text.lines().any(|line| line.starts_with("total: discovered 3, passed 1, failed 1, ignored 1")));
}

#[proxima::test(timeout = "310s")]
async fn the_printed_rerun_command_reruns_exactly_the_failures() {
    let text = stdout_of(&run(&[], &[]).await);
    let rerun = line_starting(&text, "RERUN    ").trim_start_matches("RERUN    cargo val ");
    let expression = rerun.split(" -E '").nth(1).and_then(|rest| rest.strip_suffix('\'')).expect("a filterset");

    let output = run(&[], &["-E", expression]).await;
    let replay = stdout_of(&output);

    assert!(replay.lines().any(|line| line.starts_with("SUMMARY  passed=0 failed=1 ignored=0")), "{replay}");
    assert!(!replay.lines().any(|line| line.starts_with("PASS ")), "{replay}");
}

#[proxima::test(timeout = "310s")]
async fn a_plain_run_never_emits_an_escape_byte_even_when_color_is_requested() {
    let output = run(&[("CARGO_VAL_COLOR", "true")], &[]).await;

    assert!(!output.stdout.contains(&ESCAPE), "plain stdout has no ESC");
    assert!(!output.stderr.contains(&ESCAPE), "plain stderr has no ESC");
}

fn fresh_target(name: &str) -> String {
    let target = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    _ = std::fs::remove_dir_all(&target);
    target.display().to_string()
}

#[proxima::test(timeout = "310s")]
async fn a_plain_run_asks_cargo_for_no_color_so_cargo_term_color_always_adds_no_escape_byte() {
    let target = fresh_target("plain-color-target");
    let output = run(&[("CARGO_TERM_COLOR", "always"), ("CARGO_TARGET_DIR", &target)], &[]).await;

    assert!(!output.stdout.is_empty(), "the run printed its plain lines");
    assert!(!output.stdout.contains(&ESCAPE), "plain stdout has no ESC");
    assert!(!output.stderr.contains(&ESCAPE), "plain stderr has no ESC");
}

#[proxima::test(timeout = "310s")]
async fn the_same_fresh_build_in_rich_mode_does_carry_cargo_color_so_the_plain_check_can_fail() {
    let target = fresh_target("rich-color-target");
    let output = run(&[("CARGO_TERM_COLOR", "always"), ("CARGO_TARGET_DIR", &target)], &["--output", "rich"]).await;

    assert!(output.stdout.contains(&ESCAPE) || output.stderr.contains(&ESCAPE), "rich output keeps ANSI");
}

#[proxima::test(timeout = "310s")]
async fn github_actions_wraps_each_failure_block_in_a_group_and_annotates_it() {
    let text = stdout_of(&run(&[("GITHUB_ACTIONS", "true")], &[]).await);
    let lines: Vec<&str> = text.lines().collect();

    let group = lines.iter().position(|line| *line == format!("::group::cli_run {FAILING_TEST}")).expect("a group opens");
    let end = lines.iter().position(|line| *line == "::endgroup::").expect("the group closes");
    assert!(group < end);
    assert_eq!(lines[group + 1], format!("------ stdout: cli_run {FAILING_TEST} ------"));
    assert_eq!(
        lines.iter().filter(|line| **line == format!("::error title=cargo val::cli_run::{FAILING_TEST} failed")).count(),
        1
    );
    assert!(lines.iter().any(|line| line.starts_with("SUMMARY  ")), "the plain lines are still there");
}

#[proxima::test(timeout = "310s")]
async fn without_github_actions_no_workflow_commands_are_printed() {
    let text = stdout_of(&run(&[("CI", "true")], &[]).await);

    assert!(!text.contains("::group::") && !text.contains("::error") && !text.contains("::endgroup::"));
}

#[proxima::test(timeout = "310s")]
async fn rich_forced_on_a_pipe_keeps_the_colored_labels_and_no_color_still_wins() {
    let colored = run(&[], &["--output", "rich"]).await;
    let bare = run(&[("NO_COLOR", "1")], &["--output", "rich"]).await;

    assert!(colored.stdout.contains(&ESCAPE), "rich output keeps its ANSI labels");
    assert!(!bare.stdout.contains(&ESCAPE), "NO_COLOR suppresses ANSI in rich mode");
    assert!(!stdout_of(&colored).contains("START"), "rich output is today's output");
}

#[proxima::test(timeout = "310s")]
async fn an_unknown_output_value_is_refused_naming_its_source() {
    let flag = run(&[], &["--output", "fancy"]).await;
    let environment = run(&[("CARGO_VAL_OUTPUT", "fancy")], &[]).await;

    assert_eq!(flag.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&flag.stderr).contains("--output must be auto, rich, or plain"));
    assert_eq!(environment.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&environment.stderr).contains("CARGO_VAL_OUTPUT must be auto, rich, or plain"));
}

#[proxima::test(timeout = "310s")]
async fn shard_mode_reruns_a_shard_failure_alone_and_tags_those_lines() {
    let text = stdout_of(&run(&[], &["--process", "shard"]).await);

    assert!(text.lines().any(|line| line == "START    cli_run  fixture::passes"));
    let start = line_starting(&text, "START    cli_run  fixture::fails_when_requested  (solo rerun)");
    let fail = line_starting(&text, "FAIL     cli_run  fixture::fails_when_requested  ");
    assert!(start.ends_with("(solo rerun)") && fail.ends_with("(solo rerun)"), "{text}");
    assert!(!text.as_bytes().contains(&ESCAPE));
}

#[cfg(unix)]
fn on_a_pty(environment: &[(&str, &str)], arguments: &[&str]) -> Vec<u8> {
    let (mut master, mut slave) = (-1, -1);
    let status = unsafe {
        libc::openpty(&raw mut master, &raw mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    assert_eq!(status, 0, "openpty allocates a pair");
    // openpty hands back inheritable descriptors; a master that reaches cargo-val keeps the pty alive after this process dies
    for descriptor in [master, slave] {
        assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) }, 0, "close-on-exec is settable");
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut invocation = command(environment, arguments);
    invocation.env(EXIT_WITH_PARENT, "1").process_group(0).stdin(Stdio::null()).stdout(Stdio::from(slave)).stderr(Stdio::null());
    let mut child = invocation.spawn().expect("cargo-val is spawnable");
    drop(invocation);
    let received = read_until_hangup(&master, child.id());
    child.wait().expect("cargo-val ends");
    received
}

// the master reads to the hangup that follows the last slave end closing; a run that outlives FIXTURE_DEADLINE is killed with its group
#[cfg(unix)]
fn read_until_hangup(master: &OwnedFd, leader: u32) -> Vec<u8> {
    let started = std::time::Instant::now();
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let remaining = FIXTURE_DEADLINE.saturating_sub(started.elapsed());
        let mut watched = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(master), events: libc::POLLIN, revents: 0 };
        let timeout = i32::try_from(remaining.as_millis()).expect("the deadline fits poll");
        if unsafe { libc::poll(&raw mut watched, 1, timeout) } == 0 {
            let failures = kill_group(leader).err();
            panic!("cargo-val (pid {leader}) outlived {FIXTURE_DEADLINE:?} on a pty; group kill: {failures:?}\nreceived so far:\n{}", String::from_utf8_lossy(&received));
        }
        let count = unsafe { libc::read(std::os::fd::AsRawFd::as_raw_fd(master), chunk.as_mut_ptr().cast(), chunk.len()) };
        match usize::try_from(count) {
            Ok(1..) => received.extend_from_slice(&chunk[..usize::try_from(count).expect("a positive count fits usize")]),
            _ => return received,
        }
    }
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn output_plain_forces_plain_lines_even_when_stdout_is_a_terminal() {
    let screen = on_a_pty(&[], &["--output", "plain"]);

    assert!(!screen.contains(&ESCAPE), "no ANSI on a pty with --output plain");
    assert!(String::from_utf8_lossy(&screen).contains("SUMMARY  passed=1 failed=1"));
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn auto_on_a_terminal_is_rich_unless_ci_is_set() {
    let rich = on_a_pty(&[], &[]);
    let in_ci = on_a_pty(&[("CI", "true")], &[]);

    assert!(rich.contains(&ESCAPE) && !String::from_utf8_lossy(&rich).contains("SUMMARY  "), "terminal gets today's output");
    assert!(!in_ci.contains(&ESCAPE) && String::from_utf8_lossy(&in_ci).contains("SUMMARY  "), "CI gets plain even on a terminal");
}

#[proxima::test(timeout = "310s")]
async fn a_shard_run_settles_its_failure_on_the_solo_rerun_line_with_the_seconds_column_before_the_mark() {
    let text = stdout_of(&run(&[("CARGO_VAL_PROCESS", "shard")], &[]).await);

    let fail = line_starting(&text, "FAIL     ");
    let counted = fail.strip_suffix(SOLO_MARK).expect("a shard failure is decided by its solo rerun");
    assert!(counted.starts_with(&format!("FAIL     cli_run  {FAILING_TEST}  ")), "fail: {fail}");
    assert!(has_three_decimal_seconds(counted.rsplit("  ").next().expect("seconds column")), "fail: {fail}");
    assert!(text.lines().any(|line| line == format!("START    cli_run  {FAILING_TEST}{SOLO_MARK}")), "{text}");
}

async fn build_error_run(mode: &str) -> Output {
    let mut command = Command::new(CARGO_VAL);
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_error/Cargo.toml"))
        .arg("--lib")
        .env_remove("CI")
        .env_remove("NO_COLOR")
        .env_remove("CARGO_VAL_COLOR")
        .env("CARGO_VAL_OUTPUT", mode)
        .env("CARGO_TERM_COLOR", "always")
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{mode}-build-error-target")));
    fixture_output(&mut command).await
}

fn assert_diagnostics_then_summary_then_cargo_val_line(text: &str) {
    let diagnostic = text.find("E0308").unwrap_or_else(|| panic!("no E0308:\n{text}"));
    let summary = text.find(": could not compile").unwrap_or_else(|| panic!("no cargo summary:\n{text}"));
    let failed = text.find("cargo-val: cargo test build failed").unwrap_or_else(|| panic!("no cargo-val line:\n{text}"));
    assert!(diagnostic < summary && summary < failed, "order is rustc, cargo, cargo-val:\n{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_failed_build_in_plain_mode_orders_rustc_then_cargo_then_cargo_val_with_no_escape_bytes() {
    let output = build_error_run("plain").await;
    let text = String::from_utf8_lossy(&output.stderr);

    assert_diagnostics_then_summary_then_cargo_val_line(&text);
    assert!(!output.stderr.contains(&ESCAPE), "0 ESC bytes expected:\n{text}");
    assert_eq!(output.status.code(), Some(3), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_failed_build_in_rich_mode_keeps_the_same_order() {
    let output = build_error_run("rich").await;
    let text = String::from_utf8_lossy(&output.stderr);

    assert_diagnostics_then_summary_then_cargo_val_line(&text);
    assert_eq!(output.status.code(), Some(3), "{text}");
}
