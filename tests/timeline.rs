use std::path::PathBuf;
use std::process::Output;

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const ESCAPE: u8 = 0x1b;
const STIPPLE: &str = "\u{2592}";
const AXIS_LABEL: &str = " seconds";
const SHOW_CURSOR: &str = "\x1b[?25h";
const REFUSAL: &str = "--timeline is a terminal view; use --output plain or rich";

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_run/Cargo.toml")
}

async fn run(environment: &[(&str, &str)], arguments: &[&str]) -> Output {
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
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("timeline-fixture-target"))
        .envs(environment.iter().copied());
    fixture_output(&mut command).await
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("plain output is UTF-8")
}

fn summary_wall(text: &str) -> String {
    let line = text.lines().find(|line| line.starts_with("SUMMARY ")).expect("a SUMMARY line is printed");
    line.split_whitespace().find_map(|field| field.strip_prefix("wall=")).expect("SUMMARY carries wall=").to_owned()
}

fn axis_end(text: &str) -> String {
    let line = text.lines().find(|line| line.starts_with(AXIS_LABEL)).expect("the timeline axis label row is drawn");
    line.split_whitespace().last().expect("the axis row ends in a time").to_owned()
}

fn colour_sequences(bytes: &[u8]) -> usize {
    bytes
        .split(|byte| *byte == ESCAPE)
        .skip(1)
        .filter(|rest| rest.first() == Some(&b'[') && rest[1..].iter().find(|byte| !byte.is_ascii_digit() && **byte != b';') == Some(&b'm'))
        .count()
}

const BLOCKS: std::ops::RangeInclusive<char> = '\u{2581}'..='\u{2588}';

fn strip_sgr(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(glyph) = chars.next() {
        if glyph != '\u{1b}' {
            kept.push(glyph);
            continue;
        }
        chars.by_ref().find(char::is_ascii_alphabetic);
    }
    kept
}

fn timeline_section(text: &str) -> String {
    let title = text.lines().position(|line| line.contains("where the time went")).expect("the timeline title is drawn");
    text.lines().skip(title).collect::<Vec<_>>().join("\n")
}

fn plot_rows(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(|line| line.contains("tests at once")).expect("the plot title row is drawn") + 1;
    let last = lines.iter().position(|line| line.starts_with(AXIS_LABEL)).expect("the axis label row is drawn");
    lines[first..last].to_vec()
}

fn plot_shape(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|row| row.chars().map(|glyph| if BLOCKS.contains(&glyph) { ' ' } else { glyph }).collect()).collect()
}

fn summary_line_index(text: &str) -> usize {
    text.lines().position(|line| line.starts_with("SUMMARY  ")).expect("a line starting with SUMMARY and two spaces is printed")
}

#[proxima::test(timeout = "310s")]
async fn a_green_plain_run_with_timeline_ends_its_axis_at_the_summary_wall_time() {
    let output = run(&[], &["--output", "plain", "--timeline"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("where the time went"), "{text}");
    let title = text.lines().position(|line| line.contains("where the time went")).expect("the timeline title is drawn");
    assert!(summary_line_index(&text) < title, "the timeline starts after the SUMMARY line:\n{text}");
    assert_eq!(axis_end(&text), summary_wall(&text), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_failing_plain_run_with_timeline_ends_its_axis_at_the_summary_wall_time() {
    let output = run(&[("CARGO_VAL_C52D_FAIL", "1"), ("CARGO_VAL_JOBS", "1")], &["--output", "plain", "--timeline"]).await;
    let text = stdout_of(&output);

    assert_eq!(output.status.code(), Some(1), "{text}");
    assert_eq!(takeaway_of(&text), "At most 1 of 1 jobs were busy at once (at <seconds>). Slowest test <seconds>.", "one job runs one test at a time, so the run never exceeds the tail limit and names its slowest test:\n{text}");
    assert!(!text.contains("After 0.000s"), "a tail that begins at zero is no tail:\n{text}");
    assert_eq!(axis_end(&text), summary_wall(&text), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn timeline_with_agent_or_json_output_is_an_invocation_error_that_runs_nothing() {
    for mode in ["agent", "json"] {
        let output = run(&[], &["--output", mode, "--timeline"]).await;

        assert_eq!(output.status.code(), Some(2), "{mode}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(REFUSAL), "{mode}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.is_empty(), "{mode}: nothing ran, so nothing is printed: {}", String::from_utf8_lossy(&output.stdout));
    }
}

#[proxima::test(timeout = "310s")]
async fn a_no_color_plain_run_draws_the_timeline_with_no_escape_byte_and_no_stipple() {
    let output = run(&[("NO_COLOR", "1")], &["--output", "plain", "--timeline"]).await;
    let text = stdout_of(&output);

    assert!(text.contains("where the time went"), "{text}");
    assert!(!output.stdout.contains(&ESCAPE), "{text}");
    assert!(!text.contains(STIPPLE), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_no_color_rich_run_with_a_failure_draws_the_plot_the_colour_run_draws_once_its_sgr_is_stripped() {
    let uncoloured = run(&[("NO_COLOR", "1"), ("CARGO_VAL_C52D_FAIL", "1")], &["--output", "rich", "--timeline"]).await;
    let coloured = run(&[("CARGO_VAL_COLOR", "true"), ("CARGO_VAL_C52D_FAIL", "1")], &["--output", "rich", "--timeline"]).await;
    let (uncoloured_text, coloured_text) = (stdout_of(&uncoloured), strip_sgr(&stdout_of(&coloured)));

    assert_eq!(uncoloured.status.code(), Some(1), "{uncoloured_text}");
    assert!(colour_sequences(&coloured.stdout) > 0, "the control run must carry the colour layer:\n{coloured_text}");
    assert_eq!(colour_sequences(&uncoloured.stdout), 0, "{uncoloured_text}");
    let (uncoloured_plot, coloured_plot) = (timeline_section(&uncoloured_text), timeline_section(&coloured_text));
    assert!(!uncoloured_plot.contains(STIPPLE) && !coloured_plot.contains(STIPPLE), "the timeline plot is one solid layer; run-frame bars may stipple:\n{uncoloured_plot}");
    let (uncoloured_rows, coloured_rows) = (plot_rows(&uncoloured_plot), plot_rows(&coloured_plot));
    assert_eq!(plot_shape(&uncoloured_rows), plot_shape(&coloured_rows), "same rows, labels and axis frame with the colour layer ignored:\n{uncoloured_text}\n{coloured_text}");
    let plot_glyphs = |rows: &[&str]| rows.iter().flat_map(|row| row.chars()).filter(|glyph| !glyph.is_ascii()).all(|glyph| BLOCKS.contains(&glyph) || "┬─┤".contains(glyph));
    assert!(plot_glyphs(&uncoloured_rows) && plot_glyphs(&coloured_rows), "one solid total layer draws only block glyphs and the axis frame");
}

#[proxima::test(timeout = "310s")]
async fn a_no_color_rich_run_prints_the_timeline_after_the_receipt_with_no_colour_and_no_stipple() {
    let output = run(&[("NO_COLOR", "1"), ("CARGO_VAL_C52D_FAIL", "1")], &["--output", "rich", "--timeline"]).await;
    let text = stdout_of(&output);

    assert_eq!(colour_sequences(&output.stdout), 0, "NO_COLOR suppresses every SGR sequence");
    let title = text.lines().position(|line| line.contains("where the time went")).expect("the timeline is printed in rich mode");
    let timeline_plot = timeline_section(&text);
    assert!(!timeline_plot.contains(STIPPLE), "the timeline plot is one solid layer; run-frame bars may stipple:\n{timeline_plot}");
    let receipt = summary_line_index(&text);
    assert!(receipt < title, "the timeline follows the receipt:\n{text}");
    let last_show = text.rfind(SHOW_CURSOR).expect("the rich run shows the cursor again");
    assert!(title < last_show, "the cursor returns after the timeline, never ahead of it:\n{text}");
    assert_eq!(text[last_show + SHOW_CURSOR.len()..].trim(), "", "the show-cursor is the last thing written:\n{text}");
}

// the sentence between the legend and the longest-tests list, with every duration blanked: the durations are the run's own timing, the words around them are the outcome under test
fn takeaway_of(text: &str) -> String {
    let sentence = text.lines().skip_while(|line| !line.starts_with(" \u{2588} ")).skip(1).take_while(|line| !line.starts_with(" longest")).map(str::trim).collect::<Vec<_>>().join(" ");
    let blank = |token: &str| {
        let bare = token.trim_end_matches([')', '.']);
        match bare.strip_suffix('s').filter(|seconds| seconds.parse::<f64>().is_ok()) {
            Some(_) => format!("<seconds>{}", &token[bare.len()..]),
            None => token.to_owned(),
        }
    };
    sentence.split_whitespace().map(blank).collect::<Vec<_>>().join(" ")
}
