use std::path::PathBuf;
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::Stdio;

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/plain.rs"]
mod plain;
#[path = "common/plain_summary.rs"]
mod plain_summary;
#[cfg(unix)]
#[path = "common/wait.rs"]
mod wait;

use fixture::fixture_output;
#[cfg(unix)]
use common::group;
#[cfg(unix)]
use common::group::kill_process_group;
#[cfg(unix)]
use fixture::fixture_deadline;
#[cfg(unix)]
use binary::EXIT_WITH_PARENT;

const ESCAPE: u8 = 0x1b;
const FAILING_TEST: &str = "fixture::fails_when_requested";
const SOLO_MARK: &str = "  (solo rerun)";

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_run/Cargo.toml")
}

fn command(environment: &[(&str, &str)], arguments: &[&str]) -> Command {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .args(["--tests", "--no-fail-fast"])
        .args(arguments)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CARGO_VAL_OUTPUT")        .env_remove("CLAUDECODE")
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

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
#[cfg(unix)]
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[cfg(unix)]
fn rposition(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|window| window == needle)
}

// the end of the last `ESC [ <digits> A`, the cursor-up that opens an in-place redraw
fn last_cursor_up(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).rposition(|window| window[0] == ESCAPE && window[1] == b'[' && window[2].is_ascii_digit() && window[3] == b'A').or_else(|| {
        bytes.windows(5).rposition(|window| window[0] == ESCAPE && window[1] == b'[' && window[2].is_ascii_digit() && window[3].is_ascii_digit() && window[4] == b'A')
    })
}

fn colour_sequences(bytes: &[u8]) -> usize {
    bytes
        .split(|byte| *byte == ESCAPE)
        .skip(1)
        .filter(|rest| rest.first() == Some(&b'[') && rest[1..].iter().find(|byte| !byte.is_ascii_digit() && **byte != b';') == Some(&b'm'))
        .count()
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
async fn rich_forced_on_a_pipe_draws_the_frame_in_colour_and_no_color_keeps_the_frame_but_drops_the_colour() {
    let colored = run(&[], &["--output", "rich"]).await;
    let bare = run(&[("NO_COLOR", "1")], &["--output", "rich"]).await;

    assert!(colour_sequences(&colored.stdout) > 0 && contains(&colored.stdout, "\u{2588}".as_bytes()), "colour draws solid bars");
    assert_eq!(colour_sequences(&bare.stdout), 0, "NO_COLOR suppresses every SGR sequence");
    assert!(contains(&bare.stdout, "\u{2591}".as_bytes()), "no colour draws the bar as glyph textures");
    for output in [&colored, &bare] {
        assert!(contains(&output.stdout, HIDE_CURSOR) && last_cursor_up(&output.stdout).is_some(), "the frame hides the cursor and redraws in place");
        assert!(!stdout_of(output).contains("START"), "rich output is the frame, not the plain lines");
    }
}

#[proxima::test(timeout = "310s")]
async fn an_unknown_output_value_is_refused_naming_its_source() {
    let flag = run(&[], &["--output", "fancy"]).await;
    let environment = run(&[("CARGO_VAL_OUTPUT", "fancy")], &[]).await;

    assert_eq!(flag.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&flag.stderr).contains("--output must be auto, rich, plain, agent, or json"));
    assert_eq!(environment.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&environment.stderr).contains("CARGO_VAL_OUTPUT must be auto, rich, plain, agent, or json"));
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
fn spawn_on_pty(invocation: &mut Command) -> (OwnedFd, std::process::Child) {
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
    invocation.env(EXIT_WITH_PARENT, "1").process_group(0).stdin(Stdio::null()).stdout(Stdio::from(slave)).stderr(Stdio::null());
    let child = invocation.spawn().expect("cargo-val is spawnable");
    (master, child)
}

#[cfg(unix)]
async fn on_a_pty(environment: &[(&str, &str)], arguments: &[&str]) -> Vec<u8> {
    let mut invocation = command(environment, arguments);
    let (master, child) = spawn_on_pty(&mut invocation);
    drop(invocation);
    let received = read_until_hangup(&master, child.id()).await;
    wait::wait_within(&child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not end after its pty hung up: {failure}"));
    received
}

#[cfg(unix)]
fn blocked_command(directory: &std::path::Path) -> Command {
    let mut invocation = binary::command();
    invocation
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/blocked_test/Cargo.toml"))
        .args(["--output", "rich", "--lib", "unit_blocks_when_requested"])
        .env_remove("CI")
        .env_remove("NO_COLOR")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", directory)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pty-sigint-fixture-target"));
    invocation
}

#[cfg(unix)]
fn make_fifo(path: &std::path::Path) {
    let name = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str())).expect("fifo path has no nul byte");
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo {}: {}", path.display(), std::io::Error::last_os_error());
}

// read+write keeps a reader-side fifo open without a writer; the fixture opens the other end itself
#[cfg(unix)]
fn open_fifo(path: &std::path::Path, writable: bool) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(writable)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

// the blocked test announces its pid on `ready` once it is running; the pty is drained meanwhile so the frame never stalls on a full buffer
#[cfg(unix)]
async fn await_ready(master: &OwnedFd, ready: &std::fs::File, leader: u32, screen: &mut Vec<u8>) {
    let started = std::time::Instant::now();
    let mut chunk = [0_u8; 4096];
    loop {
        let remaining = fixture_deadline().saturating_sub(started.elapsed());
        let mut watched = [
            libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(master), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(ready), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = i32::try_from(remaining.as_millis()).expect("the deadline fits poll");
        if unsafe { libc::poll(watched.as_mut_ptr(), 2, timeout) } == 0 {
            let failures = kill_process_group(leader).await.err();
            panic!("the blocked test never reached the rendezvous within {:?}; group kill: {failures:?}\n{}", fixture_deadline(), String::from_utf8_lossy(screen));
        }
        if watched[1].revents & libc::POLLIN != 0 {
            return;
        }
        let count = unsafe { libc::read(std::os::fd::AsRawFd::as_raw_fd(master), chunk.as_mut_ptr().cast(), chunk.len()) };
        match usize::try_from(count) {
            Ok(1..) => screen.extend_from_slice(&chunk[..usize::try_from(count).expect("a positive count fits usize")]),
            _ => panic!("cargo-val hung up before the blocked test was ready:\n{}", String::from_utf8_lossy(screen)),
        }
    }
}

// the master reads to the hangup that follows the last slave end closing; a run that outlives fixture_deadline() is killed with its group
#[cfg(unix)]
async fn read_until_hangup(master: &OwnedFd, leader: u32) -> Vec<u8> {
    let started = std::time::Instant::now();
    let mut received = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let remaining = fixture_deadline().saturating_sub(started.elapsed());
        let mut watched = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(master), events: libc::POLLIN, revents: 0 };
        let timeout = i32::try_from(remaining.as_millis()).expect("the deadline fits poll");
        if unsafe { libc::poll(&raw mut watched, 1, timeout) } == 0 {
            let failures = kill_process_group(leader).await.err();
            panic!("cargo-val (pid {leader}) outlived {:?} on a pty; group kill: {failures:?}\nreceived so far:\n{}", fixture_deadline(), String::from_utf8_lossy(&received));
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
    let screen = on_a_pty(&[], &["--output", "plain"]).await;

    assert!(!screen.contains(&ESCAPE), "no ANSI on a pty with --output plain");
    assert!(String::from_utf8_lossy(&screen).contains("SUMMARY  passed=1 failed=1"));
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn auto_on_a_terminal_is_rich_unless_ci_is_set() {
    let rich = on_a_pty(&[], &[]).await;
    let in_ci = on_a_pty(&[("CI", "true")], &[]).await;

    assert!(contains(&rich, HIDE_CURSOR) && contains(&rich, "\u{2588}".as_bytes()), "a terminal gets the frame");
    assert!(!in_ci.contains(&ESCAPE) && String::from_utf8_lossy(&in_ci).contains("SUMMARY  "), "CI gets plain even on a terminal");
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn rich_on_a_pty_draws_the_frame_ends_with_the_closing_block_and_shows_the_cursor_after_the_last_frame() {
    let screen = on_a_pty(&[], &["--output", "rich"]).await;
    let text = String::from_utf8_lossy(&screen);

    assert!(contains(&screen, "\u{2588}".as_bytes()) && last_cursor_up(&screen).is_some(), "bar glyphs and a cursor-up redraw:\n{text:?}");
    let last_frame_end = rposition(&screen, b"\x1b[J").expect("the last redraw clears below the cursor") + b"\x1b[J".len();
    let shown = rposition(&screen, SHOW_CURSOR).expect("the cursor is shown again");
    assert!(shown >= last_frame_end, "the cursor returns after the last frame is erased:\n{text:?}");
    assert!(!contains(&screen[last_frame_end..], HIDE_CURSOR), "nothing draws a frame once the final erase has run:\n{text:?}");
    assert_eq!(screen.windows(SHOW_CURSOR.len()).filter(|window| *window == SHOW_CURSOR).count(), 1, "the cursor is shown exactly once:\n{text:?}");
    let closing = &text[last_frame_end..shown];
    assert!(closing.contains("SUMMARY  passed=1 failed=1"), "the closing block sits between the erase and the cursor:\n{closing}");
    assert!(screen.ends_with(SHOW_CURSOR), "the cursor returns last, after the closing block:\n{text:?}");
    let before_cursor = &text[..text.len() - SHOW_CURSOR.len()];
    assert!(before_cursor.trim_end().lines().last().is_some_and(|line| line.starts_with("RERUN    ")), "the closing block ends with the rerun line:\n{text:?}");
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn shard_mode_rich_on_a_pty_prints_no_per_test_lines_between_frames() {
    let screen = on_a_pty(&[], &["--output", "rich", "--process", "shard"]).await;
    let text = String::from_utf8_lossy(&screen);

    assert!(contains(&screen, HIDE_CURSOR) && last_cursor_up(&screen).is_some(), "the shard run draws the frame:\n{text:?}");
    let last_frame_end = rposition(&screen, b"\x1b[J").expect("the last redraw clears below the cursor") + b"\x1b[J".len();
    let before_closing = &text[..last_frame_end];
    assert!(!before_closing.contains("fixture::passes"), "a passing test prints no line while frames are live:\n{before_closing:?}");
    assert!(!before_closing.contains("fixture::ignored_by_default"), "an ignored test prints no line while frames are live:\n{before_closing:?}");
    assert!(text[last_frame_end..].contains("SUMMARY  passed=1 failed=1") && screen.ends_with(SHOW_CURSOR), "the closing block follows the erase and the cursor returns after it:\n{text:?}");
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn sigint_during_a_rich_run_on_a_pty_still_shows_the_cursor_and_exits_130() {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pty-sigint-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    let ready = open_fifo(&directory.join("ready"), true);
    let _held = open_fifo(&directory.join("held"), false);
    let mut invocation = blocked_command(&directory);
    let (master, child) = spawn_on_pty(&mut invocation);
    drop(invocation);
    let mut screen = Vec::new();
    await_ready(&master, &ready, child.id(), &mut screen).await;

    assert_eq!(unsafe { libc::kill(libc::pid_t::try_from(child.id()).expect("pid fits pid_t"), libc::SIGINT) }, 0, "SIGINT lands on cargo-val alone");
    screen.extend(read_until_hangup(&master, child.id()).await);
    let status = wait::wait_within(&child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not end after the signal: {failure}"));

    assert_eq!(status.code(), Some(130), "{:?}", String::from_utf8_lossy(&screen));
    assert!(contains(&screen, HIDE_CURSOR), "the frame had hidden the cursor:\n{:?}", String::from_utf8_lossy(&screen));
    assert!(rposition(&screen, SHOW_CURSOR) > rposition(&screen, HIDE_CURSOR), "the cursor is restored after the interrupt:\n{:?}", String::from_utf8_lossy(&screen));
}

#[proxima::test(timeout = "310s")]
async fn plain_through_a_pipe_emits_zero_escape_bytes_on_both_streams() {
    let output = run(&[], &["--output", "plain"]).await;

    assert_eq!(output.stdout.iter().chain(&output.stderr).filter(|byte| **byte == ESCAPE).count(), 0);
    assert!(stdout_of(&output).contains("SUMMARY  passed=1 failed=1"));
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
    let mut command = binary::command();
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

mod plain_grammar {
    use cargo_val::render::Outcome;

    use crate::plain::{names, settled};
    use crate::plain_summary::summary;

    const RUN: &str = "cargo-val 0.1.0  process=test  jobs=10\n\
        START    cli_run  fixture::passes\n\
        IGNORED  cli_run  fixture::ignored_by_default\n\
        PASS     cli_run  fixture::passes  0.003s\n\
        FAIL     doc  src/lib.rs - subtract (line 12)  1.250s  (solo rerun)\n\
        FAILED   cli_run fixture::passes\n\
        SUMMARY  passed=1 failed=1 ignored=1 not_run=0  wall=0.409s\n";

    #[test]
    fn settled_parses_binary_test_and_outcome_and_skips_start_and_failed_lines() {
        assert_eq!(
            settled(RUN),
            [
                ("cli_run".to_owned(), "fixture::ignored_by_default".to_owned(), Outcome::Ignored),
                ("cli_run".to_owned(), "fixture::passes".to_owned(), Outcome::Passed),
                ("doc".to_owned(), "src/lib.rs - subtract (line 12)".to_owned(), Outcome::Failed),
            ]
        );
    }

    #[test]
    fn names_lists_one_outcome_in_print_order() {
        assert_eq!(names(RUN, Outcome::Passed), ["fixture::passes"]);
        assert_eq!(names(RUN, Outcome::Failed), ["src/lib.rs - subtract (line 12)"]);
    }

    #[test]
    fn summary_reads_the_key_value_counts() {
        let counts = summary(RUN).expect("the run printed a SUMMARY line");

        assert_eq!(counts["passed"], 1);
        assert_eq!(counts["not_run"], 0);
        assert!(summary("START    cli_run  fixture::passes\n").is_none());
    }
}

#[proxima::test(timeout = "310s")]
async fn a_failure_prints_the_seven_line_source_excerpt_with_a_marker_and_a_caret_row_and_no_escape_byte() {
    let output = run(&[], &[]).await;
    let text = stdout_of(&output);

    let expected = [
        "------ source: cli_run fixture::fails_when_requested ------",
        "     9 |             print!(\"{}\", \"X\".repeat(5000));",
        "    10 |         }",
        "    11 |         if std::env::var_os(\"CARGO_VAL_C52D_FAIL\").is_some() {",
        ">   12 |             panic!(\"requested fixture failure\");",
        "      |             ^^^^^ panicked here",
        "    13 |         }",
        "    14 |         if std::env::var_os(\"CARGO_VAL_C52D_ESCAPES\").is_some() {",
        "    15 |             panic!(\"path \\\"C:\\\\tmp\\\\out\\\" is missing\\nsecond line\");",
        "------ end source ------",
    ]
    .join("\n");
    assert!(text.contains(&expected), "the excerpt block is exact:\n{text}");
    assert_eq!(text.lines().filter(|line| line.starts_with("> ")).count(), 1, "{text}");
    assert!(!output.stdout.contains(&ESCAPE), "plain output carries no escape byte");
}
