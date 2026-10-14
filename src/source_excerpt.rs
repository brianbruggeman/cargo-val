//! Source excerpt: a located `file:line:col` in, a 7-line window of borrowed spans out.
//!
//! Shape: `read_source` is a cold source `In = (root, file)`, `Out = Result<Vec<u8>, SourceError>`
//! (the one allocation that owns the bytes); `Excerpt::new` is a transform from that buffer to
//! offsets into it; `Excerpt::write` is a sink into a caller-owned `Vec<u8>`. Scans use `memchr`.
//! Compose it with `failure_class::Location`, whose `file`, `line` and `col` are its inputs.
//!
//! Allocation budget: read buffer 1, caller output buffer growth (reserve it to make it 0); the
//! `highlight` feature adds the tree-sitter allocations counted in `benches/source_excerpt.rs`.

use core::fmt;
use core::ops::Range;
use std::fs;
use std::format;
use std::io;
use std::path::{Path, PathBuf};
use std::string::{String, ToString};
use std::vec::Vec;

use memchr::{memchr, memchr_iter};

use crate::failure_class::{Absent, Record, Reject};

pub const WINDOW: usize = 7;
const ABOVE: u32 = 3;
const LAST_ROW: u32 = 6;
const GUTTER_BLANK: &[u8] = b"      | ";
const CARET: u8 = b'^';
const PANIC_LABEL: &[u8] = b"panicked here";
const ROW_BYTES: usize = 512;
const CUT_MARK: &[u8] = "…".as_bytes();

/// Why no excerpt exists: always a named reason, never an empty success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    NotFound { path: PathBuf },
    Unreadable { path: PathBuf, kind: io::ErrorKind },
    NotUtf8Path { lossy: String },
    LineOutOfRange { line: u32, total: u32 },
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "source not found: {}", path.display()),
            Self::Unreadable { path, kind } => write!(formatter, "source unreadable: {} ({kind})", path.display()),
            Self::NotUtf8Path { lossy } => write!(formatter, "source path is not UTF-8: {lossy}"),
            Self::LineOutOfRange { line, total } => write!(formatter, "source line {line} is outside the file's {total} lines"),
        }
    }
}

/// # Errors
/// `NotFound`, `Unreadable` or `NotUtf8Path`, each carrying the path.
///
/// Reads `file` (as located by the failure record) relative to `root`, once. Absolute paths win.
pub fn read_source(root: &Path, file: &[u8]) -> Result<Vec<u8>, SourceError> {
    let relative = core::str::from_utf8(file).map_err(|_| SourceError::NotUtf8Path { lossy: String::from_utf8_lossy(file).into_owned() })?;
    let path = root.join(relative);
    fs::read(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => SourceError::NotFound { path },
        kind => SourceError::Unreadable { path, kind },
    })
}

/// Up to 7 consecutive lines of one buffer, as byte ranges into it. The window slides at the file
/// edges so it still holds 7 lines; a shorter file shows all of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excerpt<'a> {
    source: &'a [u8],
    first: u32,
    len: usize,
    spans: [Range<usize>; WINDOW],
    cut: u8,
    failing: usize,
    col: u32,
}

impl<'a> Excerpt<'a> {
    /// # Errors
    /// `LineOutOfRange` when `line` is 0 or past the last line.
    ///
    /// `line` and `col` are 1-based, as in `failure_class::Location`; a `col` of 0 (no column
    /// known) carets column 1.
    pub fn new(source: &'a [u8], line: u32, col: u32) -> Result<Self, SourceError> {
        let total = line_count(source);
        if line == 0 || line > total {
            return Err(SourceError::LineOutOfRange { line, total });
        }
        let wanted = line.saturating_sub(ABOVE).max(1);
        let first = wanted.min(total.saturating_sub(LAST_ROW).max(1));
        let mut spans: [Range<usize>; WINDOW] = core::array::from_fn(|_| 0..0);
        let (len, cut) = fill_spans(source, first, &mut spans);
        Ok(Self { source, first, len, spans, cut, failing: (line - first) as usize, col: col.max(1) })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn first_line(&self) -> u32 {
        self.first
    }

    #[must_use]
    pub fn failing_index(&self) -> usize {
        self.failing
    }

    #[must_use]
    pub fn row(&self, index: usize) -> &'a [u8] {
        &self.source[self.spans[index].clone()]
    }

    pub(crate) fn write_cut_mark(&self, out: &mut Vec<u8>, index: usize) {
        if self.cut >> index & 1 == 1 {
            out.extend_from_slice(CUT_MARK);
        }
    }

    #[cfg(feature = "highlight")]
    pub(crate) fn source(&self) -> &'a [u8] {
        self.source
    }

    #[cfg(feature = "highlight")]
    pub(crate) fn span(&self, index: usize) -> Range<usize> {
        self.spans[index].clone()
    }

    /// Plain form, section 10.2 of the design: ASCII gutter, no ESC byte, rows past 512 bytes cut at a char boundary and marked.
    pub fn write_plain(&self, out: &mut Vec<u8>, label: &[u8]) {
        for index in 0..self.len {
            self.write_gutter(out, index);
            out.extend_from_slice(self.row(index));
            self.write_cut_mark(out, index);
            out.push(b'\n');
            if index == self.failing {
                self.write_caret_row(out, label);
            }
        }
    }

    /// `colour` is the caller's resolved answer (`colour_allowed`), so this stays free of ambient
    /// state. Without the `highlight` feature, or with `colour` false, this is `write_plain`; the
    /// returned reason is `Some` only when highlighting was wanted and could not be done.
    pub fn write(&self, out: &mut Vec<u8>, label: &[u8], colour: bool) -> Option<&'static str> {
        #[cfg(feature = "highlight")]
        if colour {
            return match crate::source_highlight::write_highlighted(self, out, label) {
                Ok(()) => None,
                Err(reason) => {
                    self.write_plain(out, label);
                    Some(reason)
                }
            };
        }
        let _ = colour;
        self.write_plain(out, label);
        None
    }

    pub(crate) fn write_gutter(&self, out: &mut Vec<u8>, index: usize) {
        out.push(if index == self.failing { b'>' } else { b' ' });
        out.push(b' ');
        write_number(out, self.first.saturating_add(u32::try_from(index).unwrap_or(u32::MAX)));
        out.extend_from_slice(b" |");
        if !self.row(index).is_empty() {
            out.push(b' ');
        }
    }

    pub(crate) fn write_caret_row(&self, out: &mut Vec<u8>, label: &[u8]) {
        let line = self.row(self.failing);
        let skip = (self.col as usize - 1).min(line.len());
        out.extend_from_slice(GUTTER_BLANK);
        out.extend(line[..skip].iter().filter(|byte| !is_continuation(**byte)).map(|byte| if *byte == b'\t' { b'\t' } else { b' ' }));
        out.extend(core::iter::repeat_n(CARET, token_len(&line[skip..])));
        if !label.is_empty() {
            out.push(b' ');
            out.extend_from_slice(label);
        }
        out.push(b'\n');
    }
}

/// The excerpt rows for one classified failure, or the named reason there are none: the classifier's absent reason,
/// or the read error. Reads the source once; cold path. `colour` is `write`'s resolved answer.
///
/// # Errors
///
/// The reason text: `no_panic_site`, `external:<file>:<line>`, `no_panic_in_capture`, or the `SourceError` message.
pub fn for_failure(root: &Path, colour: bool, record: &Result<Record<'_>, Reject>) -> Result<Vec<u8>, Vec<u8>> {
    let site = match record {
        Ok(Record { location: Ok(site), .. }) => site,
        Ok(Record { location: Err(Absent::NoPanicSite), .. }) => return Err(b"no_panic_site".to_vec()),
        Ok(Record { location: Err(Absent::External(site)), .. }) => {
            return Err(format!("external:{}:{}", String::from_utf8_lossy(site.file), site.line).into_bytes());
        }
        Err(_) => return Err(b"no_panic_in_capture".to_vec()),
    };
    let source = read_source(root, site.file).map_err(|error| error.to_string().into_bytes())?;
    let excerpt = Excerpt::new(&source, site.line, site.col).map_err(|error| error.to_string().into_bytes())?;
    let mut rows = Vec::new();
    if let Some(reason) = excerpt.write(&mut rows, PANIC_LABEL, colour) {
        proxima_telemetry::debug!(reason = reason, file = %String::from_utf8_lossy(site.file), "highlight unavailable, plain excerpt rendered");
    }
    Ok(rows)
}

/// `NO_COLOR` present, even empty, disables colour (no-color.org); the one ambient read, so the
/// renderer takes its answer as data.
#[must_use]
pub fn colour_allowed() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

fn line_count(source: &[u8]) -> u32 {
    let newlines = memchr_iter(b'\n', source).count();
    let unterminated = !source.is_empty() && source.last() != Some(&b'\n');
    u32::try_from(newlines + usize::from(unterminated)).unwrap_or(u32::MAX)
}

fn fill_spans(source: &[u8], first: u32, spans: &mut [Range<usize>; WINDOW]) -> (usize, u8) {
    let mut start = 0;
    for _ in 1..first {
        start += memchr(b'\n', &source[start..]).map_or(source.len(), |at| at + 1);
    }
    let mut len = 0;
    let mut cut = 0;
    while len < WINDOW && start < source.len() {
        let end = memchr(b'\n', &source[start..]).map_or(source.len(), |at| start + at);
        let trimmed = if end > start && source[end - 1] == b'\r' { end - 1 } else { end };
        let bounded = bounded_end(source, start, trimmed);
        cut |= u8::from(bounded < trimmed) << len;
        spans[len] = start..bounded;
        len += 1;
        start = end + 1;
    }
    (len, cut)
}

fn bounded_end(source: &[u8], start: usize, end: usize) -> usize {
    let mut bounded = end.min(start + ROW_BYTES);
    while bounded < end && is_continuation(source[bounded]) {
        bounded -= 1;
    }
    bounded
}

fn token_len(rest: &[u8]) -> usize {
    rest.iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_').count().max(1)
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

fn write_number(out: &mut Vec<u8>, number: u32) {
    let mut digits = [b' '; 10];
    let mut remaining = number;
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    let width = digits.len() - at;
    out.extend(core::iter::repeat_n(b' ', 4usize.saturating_sub(width)));
    out.extend_from_slice(&digits[at..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write;
    use std::format;
    use std::string::ToString;

    use crate::failure_class::{Exit, classify};

    const ROOT: &str = env!("CARGO_MANIFEST_DIR");

    fn find_line(source: &[u8], needle: &[u8]) -> u32 {
        let at = memchr::memmem::find(source, needle).expect("the needle is in the real source file");
        1 + u32::try_from(memchr_iter(b'\n', &source[..at]).count()).expect("fewer than 2^32 lines")
    }

    #[test]
    fn a_failure_mid_file_gets_exactly_seven_rows_with_gutter_marker_and_caret_row() {
        let source = read_source(Path::new(ROOT), b"tests/shard_process.rs").expect("real file");
        let line = find_line(&source, b"let full = format!(");
        let excerpt = Excerpt::new(&source, line, 5).expect("inside the file");
        let mut out = Vec::new();

        excerpt.write_plain(&mut out, b"panicked here");

        let text = String::from_utf8(out).expect("the real file is UTF-8");
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows.len(), WINDOW + 1);
        assert_eq!(excerpt.failing_index(), 3);
        assert_eq!(rows.iter().filter(|row| row.starts_with("> ")).count(), 1);
        assert!(rows[3].starts_with(&format!("> {line:>4} |     let full = format!(")), "{}", rows[3]);
        assert_eq!(rows[4], "      |     ^^^ panicked here");
        assert!(rows.iter().enumerate().filter(|(index, _)| *index != 4).all(|(_, row)| row.as_bytes().get(7) == Some(&b'|')));
        assert!(rows[0].starts_with(&format!("  {:>4} | ", line - 3)));
    }

    #[test]
    fn the_window_slides_at_the_start_of_a_file_and_still_marks_the_failing_line() {
        let source = read_source(Path::new(ROOT), b"tests/sched_records.rs").expect("real file");
        let mut out = Vec::new();

        Excerpt::new(&source, 2, 1).expect("inside the file").write_plain(&mut out, b"");

        let text = String::from_utf8(out).expect("UTF-8");
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[0].starts_with("     1 | use std::path"), "{}", rows[0]);
        assert!(rows[1].starts_with(">    2 | use std::process::Command;"), "{}", rows[1]);
        assert_eq!(rows.len(), WINDOW + 1);
        assert!(rows[2].starts_with("      | ^"), "{}", rows[2]);
    }

    #[test]
    fn the_window_slides_at_the_end_of_a_file_to_keep_seven_lines() {
        let source = read_source(Path::new(ROOT), b"tests/sched_records.rs").expect("real file");
        let total = line_count(&source);
        let excerpt = Excerpt::new(&source, total, 1).expect("last line exists");

        assert_eq!(excerpt.len(), WINDOW);
        assert_eq!(excerpt.first_line(), total - 6);
        assert_eq!(excerpt.failing_index(), 6);
        let last = source.split(|byte| *byte == b'\n').rev().nth(1).expect("terminated file");
        assert_eq!(excerpt.row(6), last.strip_suffix(b"\r").unwrap_or(last));
    }

    fn excerpt_of_file_with(bytes: &[u8], line: u32) -> (Vec<Vec<u8>>, usize, u32) {
        let dir = tempfile::tempdir().expect("tempdir is creatable");
        std::fs::write(dir.path().join("lib.rs"), bytes).expect("tempdir is writable");
        let source = read_source(dir.path(), b"lib.rs").expect("file was just written");
        let excerpt = Excerpt::new(&source, line, 1).expect("line is inside the file");
        let rows = (0..excerpt.len()).map(|index| excerpt.row(index).to_vec()).collect();
        (rows, excerpt.failing_index(), excerpt.first_line())
    }

    fn numbered_lines(count: u32, ending: &str) -> String {
        (1..=count).fold(String::new(), |mut text, number| {
            write!(text, "let value_{number} = {number};{ending}").expect("writing to a String cannot fail");
            text
        })
    }

    #[test]
    fn a_crlf_file_on_disk_yields_rows_without_carriage_returns_and_the_same_window_as_lf() {
        let crlf = numbered_lines(12, "\r\n");
        let lf = numbered_lines(12, "\n");

        let (crlf_rows, crlf_failing, crlf_first) = excerpt_of_file_with(crlf.as_bytes(), 12);
        let (lf_rows, lf_failing, lf_first) = excerpt_of_file_with(lf.as_bytes(), 12);

        assert_eq!((crlf_rows.len(), crlf_first, crlf_failing), (WINDOW, 6, 6));
        assert!(crlf_rows.iter().all(|row| !row.contains(&b'\r')));
        assert_eq!((crlf_rows, crlf_first, crlf_failing), (lf_rows, lf_first, lf_failing));
    }

    #[test]
    fn a_crlf_file_counts_lines_like_an_lf_file_and_rejects_the_line_past_the_end() {
        let crlf = numbered_lines(5, "\r\n");

        let error = Excerpt::new(crlf.as_bytes(), 6, 1).expect_err("only five lines");

        assert_eq!(error, SourceError::LineOutOfRange { line: 6, total: 5 });
    }

    #[test]
    fn a_final_line_without_a_newline_is_the_last_row_and_keeps_the_window_full() {
        let unterminated = numbered_lines(9, "\n").trim_end().to_string();

        let (rows, failing, first) = excerpt_of_file_with(unterminated.as_bytes(), 9);

        assert_eq!((rows.len(), first, failing), (WINDOW, 3, 6));
        assert_eq!(rows[6], b"let value_9 = 9;");
    }

    #[test]
    fn a_crlf_file_whose_last_line_has_no_terminator_trims_only_the_interior_carriage_returns() {
        let mixed = format!("{}let value_4 = 4;", numbered_lines(3, "\r\n"));

        let (rows, failing, _) = excerpt_of_file_with(mixed.as_bytes(), 4);

        assert_eq!(rows, [&b"let value_1 = 1;"[..], b"let value_2 = 2;", b"let value_3 = 3;", b"let value_4 = 4;"]);
        assert_eq!(failing, 3);
    }

    #[test]
    fn the_caret_row_of_a_crlf_line_has_no_carriage_return_and_spans_the_token() {
        let source = b"fn main() {\r\n    boom();\r\n}\r\n";
        let mut out = Vec::new();

        Excerpt::new(source, 2, 5).expect("three lines").write_plain(&mut out, b"here");

        assert_eq!(memchr_iter(b'\r', &out).count(), 0);
        assert!(memchr::memmem::find(&out, b"      |     ^^^^ here\n").is_some(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn a_row_past_the_budget_is_cut_on_a_char_boundary_and_ends_with_the_cut_mark() {
        let wide = "é".repeat(ROW_BYTES);
        let source = format!("let before = 1;\n{wide}\nlet after = 2;\n");
        let excerpt = Excerpt::new(source.as_bytes(), 2, 1).expect("three lines");
        let mut out = Vec::new();

        excerpt.write_plain(&mut out, b"");

        let text = String::from_utf8(out).expect("the cut kept every char whole");
        let cut_row = text.lines().find(|row| row.starts_with('>')).expect("the failing row");
        assert!(cut_row.ends_with("é…"), "{cut_row}");
        assert!(cut_row.len() <= ROW_BYTES + 16, "{} bytes", cut_row.len());
        assert!(text.contains("let after = 2;\n"), "rows after the cut row survive");
    }

    #[test]
    fn a_file_shorter_than_the_window_shows_all_of_itself() {
        let excerpt = Excerpt::new(b"fn main() {}\nlet x = 1;\n", 2, 5).expect("two lines");

        assert_eq!((excerpt.len(), excerpt.first_line(), excerpt.failing_index()), (2, 1, 1));
    }

    #[test]
    fn a_missing_file_is_an_explicit_record_not_an_empty_excerpt() {
        let error = read_source(Path::new(ROOT), b"tests/no_such_file.rs").expect_err("the file does not exist");

        let expected = Path::new(ROOT).join("tests/no_such_file.rs");
        assert_eq!(error, SourceError::NotFound { path: expected.clone() });
        assert_eq!(error.to_string(), format!("source not found: {}", expected.display()));
    }

    #[test]
    fn a_line_past_the_end_of_the_file_is_rejected_with_the_file_length() {
        let error = Excerpt::new(b"a\nb\n", 3, 1).expect_err("only two lines");

        assert_eq!(error, SourceError::LineOutOfRange { line: 3, total: 2 });
        assert_eq!(Excerpt::new(b"a\n", 0, 1), Err(SourceError::LineOutOfRange { line: 0, total: 1 }));
    }

    #[test]
    fn crlf_line_ends_are_not_part_of_the_code() {
        let excerpt = Excerpt::new(b"one\r\ntwo\r\n", 1, 1).expect("two lines");

        assert_eq!((excerpt.row(0), excerpt.row(1)), (&b"one"[..], &b"two"[..]));
    }

    #[test]
    fn plain_output_contains_no_escape_byte_even_when_colour_is_asked_for_and_unavailable() {
        let source = read_source(Path::new(ROOT), b"tests/shard_process.rs").expect("real file");
        let line = find_line(&source, b"let full = format!(");
        let excerpt = Excerpt::new(&source, line, 9).expect("inside the file");
        let mut plain = Vec::new();
        let mut no_colour = Vec::new();

        excerpt.write_plain(&mut plain, b"here");
        let reason = excerpt.write(&mut no_colour, b"here", false);

        assert_eq!(memchr_iter(0x1b, &plain).count(), 0);
        assert_eq!(no_colour, plain);
        assert_eq!(reason, None);
    }

    #[test]
    fn no_color_set_even_empty_disables_colour() {
        temp_env::with_var("NO_COLOR", Some(""), || assert!(!colour_allowed()));
        temp_env::with_var("NO_COLOR", None::<&str>, || assert!(colour_allowed()));
    }

    #[test]
    fn numbers_right_align_in_four_columns_and_widen_past_them() {
        let mut out = Vec::new();
        for number in [7, 575, 3189, 12345] {
            write_number(&mut out, number);
            out.push(b'|');
        }

        assert_eq!(out, b"   7| 575|3189|12345|");
    }

    const PANIC_CAPTURE: &[u8] = b"\nthread 'fixture::fails_when_requested' (4242) panicked at src/lib.rs:12:13:\nrequested fixture failure\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";

    fn cli_run_root() -> PathBuf {
        Path::new(ROOT).join("tests/fixtures/cli_run")
    }

    fn rows_for(capture: &[u8], exit: Exit, root: &Path, colour: bool) -> Result<Vec<u8>, Vec<u8>> {
        for_failure(root, colour, &classify(capture, exit, 0))
    }

    #[test]
    fn a_real_panic_in_the_cli_run_fixture_yields_plain_rows_with_the_marker_on_its_line_and_no_escape() {
        let rows = rows_for(PANIC_CAPTURE, Exit::Code(101), &cli_run_root(), false).expect("the fixture source is readable");

        let text = String::from_utf8(rows).expect("UTF-8");
        let marked: Vec<&str> = text.lines().filter(|line| line.starts_with("> ")).collect();
        assert_eq!(marked, [">   12 |             panic!(\"requested fixture failure\");"]);
        assert!(text.contains("      |             ^^^^^ panicked here"), "{text}");
        assert_eq!(text.lines().count(), 8, "{text}");
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn a_failure_with_no_panic_site_reports_the_classifiers_absent_reason() {
        let reason = rows_for(b"", Exit::DeadlineKilled, &cli_run_root(), false).expect_err("a deadline kill has no location");

        assert_eq!(reason, b"no_panic_site");
    }

    #[test]
    fn a_missing_source_file_reports_the_read_error_naming_the_path() {
        let reason = rows_for(PANIC_CAPTURE, Exit::Code(101), &Path::new(ROOT).join("tests/fixtures/absent"), false).expect_err("no such root");

        let text = String::from_utf8(reason).expect("UTF-8");
        assert!(text.starts_with("source not found: "), "{text}");
        assert!(text.ends_with("src/lib.rs"), "{text}");
    }

    #[cfg(feature = "highlight")]
    #[test]
    fn with_colour_on_the_keyword_on_the_line_above_the_panic_carries_the_keyword_role() {
        let rows = rows_for(PANIC_CAPTURE, Exit::Code(101), &cli_run_root(), true).expect("the fixture source is readable");

        let text = String::from_utf8(rows).expect("UTF-8");
        assert!(text.contains("\x1b[35mif\x1b[0m"), "{text}");
    }
}
