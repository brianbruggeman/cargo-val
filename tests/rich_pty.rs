#![cfg(unix)]

use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
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
#[path = "common/lead.rs"]
mod lead;

const FRAME_START: &str = "\x1b[?25l";
static NEXT_RUN: AtomicUsize = AtomicUsize::new(0);
const REVIEW_HEADER: &str = "cargo val  review";
const SHOW_CURSOR: &str = "\x1b[?25h";
const CLOSING_START: &str = "cargo_val_rich_pty_fail_fixture: discovered";
const COLUMNS: u16 = 80;
const ROWS: u16 = 40;

struct Pty {
    master: File,
    slave: OwnedFd,
}

fn open_pty() -> Pty {
    let mut size = libc::winsize { ws_row: ROWS, ws_col: COLUMNS, ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master, mut slave) = (0, 0);
    let opened = unsafe { libc::openpty(&raw mut master, &raw mut slave, core::ptr::null_mut(), core::ptr::null_mut(), &raw mut size) };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { Pty { master: File::from(OwnedFd::from_raw_fd(master)), slave: OwnedFd::from_raw_fd(slave) } }
}

fn fixture_manifest(fixture: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture).join("Cargo.toml")
}

fn spawn_on(slave: &OwnedFd, fixture: &str, mode: &str, (release, run): (&Path, &str), environment: &[(&str, &str)]) -> std::process::Child {
    let stream = || Stdio::from(slave.try_clone().expect("the slave descriptor duplicates"));
    let mut command = binary::command();
    lead::lead_group(&mut command);
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
        .envs(environment.iter().copied())
        .env("CARGO_VAL_FIXTURE_RELEASE_FILE", release)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-target-{fixture}-{run}")))
        .stdin(stream())
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
    run_on_pty_with(fixture, &[("NO_COLOR", "1")]).await
}

async fn run_on_pty_with(fixture: &str, environment: &[(&str, &str)]) -> (String, ExitStatus) {
    run_driven(fixture, "rich", environment, async |master, _keys, release| read_until_exit(master, &release).await).await
}

const NO_COLOR: [(&str, &str); 1] = [("NO_COLOR", "1")];

async fn run_driven<Drive>(fixture: &str, mode: &str, environment: &[(&str, &str)], drive: Drive) -> (String, ExitStatus)
where
    Drive: AsyncFnOnce(&ChildStream, File, PathBuf) -> String,
{
    let run = format!("{}-{}", std::process::id(), NEXT_RUN.fetch_add(1, Ordering::Relaxed));
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rich-pty-release-{fixture}-{mode}-{run}"));
    std::fs::create_dir_all(&directory).expect("the release directory is creatable");
    let release = directory.join("release");
    let Pty { master, slave } = open_pty();
    let keys = master.try_clone().expect("the master descriptor duplicates for writing keys");
    let mut child = spawn_on(&slave, fixture, mode, (&release, &run), environment);
    drop(slave);
    let leader = child.id();
    let exit = ExitSource::new(NonZeroI32::new(leader.cast_signed()).expect("a spawned child has a nonzero pid"));
    // boxed: the stream owns a capture window and scratch tail, together ~40 KB, which would make every test future that awaits a run trip clippy::large_futures
    let master = Box::new(ChildStream::new(OwnedFd::from(master)));
    let waiting = join(drive(&master, keys, release), exit.call(()));
    let (output, exited) = match group::bounded(leader, group::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE", waiting).await {
        Ok(finished) => finished,
        Err((report, _)) => panic!("{report}"),
    };
    let status = exited.expect("the exit source is unspent").expect("cargo-val exit status reads");
    let _ = child.try_wait();
    (output, status)
}

async fn quit_from_review(master: &ChildStream, mut keys: File) -> String {
    let (mut output, _) = drain::drain_until(master, |bytes| String::from_utf8_lossy(bytes).contains(REVIEW_HEADER)).await;
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

fn closing_block(output: &str) -> String {
    let start = output.find("SUMMARY").unwrap_or_else(|| panic!("no closing block in:\n{}", shown(output)));
    output[start..].lines().map(|line| line.split_once("wall").map_or(line, |(head, _)| head)).collect::<Vec<_>>().join("\n")
}

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
    let (output, status) = run_on_pty_with("rich_pty", &[FAILING_RUN, ("NO_COLOR", "1")]).await;

    assert!(!status.success(), "a failing test must fail the run");
    let block = failure_source_block(&output);
    assert!(block.starts_with("------ source: cargo_val_rich_pty_fixture tests::fails_when_requested ------"), "{}", shown(block));
    let marker_rows: Vec<&str> = block.lines().filter(|line| line.starts_with("> ")).collect();
    assert_eq!(marker_rows.len(), 1, "{}", shown(block));
    assert!(marker_rows[0].contains("panic!(\"requested fixture failure\");"), "{}", shown(block));
}

#[proxima::test]
async fn with_no_color_set_the_failure_source_block_has_zero_escape_bytes() {
    let (output, _) = run_on_pty_with("rich_pty", &[FAILING_RUN, ("NO_COLOR", "1")]).await;

    let block = failure_source_block(&output);

    assert_eq!(block.bytes().filter(|byte| *byte == 0x1b).count(), 0, "{}", shown(block));
}

#[cfg(feature = "highlight")]
#[proxima::test]
async fn with_highlight_and_colour_on_the_failure_source_block_colours_the_if_keyword_with_the_keyword_role() {
    let (output, _) = run_on_pty_with("rich_pty", &[FAILING_RUN]).await;

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
    let (output, status) = run_driven("rich_pty_fail", "rich", &NO_COLOR, async |master, keys, _release| quit_from_review(master, keys).await).await;
    let (plain, plain_status) = run_driven("rich_pty_fail", "plain", &NO_COLOR, async |master, _keys, _release| read_plain(master).await).await;

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
    assert_eq!(closing_block(after_frame), closing_block(&plain), "rich receipt differs from the plain closing block");
}
