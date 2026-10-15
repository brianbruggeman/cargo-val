//! OSC 8 terminal hyperlinks: the `editor_url` template setting and the formatter that wraps text in a link.

use core::ops::Range;

use conflaguration::SettingsSource;
use memchr::memmem;

use crate::config::{Layers, OutputMode};
use crate::line::Line;

const OPEN: &[u8] = b"\x1b]8;;";
const CLOSE: &[u8] = b"\x1b]8;;\x1b\\";
const STRING_TERMINATOR: &[u8] = b"\x1b\\";
const HEX_DIGITS: [u8; 16] = *b"0123456789ABCDEF";
const FILE_URL: &[u8] = b"file://{file}";
const VSCODE_URL: &[u8] = b"vscode://file{file}:{line}:{col}";
const CURSOR_URL: &[u8] = b"cursor://file{file}:{line}:{col}";
const ZED_URL: &[u8] = b"zed://file{file}:{line}:{col}";
const SITE_BYTES: usize = 512;
const TERMINAL_PROGRAMS: [&[u8]; 3] = [b"iTerm.app", b"WezTerm", b"vscode"];

/// `CARGO_VAL_EDITOR_URL`: a template over `{file}`, `{line}`, and `{col}`; unset, [`editor_template`] derives one.
pub const EDITOR_URL: Layers<'static> = Layers::setting("CARGO_VAL_EDITOR_URL", [b""; 3]);

/// Whether `file:line` text is wrapped in OSC 8: only the rich mode, with colour on, on a terminal.
#[must_use]
pub const fn links_enabled(mode: OutputMode, color: bool, terminal: bool) -> bool {
    matches!(mode, OutputMode::Rich) && color && terminal
}

/// The link template: the resolved `editor_url` setting (see [`EDITOR_URL`]), else derived from `VISUAL` then `EDITOR`, then `TERM_PROGRAM`, else a plain `file://` URL.
///
/// A pure function of what it is handed; it reads no process environment.
#[must_use]
pub fn editor_template<'source>(editor_url: Option<&'source str>, settings: &'source impl SettingsSource) -> &'source [u8] {
    editor_url
        .filter(|template| !template.is_empty())
        .map(str::as_bytes)
        .or_else(|| editor_named(settings.get("VISUAL")))
        .or_else(|| editor_named(settings.get("EDITOR")))
        .or_else(|| terminal_named(settings.get("TERM_PROGRAM")))
        .unwrap_or(FILE_URL)
}

/// The editor program's identity: the last path segment under either separator, without a Windows `.exe` or `.cmd` suffix.
pub(crate) fn program_name(program: &[u8]) -> &[u8] {
    let base = program.rsplit(|byte| matches!(byte, b'/' | b'\\')).next().unwrap_or(program);
    [b".exe".as_slice(), b".cmd".as_slice()]
        .iter()
        .find_map(|suffix| base.len().checked_sub(suffix.len()).filter(|start| base[*start..].eq_ignore_ascii_case(suffix)).map(|start| &base[..start]))
        .unwrap_or(base)
}

fn editor_named(command: Option<&str>) -> Option<&'static [u8]> {
    let program = program_name(command?.split_whitespace().next()?.as_bytes());
    match program {
        b"code" => Some(VSCODE_URL),
        b"cursor" => Some(CURSOR_URL),
        b"zed" => Some(ZED_URL),
        _ => None,
    }
}

fn terminal_named(program: Option<&str>) -> Option<&'static [u8]> {
    let program = program?.as_bytes();
    TERMINAL_PROGRAMS.contains(&program).then_some(FILE_URL)
}

/// Writes `text` wrapped in `ESC ] 8 ; ; <url> ESC \` ... `ESC ] 8 ; ; ESC \`, or `text` alone when `enabled` is false.
///
/// `template` is from [`editor_template`]; `file` is percent-encoded into `{file}`, and a relative `file` is first made absolute against `directory`, the run's manifest directory.
/// Writes into [`Line`], so nothing allocates; a URL that does not fit is cut like any other `Line` text and shows in `lost()`.
pub fn write_link<const CAPACITY: usize>(
    out: &mut Line<CAPACITY>,
    enabled: bool,
    template: &[u8],
    directory: &[u8],
    file: &[u8],
    position: (usize, usize),
    text: &[u8],
) {
    if enabled {
        out.text(OPEN);
        expand(out, template, (directory, file), position);
        out.text(STRING_TERMINATOR);
    }
    out.text(text);
    if enabled {
        out.text(CLOSE);
    }
}

/// Where `file:line:col` first appears in `text`, as the byte range the link should wrap.
#[must_use]
pub fn site_in(text: &[u8], (file, line, column): (&[u8], u32, u32)) -> Option<Range<usize>> {
    let mut token = Line::<SITE_BYTES>::new();
    token_text(&mut token, (file, line, column));
    let start = memmem::find(text, token.bytes())?;
    Some(start..start + token.bytes().len())
}

/// Writes `text` with the first `file:line:col` in it wrapped by [`write_link`]; with an empty `template` or no such token, `text` as is.
///
/// `links` is `(template, directory)` as [`write_link`] takes them. This is how a rendered block that already holds the location as text, a captured panic line or rustc's ` --> ` line, gains the link without being re-rendered.
pub fn write_linked<const CAPACITY: usize>(out: &mut Line<CAPACITY>, (template, directory): (&[u8], &[u8]), text: &[u8], site: (&[u8], u32, u32)) {
    let found = site_in(text, site).filter(|_| !template.is_empty());
    let Some(range) = found else {
        out.text(text);
        return;
    };
    out.text(&text[..range.start]);
    let (file, line, column) = site;
    write_link(out, true, template, directory, file, (line as usize, column as usize), &text[range.clone()]);
    out.text(&text[range.end..]);
}

fn token_text<const CAPACITY: usize>(out: &mut Line<CAPACITY>, (file, line, column): (&[u8], u32, u32)) {
    out.text(file);
    out.text(b":");
    out.number(line as usize);
    out.text(b":");
    out.number(column as usize);
}

fn expand<const CAPACITY: usize>(
    out: &mut Line<CAPACITY>,
    template: &[u8],
    (directory, file): (&[u8], &[u8]),
    (line, column): (usize, usize),
) {
    let mut rest = template;
    while let Some(open) = memchr::memchr(b'{', rest) {
        out.text(&rest[..open]);
        rest = &rest[open..];
        if let Some(tail) = rest.strip_prefix(b"{file}") {
            encode_absolute(out, directory, file);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix(b"{line}") {
            out.number(line);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix(b"{col}") {
            out.number(column);
            rest = tail;
        } else {
            out.text(b"{");
            rest = &rest[1..];
        }
    }
    out.text(rest);
}

fn encode_absolute<const CAPACITY: usize>(out: &mut Line<CAPACITY>, directory: &[u8], file: &[u8]) {
    if !file.starts_with(b"/") {
        encode_path(out, directory.strip_suffix(b"/").unwrap_or(directory));
        out.text(b"/");
    }
    encode_path(out, file);
}

fn encode_path<const CAPACITY: usize>(out: &mut Line<CAPACITY>, file: &[u8]) {
    for &byte in file {
        if is_path_safe(byte) {
            out.text(&[byte]);
        } else {
            out.text(&[b'%', HEX_DIGITS[usize::from(byte >> 4)], HEX_DIGITS[usize::from(byte & 15)]]);
        }
    }
}

const fn is_path_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':')
}

#[cfg(test)]
mod tests {
    use conflaguration::BorrowedSettings;

    use super::{editor_template, links_enabled, program_name, write_link};
    use crate::config::OutputMode;
    use crate::line::Line;

    const FILE: &[u8] = b"/Users/brian/repos/cargo-val/tests/shard_process.rs";
    const MANIFEST_DIRECTORY: &[u8] = b"/Users/brian/repos/cargo-val";

    fn rendered(pairs: &[(&str, &str)], file: &[u8]) -> Line<256> {
        rendered_in(pairs, None, file)
    }

    fn rendered_in(pairs: &[(&str, &str)], editor_url: Option<&str>, file: &[u8]) -> Line<256> {
        let settings = BorrowedSettings::new(pairs);
        let mut out = Line::<256>::new();
        write_link(&mut out, true, editor_template(editor_url, &settings), MANIFEST_DIRECTORY, file, (575, 31), b"tests/shard_process.rs:575:31");
        out
    }

    #[test]
    fn visual_code_links_to_a_vscode_url_at_the_line_and_column() {
        let out = rendered(&[("VISUAL", "code")], FILE);
        assert_eq!(
            out.bytes(),
            b"\x1b]8;;vscode://file/Users/brian/repos/cargo-val/tests/shard_process.rs:575:31\x1b\\tests/shard_process.rs:575:31\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn editor_zed_with_a_path_and_flags_links_to_a_zed_url() {
        let out = rendered(&[("EDITOR", "/opt/homebrew/bin/zed --wait")], FILE);
        assert!(out.bytes().starts_with(b"\x1b]8;;zed://file/Users/brian/repos/cargo-val/tests/shard_process.rs:575:31\x1b\\"));
    }

    #[test]
    fn a_windows_editor_path_with_backslashes_and_an_exe_suffix_names_the_same_editor() {
        let out = rendered(&[("VISUAL", "C:\\Users\\runneradmin\\AppData\\Local\\Programs\\vscode\\bin\\code.exe")], FILE);
        assert!(out.bytes().starts_with(b"\x1b]8;;vscode://file/"));
        assert_eq!(program_name(b"C:\\tools\\zed.exe"), b"zed");
        assert_eq!(program_name(b"/opt/homebrew/bin/zed"), b"zed");
    }

    #[test]
    fn visual_wins_over_editor_and_an_unknown_visual_falls_through_to_editor() {
        let both = [("VISUAL", "cursor"), ("EDITOR", "code")];
        assert!(rendered(&both, FILE).bytes().starts_with(b"\x1b]8;;cursor://file/"));
        let unknown = [("VISUAL", "vim"), ("EDITOR", "code")];
        assert!(rendered(&unknown, FILE).bytes().starts_with(b"\x1b]8;;vscode://file/"));
    }

    #[test]
    fn term_program_iterm_links_to_a_file_url() {
        let out = rendered(&[("TERM_PROGRAM", "iTerm.app")], FILE);
        assert_eq!(
            out.bytes(),
            b"\x1b]8;;file:///Users/brian/repos/cargo-val/tests/shard_process.rs\x1b\\tests/shard_process.rs:575:31\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn explicit_editor_url_overrides_every_derived_template() {
        let pairs = [
            ("VISUAL", "code"),
            ("TERM_PROGRAM", "WezTerm"),
        ];
        let out = rendered_in(&pairs, Some("idea://open?file={file}&line={line}"), FILE);
        assert!(out.bytes().starts_with(
            b"\x1b]8;;idea://open?file=/Users/brian/repos/cargo-val/tests/shard_process.rs&line=575\x1b\\"
        ));
    }

    #[test]
    fn no_editor_and_an_unlisted_terminal_fall_back_to_a_file_url() {
        let out = rendered(&[("TERM_PROGRAM", "Apple_Terminal")], FILE);
        assert!(out.bytes().starts_with(b"\x1b]8;;file:///Users/brian/repos/cargo-val/tests/shard_process.rs\x1b\\"));
        assert!(rendered(&[], FILE).bytes().starts_with(b"\x1b]8;;file:///Users/"));
    }

    #[test]
    fn spaces_and_non_ascii_in_the_path_are_percent_encoded() {
        let out = rendered(&[], "/Users/brian/My Repos/caf\u{e9}/lib.rs".as_bytes());
        assert!(out.bytes().starts_with(b"\x1b]8;;file:///Users/brian/My%20Repos/caf%C3%A9/lib.rs\x1b\\"));
    }

    #[test]
    fn a_relative_file_is_made_absolute_against_the_manifest_directory() {
        let out = rendered(&[("VISUAL", "code")], b"tests/shard_process.rs");
        assert_eq!(
            out.bytes(),
            b"\x1b]8;;vscode://file/Users/brian/repos/cargo-val/tests/shard_process.rs:575:31\x1b\\tests/shard_process.rs:575:31\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn visual_cursor_links_to_a_cursor_url_at_the_line_and_column() {
        let out = rendered(&[("VISUAL", "cursor")], FILE);
        assert!(out.bytes().starts_with(b"\x1b]8;;cursor://file/Users/brian/repos/cargo-val/tests/shard_process.rs:575:31\x1b\\"));
    }

    #[test]
    fn disabled_links_write_the_text_byte_for_byte() {
        let settings = BorrowedSettings::new(&[("VISUAL", "code")]);
        let mut out = Line::<64>::new();
        write_link(&mut out, false, editor_template(None, &settings), b"", FILE, (575, 31), b"tests/shard_process.rs:575:31");
        assert_eq!(out.bytes(), b"tests/shard_process.rs:575:31");
    }

    #[test]
    fn only_rich_colour_on_a_terminal_enables_links() {
        assert!(links_enabled(OutputMode::Rich, true, true));
        for (mode, color, terminal) in [
            (OutputMode::Plain, true, true),
            (OutputMode::Agent, true, true),
            (OutputMode::Json, true, true),
            (OutputMode::Rich, false, true),
            (OutputMode::Rich, true, false),
        ] {
            assert!(!links_enabled(mode, color, terminal), "{mode:?} color={color} terminal={terminal}");
        }
    }

    #[test]
    fn ten_thousand_links_allocate_nothing() {
        let settings = BorrowedSettings::new(&[("VISUAL", "code")]);
        let mut out = Line::<256>::new();
        let ((), allocations) = crate::test_alloc::measure_allocations(|| {
            for _ in 0..10_000 {
                out.clear();
                write_link(&mut out, true, editor_template(None, &settings), MANIFEST_DIRECTORY, FILE, (575, 31), b"tests/shard_process.rs:575:31");
            }
        });
        assert_eq!(out.lost(), 0);
        assert!(out.bytes().ends_with(b"\x1b]8;;\x1b\\"));
        assert_eq!(allocations, 0);
    }
}
