#![cfg(unix)]

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::num::NonZeroI32;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_os = "linux")]
use cargo_val::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
use cargo_val::edge_exit_macos::MacosExit as ExitSource;
use cargo_val::edge_stream_unix::ChildStream;
use futures::future::join;
use proxima_primitives::pipe::Pipe;

#[path = "common/binary.rs"]
mod binary;
#[path = "common/drain.rs"]
mod drain;
#[path = "common/group.rs"]
mod group;
#[path = "common/pty.rs"]
mod pty;

use pty::{Pty, open_pty};

const FRAME_START: &str = "\x1b[?25l";
static NEXT_RUN: AtomicUsize = AtomicUsize::new(0);
const REVIEW_HEADER: &str = "cargo val  review";
const SHOW_CURSOR: &str = "\x1b[?25h";
const CLOSING_START: &str = "cargo_val_rich_pty_fail_fixture: discovered";

fn fixture_manifest(fixture: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture).join("Cargo.toml")
}

fn spawn_on(slave: &OwnedFd, stdin: Stdio, fixture: &str, mode: &str, (release, run): (&Path, &str), environment: &[(&str, &str)]) -> std::process::Child {
    let stream = || Stdio::from(slave.try_clone().expect("the slave descriptor duplicates"));
    let mut command = binary::command();
    // the slave becomes the controlling terminal, so a background-group child reading stdin would get SIGTTIN here as it does in a shell
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY.into(), 0);
            Ok(())
        });
    }
    command
        .arg("--manifest-path")
        .arg(fixture_manifest(fixture))
        .args(["--tests", "--no-fail-fast", "--output", mode])
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env_remove("NO_COLOR")
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .env_remove("TERM_PROGRAM")
        .env_remove("CARGO_VAL_EDITOR_URL")
        .envs(environment.iter().copied())
        .env("CARGO_VAL_FIXTURE_RELEASE_FILE", release)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-target-{fixture}-{run}")))
        .stdin(stdin)
        .stdout(stream())
        .stderr(stream())
        .spawn()
        .expect("cargo-val is spawnable on a pty")
}

fn count_before_slash(frame: &str, is_row: impl Fn(&str) -> bool) -> Option<usize> {
    let row = frame.lines().find(|line| is_row(line))?;
    let before = &row[..row.find('/')?];
    let digits: String = before.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse().ok()
}

fn built_count(frame: &str) -> Option<usize> {
    count_before_slash(frame, |line| line.contains("compile units"))
}

fn settled_count(frame: &str) -> Option<usize> {
    count_before_slash(frame, |line| line.starts_with(" test  "))
}

fn clock_shows_elapsed(frame: &str) -> bool {
    let header = frame.lines().find(|line| line.contains("cargo val"));
    header.and_then(|line| line.split_whitespace().last()).is_some_and(|stamp| stamp != "0:00")
}

fn live_frames(output: &str) -> Vec<&str> {
    output.split(FRAME_START).skip(1).filter(|frame| frame.contains("compile units")).collect()
}

fn shown(frame: &str) -> String {
    frame.replace('\x1b', "\\e").replace('\r', "")
}

fn released_frame(frame: &str) -> bool {
    clock_shows_elapsed(frame) && settled_count(frame).is_some_and(|settled| settled > 0)
}

fn without_sgr(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\x1b[") {
        let tail = &rest[at + 2..];
        let end = tail.find(|character: char| character.is_ascii_alphabetic()).map_or(tail.len(), |found| found + 1);
        let sgr = tail[..end].ends_with('m');
        plain.push_str(if sgr { &rest[..at] } else { &rest[..at + 2 + end] });
        rest = &tail[end..];
    }
    plain.push_str(rest);
    plain
}

async fn read_until_exit(master: &ChildStream, release: &Path) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| {
        let screen = without_sgr(&String::from_utf8_lossy(bytes));
        live_frames(&screen).iter().any(|frame| released_frame(frame))
    }).await;
    File::create(release).expect("the release file is creatable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

async fn run_on_pty(fixture: &str) -> (String, ExitStatus) {
    run_on_pty_with(fixture, &NO_COLOR).await
}

async fn run_on_pty_with(fixture: &str, environment: &[(&str, &str)]) -> (String, ExitStatus) {
    run_driven(fixture, "rich", environment, async |master, _keys, release, _leader| read_until_exit(master, &release).await).await
}

const NO_COLOR: [(&str, &str); 1] = [("NO_COLOR", "1")];

async fn run_driven<Drive>(fixture: &str, mode: &str, environment: &[(&str, &str)], drive: Drive) -> (String, ExitStatus)
where
    Drive: AsyncFnOnce(&ChildStream, File, PathBuf, u32) -> String,
{
    run_with(fixture, mode, environment, true, drive).await
}

fn assert_line_discipline_restored(pty: &File, fixture: &str, mode: &str) {
    let mut attributes: libc::termios = unsafe { core::mem::zeroed() };
    let queried = unsafe { libc::tcgetattr(pty.as_raw_fd(), &raw mut attributes) };
    assert_eq!(queried, 0, "tcgetattr on the pty: {}", std::io::Error::last_os_error());
    let wanted = libc::ICANON | libc::ECHO;
    assert_eq!(attributes.c_lflag & wanted, wanted, "{fixture} {mode}: ICANON and ECHO must be set again after the run, c_lflag={:#x}", attributes.c_lflag);
}

async fn run_with<Drive>(fixture: &str, mode: &str, environment: &[(&str, &str)], tty_stdin: bool, drive: Drive) -> (String, ExitStatus)
where
    Drive: AsyncFnOnce(&ChildStream, File, PathBuf, u32) -> String,
{
    let target_run = format!("{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed));
    run_targeting(fixture, mode, environment, (tty_stdin, &target_run), drive).await
}

async fn run_targeting<Drive>(fixture: &str, mode: &str, environment: &[(&str, &str)], (tty_stdin, target_run): (bool, &str), drive: Drive) -> (String, ExitStatus)
where
    Drive: AsyncFnOnce(&ChildStream, File, PathBuf, u32) -> String,
{
    let run = format!("{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed));
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-release-{fixture}-{mode}-{run}"));
    std::fs::create_dir_all(&directory).expect("the release directory is creatable");
    let release = directory.join("release");
    let Pty { master, slave } = open_pty();
    let keys = master.try_clone().expect("the master descriptor duplicates for writing keys");
    let probe = master.try_clone().expect("the master descriptor duplicates for the termios query");
    let stdin = if tty_stdin { Stdio::from(slave.try_clone().expect("the slave descriptor duplicates")) } else { Stdio::piped() };
    let mut child = spawn_on(&slave, stdin, fixture, mode, (&release, target_run), environment);
    drop(child.stdin.take());
    drop(slave);
    let leader = child.id();
    let exit = ExitSource::new(NonZeroI32::new(leader.cast_signed()).expect("a spawned child has a nonzero pid"));
    // boxed: the stream owns a capture window and scratch tail, together ~40 KB, which would make every test future that awaits a run trip clippy::large_futures
    let master = Box::new(ChildStream::new(OwnedFd::from(master)));
    let waiting = join(drive(&master, keys, release, leader), exit.call(()));
    let (output, exited) = match group::bounded(leader, group::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE", waiting).await {
        Ok(finished) => finished,
        Err((report, _)) => panic!("{report}"),
    };
    let status = exited.expect("the exit source is unspent").expect("cargo-val exit status reads");
    let _ = child.try_wait();
    if tty_stdin {
        assert_line_discipline_restored(&probe, fixture, mode);
    }
    (output, status)
}

async fn release_then_quit(master: &ChildStream, mut keys: File, release: PathBuf) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| {
        let screen = without_sgr(&String::from_utf8_lossy(bytes));
        live_frames(&screen).iter().any(|frame| released_frame(frame))
    }).await;
    File::create(release).expect("the release file is creatable");
    let (review, _) = drain::drain_until(master, |bytes| without_sgr(&String::from_utf8_lossy(bytes)).contains(REVIEW_HEADER)).await;
    output.extend_from_slice(&review);
    keys.write_all(b"qq").expect("the quit keys are writable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

async fn run_failing_on_pty(environment: &[(&str, &str)]) -> (String, ExitStatus) {
    run_driven("rich_pty", "rich", environment, async |master, keys, release, _leader| release_then_quit(master, keys, release).await).await
}

async fn quit_from_review(master: &ChildStream, mut keys: File) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    assert!(!output.windows(7).any(|window| window == b"SUMMARY"), "the receipt printed before any key was sent:\n{}", shown(&String::from_utf8_lossy(&output)));
    keys.write_all(b"q").expect("the first quit key is writable");
    keys.write_all(b"q").expect("the confirming quit key is writable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

async fn read_plain(master: &ChildStream) -> String {
    let (output, _) = drain::drain_until(master, |_| false).await;
    String::from_utf8_lossy(&output).into_owned()
}

fn without_wall_values(text: &str) -> String {
    let blanked = |line: &str| {
        let marker = ["wall=", "wall "].iter().filter_map(|marker| line.find(marker).map(|at| at + marker.len())).min();
        marker.map_or_else(
            || line.to_owned(),
            |value| {
                let tail = &line[value..];
                format!("{}{}", &line[..value], &tail[tail.find(char::is_whitespace).unwrap_or(tail.len())..])
            },
        )
    };
    text.replace('\r', "").lines().map(blanked).collect::<Vec<_>>().join("\n")
}

const LISTED_LINE: &str = "LISTED   binaries=1  tests=2\n";

#[proxima::test]
async fn a_later_live_frame_shows_nonzero_built_and_settled_counts() {
    let (output, status) = run_on_pty("rich_pty").await;

    let frames = live_frames(&output);
    println!("first frame:\n{}", shown(frames.first().expect("a live frame was drawn")));
    println!("middle frame:\n{}", shown(frames[frames.len() / 2]));
    println!("last live frame:\n{}", shown(frames.last().expect("a live frame was drawn")));
    assert!(status.success(), "cargo-val exited {status}");
    let later = frames.iter().find(|frame| built_count(frame).is_some_and(|built| built > 0) && settled_count(frame).is_some_and(|settled| settled > 0));
    assert!(later.is_some(), "no live frame showed a nonzero built count with a nonzero settled count among {} frames", frames.len());
}

#[proxima::test]
async fn a_later_live_frame_clock_shows_elapsed_time_from_the_run_start() {
    let (output, status) = run_on_pty("rich_pty").await;

    let frames = live_frames(&output);
    assert!(status.success(), "cargo-val exited {status}");
    let clocks: Vec<&str> = frames.iter().filter_map(|frame| frame.lines().find(|line| line.contains("cargo val"))).filter_map(|line| line.split_whitespace().last()).collect();
    assert!(frames.iter().any(|frame| clock_shows_elapsed(frame)), "every one of {} frames read 0:00: {clocks:?}", frames.len());
}

#[proxima::test]
async fn a_fresh_build_prints_no_cargo_status_lines_into_or_between_frames() {
    let (output, status) = run_on_pty("rich_pty").await;

    assert!(status.success(), "cargo-val exited {status}");
    assert!(!live_frames(&output).is_empty(), "no live frame was drawn");
    for status_word in ["Compiling", "Finished", "Fresh", "Blocking"] {
        assert!(!output.contains(status_word), "cargo's `{status_word}` line leaked into the pty output:\n{}", shown(&output));
    }
}

#[proxima::test]
async fn a_failed_build_still_prints_rustcs_diagnostic_and_cargos_summary_after_the_frame_is_erased() {
    let (output, status) = run_on_pty("build_error").await;

    assert!(!status.success(), "a build error must fail the run");
    let diagnostic = output.find("error[E0308]: mismatched types").unwrap_or_else(|| panic!("rustc's E0308 line is missing:\n{}", shown(&output)));
    let summary = output.find("error: could not compile").unwrap_or_else(|| panic!("cargo's summary line is missing:\n{}", shown(&output)));
    let failed = output.find("cargo-val: cargo test build failed").unwrap_or_else(|| panic!("the build-failed line is missing:\n{}", shown(&output)));
    assert!(diagnostic < summary && summary < failed, "diagnostics out of order:\n{}", shown(&output));
    for status_word in ["Compiling", "Finished"] {
        assert!(!output.contains(status_word), "cargo's `{status_word}` line leaked:\n{}", shown(&output));
    }
}

const FAILING_RUN: (&str, &str) = ("CARGO_VAL_FIXTURE_FAIL", "1");
const ERASE_FRAME: &str = "\x1b[J";
const SOURCE_OPEN: &str = "------ source: ";
const SOURCE_CLOSE: &str = "------ end source ------";
#[cfg(feature = "highlight")]
const KEYWORD_SGR: &str = "\x1b[35mif\x1b[0m";

fn failure_source_block(output: &str) -> &str {
    let open = output.rfind(SOURCE_OPEN).unwrap_or_else(|| panic!("no source fence after the frame:\n{}", shown(output)));
    let erased = output.rfind(ERASE_FRAME).expect("the live frame was erased");
    assert!(open < erased, "the source block must survive the final frame erase:\n{}", shown(output));
    let close = open + output[open..].find(SOURCE_CLOSE).unwrap_or_else(|| panic!("no closing fence:\n{}", shown(output)));
    &output[open..close + SOURCE_CLOSE.len()]
}

#[proxima::test]
async fn after_the_frame_is_erased_the_failure_block_has_the_source_fence_and_the_marker_row() {
    let (output, status) = run_failing_on_pty(&[FAILING_RUN, VISUAL_CODE, ("NO_COLOR", "1")]).await;

    assert!(!status.success(), "a failing test must fail the run");
    let block = failure_source_block(&output);
    assert!(block.starts_with("------ source: cargo_val_rich_pty_fixture tests::fails_when_requested ------"), "{}", shown(block));
    let marker_rows: Vec<&str> = block.lines().filter(|line| line.starts_with("> ")).collect();
    assert_eq!(marker_rows.len(), 1, "{}", shown(block));
    assert!(marker_rows[0].contains("panic!(\"requested fixture failure\");"), "{}", shown(block));
}

#[proxima::test]
async fn with_no_color_set_the_failure_source_block_has_zero_escape_bytes() {
    let (output, _) = run_failing_on_pty(&[FAILING_RUN, VISUAL_CODE, ("NO_COLOR", "1")]).await;

    let block = failure_source_block(&output);

    assert_eq!(block.bytes().filter(|byte| *byte == 0x1b).count(), 0, "{}", shown(block));
}

async fn release_open_detail_quit(master: &ChildStream, mut keys: File, release: PathBuf) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| {
        let screen = without_sgr(&String::from_utf8_lossy(bytes));
        live_frames(&screen).iter().any(|frame| released_frame(frame))
    }).await;
    File::create(release).expect("the release file is creatable");
    let (review, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    output.extend_from_slice(&review);
    let opened_at = output.len();
    keys.write_all(b"\r").expect("the open key is writable");
    let (detail, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(EXCERPT_MARKER)).await;
    output.extend_from_slice(&detail);
    assert!(opened_at < output.len(), "the detail screen drew after the open key");
    keys.write_all(b"qq").expect("the quit keys are writable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

const EXCERPT_MARKER: &str = "panic!(\"requested fixture failure\");";

#[proxima::test]
async fn with_no_color_set_the_pinned_and_review_detail_excerpt_rows_have_zero_escape_bytes() {
    let environment = [FAILING_RUN, VISUAL_CODE, ("NO_COLOR", "1")];
    let (output, _) = run_driven("rich_pty", "rich", &environment, async |master, keys, release, _leader| release_open_detail_quit(master, keys, release).await).await;

    let lines: Vec<&str> = output.split('\n').collect();
    let marker_lines: Vec<usize> = lines.iter().enumerate().filter(|(_, line)| line.contains(EXCERPT_MARKER)).map(|(index, _)| index).collect();
    let rows: Vec<&str> = marker_lines.iter().flat_map(|index| [lines[index - 1], lines[*index], lines[index + 1]]).filter(|line| line.contains(" | ")).collect();

    assert!(marker_lines.len() >= 3, "a pinned frame, the review detail and the closing block each carried the marker row, got {}:\n{}", marker_lines.len(), shown(&output));
    assert!(rows.len() >= 6, "the marker rows and their neighbours were scanned, got {}", rows.len());
    let escaped: Vec<String> = rows.iter().filter(|row| row.bytes().any(|byte| byte == 0x1b)).map(|row| shown(row)).collect();
    assert!(escaped.is_empty(), "excerpt rows with escape bytes: {escaped:?}");
}

#[cfg(feature = "highlight")]
#[proxima::test]
async fn with_highlight_and_colour_on_the_failure_source_block_colours_the_if_keyword_with_the_keyword_role() {
    let (output, _) = run_failing_on_pty(&[FAILING_RUN]).await;

    let block = failure_source_block(&output);

    assert!(block.contains(KEYWORD_SGR), "{}", shown(block));
}

const BUILD_ERROR_MARKER: &str = ">    4 |     \"forty-two\"";
const EXCERPT_ROWS_BEFORE_MARKER: usize = 3;
const EXCERPT_ROWS_AFTER_MARKER: usize = 4;

fn raw_excerpt_lines(output: &str) -> Vec<String> {
    let lines: Vec<String> = output.replace('\r', "").lines().map(ToOwned::to_owned).collect();
    let marker = lines
        .iter()
        .rposition(|line| without_sgr(line) == BUILD_ERROR_MARKER)
        .unwrap_or_else(|| panic!("no marker row {BUILD_ERROR_MARKER:?}:\n{}", shown(output)));
    lines[marker - EXCERPT_ROWS_BEFORE_MARKER..=marker + EXCERPT_ROWS_AFTER_MARKER].to_vec()
}

fn screen_lines(output: &str) -> Vec<String> {
    without_sgr(&output.replace('\r', "")).lines().map(ToOwned::to_owned).collect()
}

fn line_at(lines: &[String], text: &str) -> usize {
    lines.iter().position(|line| line.ends_with(text)).unwrap_or_else(|| panic!("no line {text:?} in:\n{lines:#?}"))
}

#[proxima::test]
async fn a_rich_build_error_replaces_rustcs_snippet_with_the_excerpt_between_the_header_and_the_label() {
    let (output, status) = run_on_pty("build_error").await;

    assert_eq!(status.code(), Some(3), "{}", shown(&output));
    let lines = screen_lines(&output);
    let header = line_at(&lines, "error[E0308]: mismatched types");
    let expected = [
        " --> src/lib.rs:4:5",
        "     1 | pub fn answer() -> u32 {",
        "     2 |     /// scale applied before the answer is reported",
        "     3 |     let scale = 6;",
        BUILD_ERROR_MARKER,
        "      |     ^^^^^^^^^^^ expected `u32`, found `&str`",
        "     5 | }",
        "     6 |",
        "     7 | #[cfg(test)]",
    ];
    assert_eq!(lines[header + 1..=header + expected.len()], expected, "{}", shown(&output));
    for rustc_row in ["1 | pub fn answer() -> u32 {", "4 |     \"forty-two\"", "..."] {
        assert!(!lines.iter().any(|line| line == rustc_row), "rustc's own snippet row {rustc_row:?} survived:\n{}", shown(&output));
    }
    assert!(!lines.iter().any(|line| line.contains("--- expected `u32` because of return type")), "{}", shown(&output));
    let at_summary = lines.iter().position(|line| line.starts_with("error: could not compile")).expect("cargo's summary is printed");
    assert!(header + expected.len() < at_summary, "{}", shown(&output));
}

#[proxima::test]
async fn a_rich_build_error_keeps_the_separator_note_and_help_blocks_after_the_excerpt_verbatim() {
    let (output, status) = run_on_pty("build_error_help").await;

    assert_eq!(status.code(), Some(3), "{}", shown(&output));
    let lines = screen_lines(&output);
    let header = line_at(&lines, "error[E0382]: use of moved value: `text`");
    let caret = format!("      | {}^^^^ value used here after move", " ".repeat(20));
    let head = [" --> src/lib.rs:8:21", "     3 | }", "     4 |", "     5 | pub fn twice() -> usize {", "     6 |     let text = String::from(\"forty-two\");", "     7 |     let first = consume(text);", ">    8 |     first + consume(text)", caret.as_str(), "     9 | }"];
    assert_eq!(lines[header + 1..=header + head.len()], head, "{}", shown(&output));
    let tail = [
        "  |",
        "note: consider changing this parameter type in function `consume` to borrow instead if owning the value isn't necessary",
        " --> src/lib.rs:1:22",
        "  |",
        "1 | pub fn consume(text: String) -> usize {",
        "  |        -------       ^^^^^^ this parameter takes ownership of the value",
        "  |        |",
        "  |        in this function",
        "help: consider cloning the value if the performance cost is acceptable",
        "  |",
        "7 |     let first = consume(text.clone());",
        "  |                             ++++++++",
    ];
    let after = header + head.len() + 1;
    assert_eq!(lines[after..after + tail.len()], tail, "{}", shown(&output));
}

#[proxima::test]
async fn with_no_color_set_the_build_error_excerpt_rows_have_zero_escape_bytes() {
    let (output, _) = run_on_pty("build_error").await;

    let rows = raw_excerpt_lines(&output);

    assert_eq!(rows.len(), EXCERPT_ROWS_BEFORE_MARKER + 1 + EXCERPT_ROWS_AFTER_MARKER);
    assert_eq!(rows.iter().flat_map(|row| row.bytes()).filter(|byte| *byte == 0x1b).count(), 0, "{rows:?}");
}

#[cfg(feature = "highlight")]
#[proxima::test]
async fn with_highlight_and_colour_on_the_build_error_excerpt_colours_the_keyword_and_reads_the_same_without_sgr() {
    let (output, _) = run_on_pty_with("build_error", &[]).await;

    let rows = raw_excerpt_lines(&output);

    assert!(rows[0].contains("\x1b[35mfn\x1b[0m"), "{}", shown(&rows[0]));
    assert_eq!(without_sgr(&rows[0]), "     1 | pub fn answer() -> u32 {");
    assert_eq!(without_sgr(&rows[3]), BUILD_ERROR_MARKER);
}

#[proxima::test]
async fn quitting_review_with_q_then_q_restores_the_terminal_and_prints_the_plain_closing_block() {
    let (output, status) = run_driven("rich_pty_fail", "rich", &NO_COLOR, async |master, keys, _release, _leader| quit_from_review(master, keys).await).await;
    let (plain, plain_status) = run_driven("rich_pty_fail", "plain", &NO_COLOR, async |master, _keys, _release, _leader| read_plain(master).await).await;

    let last_frame = output.rfind(REVIEW_HEADER).unwrap_or_else(|| panic!("no review frame was drawn:\n{}", shown(&output)));
    let shown_cursor = output.rfind(SHOW_CURSOR).unwrap_or_else(|| panic!("the cursor was never shown again:\n{}", shown(&output)));
    println!("last review frame onward:\n{}", shown(&output[output[..last_frame].rfind('\x1b').unwrap_or(last_frame)..]));
    let review_frame = &output[last_frame..output.len().min(last_frame + output[last_frame..].find("\x1b[J").unwrap_or(output.len() - last_frame))];
    assert!(review_frame.contains("expected three listeners"), "the last review frame must show the fixture's panic message:\n{}", shown(review_frame));
    assert_eq!(status.code(), Some(1), "a run with a failure exits 1 after leaving review: {status}");
    assert_eq!(plain_status.code(), Some(1), "the plain run of the same fixture exits 1: {plain_status}");
    assert!(shown_cursor > last_frame, "the cursor was shown at {shown_cursor}, before the last review frame at {last_frame}");
    let receipt = output.rfind("SUMMARY").unwrap_or_else(|| panic!("no receipt was printed:\n{}", shown(&output)));
    assert!(receipt < shown_cursor, "the cursor returns only after the receipt: receipt at {receipt}, cursor at {shown_cursor}");
    assert!(output.ends_with(SHOW_CURSOR), "the last bytes of the run show the cursor:\n{}", shown(&output));
    let after_frame = output.rsplit_once("\x1b[J").map_or("", |(_, tail)| tail);
    let after_frame = after_frame.strip_suffix(SHOW_CURSOR).unwrap_or(after_frame);
    assert!(after_frame.starts_with(CLOSING_START), "the bytes after the last frame erase must open the closing block:\n{}", shown(after_frame));
    let plain_receipt = plain.replace('\r', "");
    let plain_receipt = &plain_receipt[plain_receipt.find(CLOSING_START).expect("the plain run prints the closing block")..];
    assert!(plain_receipt.contains(LISTED_LINE), "plain no longer prints the LISTED line:\n{plain_receipt}");
    assert!(!after_frame.contains("LISTED"), "rich must not print the LISTED line:\n{}", shown(after_frame));
    assert_eq!(without_wall_values(after_frame), without_wall_values(&plain_receipt.replacen(LISTED_LINE, "", 1)), "rich receipt differs from the plain closing block after the final erase");
}

async fn interrupt_in_review(master: &ChildStream, mut keys: File, leader: u32, via_key: bool) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    if via_key {
        keys.write_all(b"c").expect("the interrupt key is writable");
    } else {
        group::interrupt(leader).await.expect("SIGINT reaches the process group");
    }
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

fn assert_interrupted_exit(output: &str, status: ExitStatus) {
    let last_frame = output.rfind(REVIEW_HEADER).unwrap_or_else(|| panic!("no review frame was drawn:\n{}", shown(output)));
    let shown_cursor = output.rfind(SHOW_CURSOR).unwrap_or_else(|| panic!("the cursor was never shown again:\n{}", shown(output)));
    assert_eq!(status.code(), Some(130), "an interrupted review exits 130: {status}\n{}", shown(output));
    assert!(shown_cursor > last_frame, "the cursor was shown at {shown_cursor}, before the last review frame at {last_frame}");
    let block = output.rfind("INTERRUPTED  signal=2  forwarded_groups=0").unwrap_or_else(|| panic!("no interrupted block with its group count:\n{}", shown(output)));
    assert!(block > last_frame, "the interrupted block at {block} must follow the last review frame at {last_frame}");
    let tail = &output[block..];
    assert!(!tail.contains("RUNNING"), "a post-run review has no in-flight test to name:\n{}", shown(tail));
    let summary = tail.find("SUMMARY  ").unwrap_or_else(|| panic!("the interrupted block has no summary:\n{}", shown(tail)));
    assert!(summary > 0 && shown_cursor > block + summary, "the cursor at {shown_cursor} must come after the summary at {}", block + summary);
    assert!(output.ends_with(SHOW_CURSOR), "the cursor is the last thing shown:\n{}", shown(output));
}

#[proxima::test]
async fn a_green_rich_run_on_a_terminal_never_shows_review_and_exits_zero() {
    let (output, status) = run_on_pty("rich_pty").await;

    assert!(status.success(), "a green run exits 0: {status}");
    assert!(!output.contains(REVIEW_HEADER), "a green run must not open review:\n{}", shown(&output));
}

#[proxima::test]
async fn a_failing_rich_run_with_piped_stdin_prints_the_closing_block_without_review_and_exits_one() {
    let (output, status) = run_with("rich_pty_fail", "rich", &NO_COLOR, false, async |master, _keys, _release, _leader| read_plain(master).await).await;

    assert_eq!(status.code(), Some(1), "a failing run exits 1: {status}");
    assert!(!output.contains(REVIEW_HEADER), "no review without a terminal on stdin:\n{}", shown(&output));
    assert!(output.contains("SUMMARY"), "the closing block must print:\n{}", shown(&output));
}

#[proxima::test]
async fn pressing_c_in_review_exits_130_with_the_interrupted_block_after_the_terminal_is_restored() {
    let (output, status) = run_driven("rich_pty_fail", "rich", &NO_COLOR, async |master, keys, _release, leader| interrupt_in_review(master, keys, leader, true).await).await;

    assert_interrupted_exit(&output, status);
}

#[proxima::test]
async fn sigint_sent_to_the_process_group_during_review_exits_130_with_the_terminal_restored() {
    let (output, status) = run_driven("rich_pty_fail", "rich", &NO_COLOR, async |master, keys, _release, leader| interrupt_in_review(master, keys, leader, false).await).await;

    assert_interrupted_exit(&output, status);
}

fn csi_final(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> (String, char) {
    let mut parameters = String::new();
    for next in characters.by_ref() {
        if ('@'..='~').contains(&next) {
            return (parameters, next);
        }
        parameters.push(next);
    }
    (parameters, ' ')
}

fn final_scrollback(output: &str) -> Vec<String> {
    let (mut lines, mut row) = (vec![String::new()], 0_usize);
    let mut characters = output.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' if characters.peek() == Some(&'[') => {
                characters.next();
                match csi_final(&mut characters) {
                    (count, 'A') => row = row.saturating_sub(count.parse().unwrap_or(1)),
                    (_, 'J') => {
                        lines[row].clear();
                        lines.truncate(row + 1);
                    }
                    _ => {}
                }
            }
            '\n' => {
                row += 1;
                if row == lines.len() {
                    lines.push(String::new());
                }
            }
            '\r' | '\x1b' => {}
            other => lines[row].push(other),
        }
    }
    lines
}

#[proxima::test]
async fn the_final_scrollback_holds_the_excerpt_once_and_no_pinned_row_after_the_closing_block() {
    let (output, status) = run_failing_on_pty(&[FAILING_RUN, VISUAL_CODE, ("NO_COLOR", "1")]).await;

    let scrollback = final_scrollback(&output);
    let marker = "panic!(\"requested fixture failure\");";
    let drawn_in_all_bytes = output.matches(marker).count();
    let kept: Vec<&String> = scrollback.iter().filter(|line| line.contains(marker)).collect();
    let summary = scrollback.iter().position(|line| line.starts_with("SUMMARY")).unwrap_or_else(|| panic!("no closing block in:\n{}", shown(&output)));
    assert!(!status.success(), "a failing test must fail the run");
    assert!(drawn_in_all_bytes >= 2, "the block and at least one live pinned frame carried the row, {drawn_in_all_bytes} times:\n{}", shown(&output));
    assert_eq!(kept.len(), 1, "the settled scrollback keeps the excerpt row once: {kept:?}\n{}", shown(&scrollback.join("\n")));
    assert!(kept[0].starts_with("> "), "the surviving row is the printed block's marker row: {:?}", kept[0]);
    assert!(scrollback[summary..].iter().all(|line| !line.contains(marker) && !line.starts_with(" FAIL 1")), "{}", shown(&scrollback.join("\n")));
    assert!(scrollback.iter().all(|line| !line.starts_with(" FAIL 1")), "no pinned row survives the erase:\n{}", shown(&scrollback.join("\n")));
}

const TIMELINE_HEADER: &str = "cargo val  timeline";

async fn leave_a_sub_screen_with_a_lone_escape(master: &ChildStream, mut keys: File) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    keys.write_all(b"s").expect("the timeline key is writable");
    let (timeline, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(TIMELINE_HEADER)).await;
    output.extend_from_slice(&timeline);
    keys.write_all(b"\x1b").expect("the lone escape is writable");
    let (back, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    output.extend_from_slice(&back);
    keys.write_all(b"qq").expect("the quit keys are writable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

#[proxima::test]
async fn a_lone_escape_in_a_post_run_sub_screen_returns_to_review_on_the_next_tick_without_another_key() {
    let (output, status) = run_driven("rich_pty_fail", "rich", &NO_COLOR, async |master, keys, _release, _leader| leave_a_sub_screen_with_a_lone_escape(master, keys).await).await;

    let timeline = output.rfind(TIMELINE_HEADER).unwrap_or_else(|| panic!("the timeline screen was never drawn:\n{}", shown(&output)));
    let review_after = output[timeline..].find(REVIEW_HEADER).unwrap_or_else(|| panic!("review was not redrawn after the lone escape:\n{}", shown(&output[timeline..])));
    assert_eq!(status.code(), Some(1), "a run with a failure exits 1 after leaving review: {status}");
    assert!(review_after > 0, "review returned after the timeline frame, {review_after} bytes later");
    let receipt = output.rfind("SUMMARY").unwrap_or_else(|| panic!("no receipt was printed:\n{}", shown(&output)));
    let after_receipt = &output[receipt..];
    assert!(!after_receipt.contains(FRAME_START) && !after_receipt.contains(REVIEW_HEADER) && !after_receipt.contains(TIMELINE_HEADER), "frame bytes followed the receipt:\n{}", shown(after_receipt));
}

fn test_row_count(frame: &str) -> Option<String> {
    let row = frame.lines().find(|line| line.starts_with(" test  "))?;
    row.split_whitespace().find(|word| word.contains('/')).map(str::to_owned)
}

#[proxima::test]
async fn a_second_run_opens_with_the_first_runs_test_count_as_an_estimate_and_ends_exact() {
    let target_run = format!("{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed));
    let (first, first_status) = run_targeting("rich_pty", "rich", &NO_COLOR, (true, &target_run), async |master, _keys, release, _leader| read_until_exit(master, &release).await).await;
    let (second, second_status) = run_targeting("rich_pty", "rich", &NO_COLOR, (true, &target_run), async |master, _keys, release, _leader| read_until_exit(master, &release).await).await;

    assert!(first_status.success() && second_status.success(), "cargo-val exited {first_status} then {second_status}");
    let first_frames = live_frames(&first);
    let exact = test_row_count(first_frames.last().expect("the first run drew a frame")).expect("the first run's last frame has a test row");
    let total = exact.split_once('/').expect("the count is done/total").1;
    assert!(!total.starts_with('~'), "the first run's last frame is exact: {exact}");
    let first_estimates: Vec<String> = first_frames.iter().filter_map(|frame| test_row_count(frame)).collect();
    println!("first run counts: {first_estimates:?}");
    let second_frames = live_frames(&second);
    let estimates: Vec<String> = second_frames.iter().filter_map(|frame| test_row_count(frame)).collect();
    let closing = test_row_count(second_frames.last().expect("the second run drew a frame")).expect("the second run's last frame has a test row");
    println!("second run counts {estimates:?} closes {closing}");
    assert!(estimates.iter().any(|count| count.ends_with(&format!("/~{total}"))), "no second-run frame showed the first run's count as an estimate: {estimates:?}");
    assert!(closing.ends_with(&format!("/{total}")), "the second run's last frame is exact: {closing}");
}

#[proxima::test]
async fn a_test_that_reads_stdin_under_a_controlling_terminal_completes_without_stopping() {
    let run = format!("{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed));
    let release = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-stdin-{run}"));
    let Pty { master, slave } = open_pty();
    let mut child = spawn_on(&slave, Stdio::from(slave.try_clone().expect("the slave descriptor duplicates")), "stdin_reader", "rich", (&release, &run), &NO_COLOR);
    drop(slave);
    let leader = child.id();
    let exit = ExitSource::new(NonZeroI32::new(leader.cast_signed()).expect("a spawned child has a nonzero pid"));
    // boxed: the stream owns a capture window and scratch tail, together ~40 KB, which would make every test future that awaits a run trip clippy::large_futures
    let master = Box::new(ChildStream::new(OwnedFd::from(master)));
    let waiting = join(drain::drain_until(&master, |_| false), exit.call(()));
    let (_, exited) = match group::bounded(leader, group::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE", waiting).await {
        Ok(finished) => finished,
        Err((report, _)) => panic!("{report}"),
    };
    let status = exited.expect("the exit source is unspent").expect("cargo-val exit status reads");
    let _ = child.try_wait();

    assert!(status.success(), "cargo-val exited {status} while its test child read stdin");
}

const OSC8_OPEN: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";
const VISUAL_CODE: (&str, &str) = ("VISUAL", "code");

fn panic_site_link(text: &str) -> String {
    let directory = fixture_manifest("rich_pty").parent().expect("the manifest has a directory").display().to_string();
    format!("{OSC8_OPEN}vscode://file{directory}/src/lib.rs:28:13\x1b\\{text}{OSC8_CLOSE}")
}

#[proxima::test]
async fn with_visual_code_the_closing_block_panic_location_is_one_osc8_link_to_the_line_and_column() {
    let (output, _) = run_failing_on_pty(&[FAILING_RUN, VISUAL_CODE]).await;

    let linked = format!("panicked at {}:", panic_site_link("src/lib.rs:28:13"));
    assert_eq!(output.matches(&linked).count(), 1, "{}", shown(&output));
    assert_eq!(output.matches(&panic_site_link("src/lib.rs:28:13")).count(), 1, "{}", shown(&output));
}

#[proxima::test]
async fn with_visual_code_the_review_detail_location_row_is_an_osc8_link_to_the_line_and_column() {
    let (output, _) = run_failing_on_pty(&[FAILING_RUN, VISUAL_CODE]).await;

    let linked = format!("{}\x1b[2m  e opens", panic_site_link("src/lib.rs:28"));
    assert!(output.matches(&linked).count() >= 1, "{}", shown(&output));
}

#[proxima::test]
async fn with_no_color_set_and_visual_code_the_whole_run_has_zero_osc8_bytes() {
    let (output, _) = run_failing_on_pty(&[FAILING_RUN, VISUAL_CODE, ("NO_COLOR", "1")]).await;

    assert_eq!(output.matches("\x1b]8").count(), 0, "{}", shown(&output));
    assert!(output.contains("panicked at src/lib.rs:28:13:"), "{}", shown(&output));
}

#[proxima::test]
async fn with_visual_code_the_rich_build_error_location_line_is_an_osc8_link_to_the_line_and_column() {
    let (output, status) = run_on_pty_with("build_error", &[VISUAL_CODE]).await;

    assert_eq!(status.code(), Some(3), "{}", shown(&output));
    let directory = fixture_manifest("build_error").parent().expect("the manifest has a directory").display().to_string();
    let linked = format!("{OSC8_OPEN}vscode://file{directory}/src/lib.rs:4:5\x1b\\src/lib.rs:4:5{OSC8_CLOSE}");
    assert_eq!(output.matches(&linked).count(), 1, "{}", shown(&output));
}

fn rerun_scratch(label: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-rerun-{label}-{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&directory).expect("the rerun scratch directory is creatable");
    directory
}

fn rerun_environment(scratch: &Path, hang: bool) -> Vec<(String, String)> {
    let path = |name: &str| scratch.join(name).to_string_lossy().into_owned();
    let hanging = [("CARGO_VAL_FIXTURE_HANG_STATE".to_owned(), path("hang.state")), ("CARGO_VAL_FIXTURE_HANG_PID".to_owned(), path("hang.pid"))];
    let mut environment = vec![
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("CARGO_VAL_FIXTURE_RUNS_LOG".to_owned(), path("runs.log")),
        ("CARGO_VAL_FIXTURE_ONCE_STATE".to_owned(), path("once.state")),
        ("CARGO_VAL_FIXTURE_EXE_FILE".to_owned(), path("exe.path")),
    ];
    if hang {
        environment.extend(hanging);
    }
    environment
}

async fn run_rerun_fixture<Drive>(environment: &[(String, String)], drive: Drive) -> (String, ExitStatus)
where
    Drive: AsyncFnOnce(&ChildStream, File, PathBuf, u32) -> String,
{
    let borrowed: Vec<(&str, &str)> = environment.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
    run_driven("rich_pty_rerun", "rich", &borrowed, drive).await
}

fn review_frames(output: &str) -> Vec<String> {
    let whole = |frame: &&str| frame.contains("r rerun") || frame.contains("any key cancels");
    without_sgr(output).split(REVIEW_HEADER).skip(1).filter(whole).map(str::to_owned).collect()
}

fn rerun_settled(bytes: &[u8]) -> bool {
    let frames = review_frames(&String::from_utf8_lossy(bytes));
    frames.iter().position(|frame| frame.contains("RERUN")).is_some_and(|first| frames[first..].iter().any(|frame| !frame.contains("RERUN")))
}

fn flaky_cleared(bytes: &[u8]) -> bool {
    review_frames(&String::from_utf8_lossy(bytes)).iter().any(|frame| frame.contains(" 3 failed  1 passed") && !frame.contains("RERUN"))
}

fn passed_row_shown(bytes: &[u8]) -> bool {
    review_frames(&String::from_utf8_lossy(bytes)).iter().any(|frame| frame.contains("PASS "))
}

async fn press_then_quit(master: &ChildStream, mut keys: File, pressed: &[u8], settled: fn(&[u8]) -> bool) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    keys.write_all(pressed).expect("the rerun key is writable");
    let (finished, _) = drain::drain_until(master, settled).await;
    output.extend_from_slice(&finished);
    keys.write_all(b"qq").expect("the quit keys are writable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

fn run_counts(scratch: &Path) -> Vec<String> {
    std::fs::read_to_string(scratch.join("runs.log")).expect("the fixture logged its runs").lines().map(str::to_owned).collect()
}

fn run_count_evidence(scratch: &Path, output: &str, status: ExitStatus) -> String {
    let log = std::fs::read_to_string(scratch.join("runs.log")).unwrap_or_else(|error| format!("<unreadable: {error}>"));
    let log_lines = log.lines().enumerate().map(|(index, line)| format!("  {index}: {line}")).collect::<Vec<_>>().join("\n");
    let review_open = output.find(REVIEW_HEADER).unwrap_or(0);
    let receipt = output.rfind("SUMMARY").map_or("<no receipt printed>", |at| &output[at..]);
    format!("run log ({} lines):\n{log_lines}\nexit status: {status}\nreceipt:\n{}\nframes from the review open (the key follows it) to the end:\n{}", log.lines().count(), shown(receipt), shown(&output[review_open..]))
}

fn assert_receipt_after_review(output: &str, status: ExitStatus) -> &str {
    assert_eq!(status.code(), Some(1), "a receipt with unresolved failures exits 1: {status}\n{}", shown(output));
    let receipt = output.rfind("SUMMARY").unwrap_or_else(|| panic!("no receipt was printed:\n{}", shown(output)));
    let shown_cursor = output.rfind(SHOW_CURSOR).unwrap_or_else(|| panic!("the cursor was never shown again:\n{}", shown(output)));
    assert!(receipt < shown_cursor, "the cursor returns only after the receipt: receipt at {receipt}, cursor at {shown_cursor}");
    assert!(output.ends_with(SHOW_CURSOR), "the last bytes of the run show the cursor:\n{}", shown(output));
    &output[receipt..]
}

#[proxima::test]
async fn r_in_review_reruns_the_selected_failure_with_a_rerun_row_then_its_result_and_review_stays_open() {
    let scratch = rerun_scratch("one");
    let mut environment = rerun_environment(&scratch, false);
    environment.retain(|(key, _)| key != "CARGO_VAL_FIXTURE_ONCE_STATE");
    let (output, status) = run_rerun_fixture(&environment, async |master, keys, _release, _leader| press_then_quit(master, keys, b"r", rerun_settled).await).await;

    let frames = review_frames(&output);
    let running = frames.iter().position(|frame| frame.contains("RERUN")).unwrap_or_else(|| panic!("no frame showed a RERUN row:\n{}", shown(&output)));
    println!("frame with the RERUN row (80 columns):\n{}", shown(&format!("{REVIEW_HEADER}{}", frames[running])));
    let settled = frames[running..].iter().find(|frame| !frame.contains("RERUN")).unwrap_or_else(|| panic!("review never left the RERUN state:\n{}", shown(&output)));
    assert!(settled.contains("ASSERT") || settled.contains("PANIC"), "a failing rerun leaves the row with its class:\n{}", shown(settled));
    let receipt = assert_receipt_after_review(&output, status);
    assert!(receipt.starts_with("SUMMARY  passed=0 failed=4 "), "one rerun that failed again leaves all 4 failures:\n{}", shown(receipt));
    assert_eq!(run_counts(&scratch).len(), 5, "4 first runs plus exactly 1 rerun\n{}", run_count_evidence(&scratch, &output, status));
}

#[proxima::test]
async fn capital_r_in_review_reruns_every_failure_and_the_receipt_counts_the_one_that_passed_as_flaky() {
    let scratch = rerun_scratch("all");
    let (output, status) = run_rerun_fixture(&rerun_environment(&scratch, false), async |master, keys, _release, _leader| press_then_quit(master, keys, b"R", flaky_cleared).await).await;
    let receipt = assert_receipt_after_review(&output, status);
    assert!(receipt.starts_with("SUMMARY  passed=0 failed=3 flaky=1 "), "3 failures stay and 1 is flaky:\n{}", shown(receipt));
    assert_eq!(receipt.lines().filter(|line| line.starts_with("FAILED")).count(), 3, "{}", shown(receipt));
    assert!(receipt.lines().any(|line| line.starts_with("FLAKY") && line.ends_with("fails_once_then_passes")), "{}", shown(receipt));
    assert_eq!(run_counts(&scratch).len(), 8, "4 first runs plus exactly 4 reruns\n{}", run_count_evidence(&scratch, &output, status));
}

fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn interrupt_during_rerun(master: &ChildStream, mut keys: File, (scratch, leader, via_key): (&Path, u32, bool)) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    keys.write_all(b"R").expect("the rerun key is writable");
    let pid_file = scratch.join("hang.pid");
    let (running, _) = drain::drain_until(master, |_| pid_file.exists()).await;
    output.extend_from_slice(&running);
    let pid: i32 = std::fs::read_to_string(&pid_file).expect("the blocked rerun wrote its pid").trim().parse().expect("the pid parses");
    assert!(process_alive(pid), "the blocked rerun is alive before the interrupt");
    let group = unsafe { libc::getpgid(pid) };
    std::fs::write(scratch.join("hang.pgid"), group.to_string()).expect("the group id is writable");
    if via_key {
        keys.write_all(b"c").expect("the interrupt key is writable");
    } else {
        let cargo_val = libc::pid_t::try_from(leader).expect("a pid fits pid_t");
        assert_ne!(group, unsafe { libc::getpgid(cargo_val) }, "the rerun leads its own group, apart from cargo-val's");
        assert_eq!(unsafe { libc::kill(cargo_val, libc::SIGINT) }, 0, "SIGINT reaches cargo-val's pid alone");
    }
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

fn assert_rerun_interrupted(output: &str, status: ExitStatus, group: i32) {
    assert_eq!(status.code(), Some(130), "an interrupted rerun exits 130: {status}\n{}", shown(output));
    let block = output.rfind("INTERRUPTED  signal=2  forwarded_groups=").unwrap_or_else(|| panic!("no interrupted block naming the rerun's group:\n{}", shown(output)));
    let named = format!("killed group={group}");
    assert!(output[block..].contains(&named), "the interrupted block names the rerun's group {group}:\n{}", shown(&output[block..]));
    assert!(output.ends_with(SHOW_CURSOR), "the cursor is the last thing shown:\n{}", shown(output));
}

async fn interrupted_rerun(label: &str, via_key: bool) {
    let scratch = rerun_scratch(label);
    let (output, status) = run_rerun_fixture(&rerun_environment(&scratch, true), async |master, keys, _release, leader| interrupt_during_rerun(master, keys, (&scratch, leader, via_key)).await).await;

    let pid: i32 = std::fs::read_to_string(scratch.join("hang.pid")).expect("pid file").trim().parse().expect("pid");
    let group: i32 = std::fs::read_to_string(scratch.join("hang.pgid")).expect("pgid file").trim().parse().expect("pgid");
    assert!(!process_alive(pid), "the rerun child {pid} must be gone after the run exits");
    assert!(!process_alive(-group), "the rerun's process group {group} must be gone after the run exits");
    assert_rerun_interrupted(&output, status, group);
}

#[proxima::test]
async fn sigint_to_cargo_val_alone_during_a_rerun_is_forwarded_to_the_reruns_own_group_and_exits_130() {
    interrupted_rerun("sigint", false).await;
}

#[proxima::test]
async fn the_c_key_during_a_rerun_exits_130_kills_the_rerun_and_restores_the_terminal() {
    interrupted_rerun("ckey", true).await;
}

#[proxima::test]
async fn r_on_the_row_that_fails_only_on_its_first_run_shows_pass_and_the_receipt_counts_it_flaky() {
    let scratch = rerun_scratch("flaky");
    let mut environment = rerun_environment(&scratch, false);
    environment.push(("CARGO_VAL_FIXTURE_ONLY_ONCE".to_owned(), "1".to_owned()));
    let (output, status) = run_rerun_fixture(&environment, async |master, keys, _release, _leader| press_then_quit(master, keys, b"r", passed_row_shown).await).await;

    let frames = review_frames(&output);
    let passed = frames.iter().find(|frame| frame.contains("PASS ")).unwrap_or_else(|| panic!("no frame showed a PASS row:\n{}", shown(&output)));
    assert!(passed.contains("fails_once_then_passes"), "the PASS row names the flaky test:\n{}", shown(passed));
    let receipt = assert_receipt_after_review(&output, status);
    assert!(receipt.starts_with("SUMMARY  passed=3 failed=0 flaky=1 "), "the test that passed alone is flaky, not passed and not failed:\n{}", shown(receipt));
    assert!(receipt.lines().any(|line| line.starts_with("FLAKY") && line.ends_with("tests::fails_once_then_passes")), "{}", shown(receipt));
    assert_eq!(receipt.lines().filter(|line| line.starts_with("FAILED")).count(), 0, "{}", shown(receipt));
    assert_eq!(run_counts(&scratch).len(), 5, "4 first runs plus exactly 1 rerun\n{}", run_count_evidence(&scratch, &output, status));
}

fn rerun_refused(bytes: &[u8]) -> bool {
    without_sgr(&String::from_utf8_lossy(bytes)).contains("rerun not started")
}

async fn delete_the_binary_then_rerun(master: &ChildStream, mut keys: File, scratch: &Path) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
    let executable = std::fs::read_to_string(scratch.join("exe.path")).expect("the fixture noted its test binary");
    std::fs::remove_file(executable.trim()).expect("the test binary is deletable while review is open");
    keys.write_all(b"r").expect("the rerun key is writable");
    let (refused, _) = drain::drain_until(master, rerun_refused).await;
    output.extend_from_slice(&refused);
    keys.write_all(b"qq").expect("the quit keys are writable");
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

#[proxima::test]
async fn a_rerun_that_cannot_launch_restores_the_row_says_why_on_the_footer_and_keeps_review_open() {
    let scratch = rerun_scratch("refused");
    let (output, status) = run_rerun_fixture(&rerun_environment(&scratch, false), async |master, keys, _release, _leader| delete_the_binary_then_rerun(master, keys, &scratch).await).await;

    let frames = review_frames(&output);
    let refused = frames.iter().position(|frame| without_sgr(frame).contains("rerun not started")).unwrap_or_else(|| panic!("no review frame carried the footer message:\n{}", shown(&output)));
    let notice = without_sgr(&frames[refused]);
    assert!(notice.contains("rerun not started: the OS refused the launch, error 2: "), "the footer names the OS error:\n{}", shown(&notice));
    assert!(!notice.contains("RERUN"), "the row left RERUN when the launch was refused:\n{}", shown(&notice));
    assert!(frames[refused].contains("r rerun"), "review is still open with its keys:\n{}", shown(&frames[refused]));
    let receipt = assert_receipt_after_review(&output, status);
    assert!(receipt.starts_with("SUMMARY  passed=0 failed=4 "), "a refused rerun leaves every failure standing:\n{}", shown(receipt));
    assert_eq!(run_counts(&scratch).len(), 4, "4 first runs and no rerun, because none could launch");
}

fn second_attempt_shown(bytes: &[u8]) -> bool {
    review_frames(&String::from_utf8_lossy(bytes)).iter().any(|frame| frame.contains("second attempt") && frame.contains("rerun 1"))
}

#[proxima::test]
async fn a_rerun_that_fails_with_a_new_message_shows_that_message_in_the_detail_labelled_with_the_attempt() {
    let scratch = rerun_scratch("changing");
    let mut environment = rerun_environment(&scratch, false);
    environment.push(("CARGO_VAL_FIXTURE_ONLY_CHANGING".to_owned(), "1".to_owned()));
    environment.push(("CARGO_VAL_FIXTURE_CHANGING_STATE".to_owned(), scratch.join("changing.state").to_string_lossy().into_owned()));
    let (output, status) = run_rerun_fixture(&environment, async |master, keys, _release, _leader| press_then_quit(master, keys, b"r", second_attempt_shown).await).await;

    let frames = review_frames(&output);
    let before = frames.first().unwrap_or_else(|| panic!("no review frame:\n{}", shown(&output)));
    assert!(before.contains("first attempt") && !before.contains("rerun 1"), "the first review frame shows the original failure unlabelled:\n{}", shown(before));
    let after = frames.iter().find(|frame| frame.contains("second attempt")).unwrap_or_else(|| panic!("no frame showed the rerun's message:\n{}", shown(&output)));
    assert!(!after.contains("first attempt"), "the detail replaced the original output:\n{}", shown(after));
    assert!(after.contains("rerun 1"), "the header names the attempt:\n{}", shown(after));
    assert_eq!(status.code(), Some(1), "{status}");
}

const MOUSE_ENABLE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_DISABLE: &str = "\x1b[?1000l\x1b[?1002l\x1b[?1006l";
const CURSOR_QUERY: &str = "\x1b[6n";
const TERMINAL_ROW_UNDER_FRAME: usize = 100;

fn last_screen(text: &str) -> Option<Vec<String>> {
    let segment = text[text.rfind(REVIEW_HEADER)?..].split('\x1b').next()?.replace('\r', "");
    let lines: Vec<String> = segment.strip_suffix('\n')?.split('\n').map(str::to_owned).collect();
    let footer = lines.last()?;
    (footer.contains("q quit") || footer.contains("any key cancels")).then_some(lines)
}

async fn until_screen(master: &ChildStream, output: &mut Vec<u8>, accept: impl Fn(&[String]) -> bool) -> Vec<String> {
    let start = output.len();
    let (more, _) = drain::drain_until(master, |bytes| last_screen(&String::from_utf8_lossy(bytes)).is_some_and(|lines| accept(&lines))).await;
    output.extend_from_slice(&more);
    last_screen(&String::from_utf8_lossy(&output[start..])).unwrap_or_else(|| panic!("no accepted review screen was drawn:\n{}", shown(&String::from_utf8_lossy(&output[start..]))))
}

fn list_rows(lines: &[String]) -> Vec<usize> {
    lines.iter().enumerate().take_while(|(_, line)| !line.starts_with(" \u{2500}")).filter(|(_, line)| line.starts_with("\u{203a}ASSERT") || line.starts_with(" ASSERT")).map(|(index, _)| index).collect()
}

fn selected_row(lines: &[String]) -> Option<usize> {
    list_rows(lines).into_iter().find(|index| lines[*index].starts_with('\u{203a}'))
}

fn sgr_mouse(button: u8, column: usize, row: usize, press: bool) -> String {
    format!("\x1b[<{button};{column};{row}{}", if press { 'M' } else { 'm' })
}

fn answer_cursor_query(keys: &mut File, screen_rows: usize) -> usize {
    keys.write_all(format!("\x1b[{TERMINAL_ROW_UNDER_FRAME};1R").as_bytes()).expect("the cursor reply is writable");
    TERMINAL_ROW_UNDER_FRAME - screen_rows
}

fn click(keys: &mut File, column: usize, row: usize) {
    let report = format!("{}{}", sgr_mouse(0, column, row, true), sgr_mouse(0, column, row, false));
    keys.write_all(report.as_bytes()).expect("the click is writable");
}

async fn click_wheel_and_quit(master: &ChildStream, mut keys: File) -> String {
    let mut output = Vec::new();
    let first = until_screen(master, &mut output, |lines| list_rows(lines).len() == 2).await;
    let rows = list_rows(&first);
    assert_eq!(selected_row(&first), Some(rows[0]), "review opens on the first row:\n{}", first.join("\n"));
    println!("before the click:\n{}", shown(&first.join("\n")));

    let origin = answer_cursor_query(&mut keys, first.len());
    click(&mut keys, 3, origin + rows[1]);
    let clicked = until_screen(master, &mut output, |lines| selected_row(lines) == Some(rows[1])).await;
    println!("after the click on row {}:\n{}", rows[1], shown(&clicked.join("\n")));
    assert_ne!(clicked[rows[1] + 1..], first[rows[1] + 1..], "the detail below the list followed the selection");

    let origin = answer_cursor_query(&mut keys, clicked.len());
    keys.write_all(sgr_mouse(64, 3, origin + rows[0], true).as_bytes()).expect("the wheel is writable");
    let wheeled = until_screen(master, &mut output, |lines| selected_row(lines) == Some(rows[0])).await;
    println!("after wheel up over the list:\n{}", shown(&wheeled.join("\n")));

    let origin = answer_cursor_query(&mut keys, wheeled.len());
    let footer_at = wheeled.len() - 1;
    let quit_column = wheeled[footer_at].find("q quit").expect("the footer offers q quit") + 1;
    click(&mut keys, quit_column, origin + footer_at);
    let armed = until_screen(master, &mut output, |lines| lines.last().is_some_and(|footer| footer.contains("any key cancels"))).await;
    assert_eq!(armed.len(), wheeled.len(), "arming keeps the frame the same height, so the same click lands again");
    click(&mut keys, quit_column, origin + footer_at);
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

#[proxima::test]
async fn review_enables_mouse_reporting_and_clicks_and_the_wheel_move_the_selection_and_two_clicks_on_q_quit_leave_with_the_receipt() {
    let (output, status) = run_driven("rich_pty_fail_pair", "rich", &NO_COLOR, async |master, keys, _release, _leader| click_wheel_and_quit(master, keys).await).await;

    let enabled = output.find(MOUSE_ENABLE).unwrap_or_else(|| panic!("review never enabled mouse reporting:\n{}", shown(&output)));
    assert_eq!(output.matches(MOUSE_ENABLE).count(), 1, "reporting is enabled once, not per frame");
    assert!(!live_frames(&output[..enabled]).is_empty(), "live frames were drawn before review");
    assert!(!output[..enabled].contains(REVIEW_HEADER) && !output[..enabled].contains("?1000"), "the live run asked for no mouse reporting:\n{}", shown(&output[..enabled]));
    assert!(output[enabled..].contains(CURSOR_QUERY), "review asks the terminal where the frame is");
    let disabled = output.rfind(MOUSE_DISABLE).unwrap_or_else(|| panic!("reporting was never turned off:\n{}", shown(&output)));
    let receipt = output.rfind("SUMMARY").unwrap_or_else(|| panic!("no receipt was printed:\n{}", shown(&output)));
    let shown_cursor = output.rfind(SHOW_CURSOR).unwrap_or_else(|| panic!("the cursor was never shown again:\n{}", shown(&output)));
    assert_eq!(status.code(), Some(1), "two clicks on q quit exit like q q: {status}");
    assert!(enabled < disabled && disabled < receipt && receipt < shown_cursor, "order: enable {enabled}, disable {disabled}, receipt {receipt}, cursor {shown_cursor}");
    assert!(output.ends_with(SHOW_CURSOR), "the last bytes of the run show the cursor:\n{}", shown(&output));
}

#[proxima::test]
async fn with_the_mouse_setting_off_review_never_enables_reporting_and_keys_still_quit() {
    let (output, status) = run_driven("rich_pty_fail", "rich", &[("NO_COLOR", "1"), ("CARGO_VAL_MOUSE", "false")], async |master, keys, _release, _leader| quit_from_review(master, keys).await).await;

    assert!(output.contains(REVIEW_HEADER), "review was drawn:\n{}", shown(&output));
    assert!(!output.contains("\x1b[?1000") && !output.contains("\x1b[?1002") && !output.contains("\x1b[?1006"), "no mouse mode sequence was written:\n{}", shown(&output));
    assert!(!output.contains(CURSOR_QUERY), "no cursor query without reporting:\n{}", shown(&output));
    assert_eq!(status.code(), Some(1), "q q still exits: {status}");
    assert!(output.contains("SUMMARY"), "the receipt printed:\n{}", shown(&output));
}

const MOUSE_REPORTING_ON: &str = "\x1b[?1000h";
const MOUSE_REPORTING_OFF: &str = "\x1b[?1000l";

async fn leave_review(master: &ChildStream, mut keys: File, leader: u32, signal: Option<libc::c_int>) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(MOUSE_ENABLE)).await;
    match signal {
        None => keys.write_all(b"c").expect("the interrupt key is writable"),
        Some(signal) => {
            let pid = libc::pid_t::try_from(leader).expect("a pid fits pid_t");
            assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "the signal reaches cargo-val's pid alone");
        }
    }
    let (rest, _) = drain::drain_until(master, |_| false).await;
    output.extend_from_slice(&rest);
    String::from_utf8_lossy(&output).into_owned()
}

async fn left_review_with(signal: Option<libc::c_int>, code: i32) {
    let (output, status) = run_driven("rich_pty_fail_pair", "rich", &NO_COLOR, async |master, keys, _release, leader| leave_review(master, keys, leader, signal).await).await;

    let enabled = output.rfind(MOUSE_REPORTING_ON).unwrap_or_else(|| panic!("review never enabled mouse reporting:\n{}", shown(&output)));
    let disabled = output.rfind(MOUSE_REPORTING_OFF).unwrap_or_else(|| panic!("reporting was never turned off:\n{}", shown(&output)));
    let shown_cursor = output.rfind(SHOW_CURSOR).unwrap_or_else(|| panic!("the cursor was never shown again:\n{}", shown(&output)));
    assert_eq!(status.code(), Some(code), "{status}\n{}", shown(&output));
    assert!(enabled < disabled && disabled < shown_cursor, "order: enable {enabled}, disable {disabled}, cursor {shown_cursor}\n{}", shown(&output));
}

#[proxima::test]
async fn pressing_c_in_review_turns_mouse_reporting_off_before_the_cursor_shows_and_exits_130() {
    left_review_with(None, 130).await;
}

#[proxima::test]
async fn sigint_during_review_turns_mouse_reporting_off_before_the_cursor_shows_and_exits_130() {
    left_review_with(Some(libc::SIGINT), 130).await;
}

#[proxima::test]
async fn sigterm_during_review_turns_mouse_reporting_off_before_the_cursor_shows_and_exits_143() {
    left_review_with(Some(libc::SIGTERM), 143).await;
}

#[proxima::test]
async fn a_failing_run_with_stdin_piped_never_asks_for_mouse_reporting_or_the_cursor_position() {
    let (output, status) = run_with("rich_pty_fail_pair", "rich", &NO_COLOR, false, async |master, _keys, _release, _leader| read_plain(master).await).await;

    assert!(!live_frames(&output).is_empty(), "rich frames were drawn, so the absence below is not an empty run:\n{}", shown(&output));
    assert!(!output.contains("\x1b[?1000") && !output.contains("\x1b[?1002") && !output.contains("\x1b[?1006"), "a mouse mode sequence was written:\n{}", shown(&output));
    assert!(!output.contains(CURSOR_QUERY), "the cursor position was queried:\n{}", shown(&output));
    assert!(!output.contains(REVIEW_HEADER), "review needs a tty on stdin:\n{}", shown(&output));
    assert_eq!(status.code(), Some(1), "{status}");
}
