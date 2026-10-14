//! Syntax colour for an `Excerpt`, behind cargo feature `highlight` (tree-sitter, cold path).
//!
//! Shape: a transform `In = &Excerpt`, `Out = bytes appended to a caller Vec`, `Err =
//! &'static str` (the reason; the caller falls back to `Excerpt::write_plain`). The whole file is
//! parsed, not the 7-line window, because a fragment cut from the middle of a function does not
//! parse; spans outside the window are discarded. Roles are the ANSI-16 table of design section
//! 10.3: tokens use magenta, blue, bright black and the default foreground only; state colours
//! (green, red, yellow, cyan) never appear here.

use std::sync::OnceLock;
use std::vec::Vec;

use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::source_excerpt::Excerpt;

const RESET: &[u8] = b"\x1b[0m";

/// Capture name, then the SGR parameters of its role. An empty role is the default foreground.
const ROLES: [(&str, &str); 21] = [
    ("keyword", "35"),
    ("function", "34"),
    ("function.method", "34"),
    ("function.macro", "1;34"),
    ("type", "1"),
    ("type.builtin", "1"),
    ("constructor", "1"),
    ("string", "3;34"),
    ("label", "3;34"),
    ("escape", "1;35"),
    ("constant", "1;35"),
    ("constant.builtin", "1;35"),
    ("attribute", "3;35"),
    ("variable.builtin", "3;35"),
    ("comment", "3;90"),
    ("comment.documentation", "3;90"),
    ("operator", "90"),
    ("punctuation.bracket", "90"),
    ("punctuation.delimiter", "90"),
    ("variable.parameter", ""),
    ("property", ""),
];

static RUST: OnceLock<Result<HighlightConfiguration, &'static str>> = OnceLock::new();

fn rust_configuration() -> Result<&'static HighlightConfiguration, &'static str> {
    let built = RUST.get_or_init(|| {
        let language = Language::from(tree_sitter_rust::LANGUAGE);
        let mut configuration = HighlightConfiguration::new(language, "rust", tree_sitter_rust::HIGHLIGHTS_QUERY, tree_sitter_rust::INJECTIONS_QUERY, "")
            .map_err(|_| "rust highlight query did not compile")?;
        let names: Vec<&str> = ROLES.iter().map(|(name, _)| *name).collect();
        configuration.configure(&names);
        Ok(configuration)
    });
    built.as_ref().map_err(|reason| *reason)
}

/// Styled byte ranges of `source`, ascending and non-overlapping; unstyled bytes are the gaps.
fn styled_spans(source: &[u8]) -> Result<Vec<(usize, usize, &'static str)>, &'static str> {
    let configuration = rust_configuration()?;
    let mut highlighter = Highlighter::new();
    let events = highlighter.highlight(configuration, source, None, None, |_| None).map_err(|_| "highlighter rejected the source")?;
    let mut open: Vec<usize> = Vec::new();
    let mut spans = Vec::new();
    for event in events {
        match event.map_err(|_| "highlighting failed part way")? {
            HighlightEvent::HighlightStart(highlight) => open.push(highlight.0),
            HighlightEvent::HighlightEnd => {
                open.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(role) = open.last().map(|index| ROLES[*index].1).filter(|role| !role.is_empty()) {
                    spans.push((start, end, role));
                }
            }
        }
    }
    Ok(spans)
}

/// # Errors
/// The reason highlighting was impossible; nothing was appended.
///
/// Writes the same rows as `Excerpt::write_plain`, with token roles added. On `Err` nothing was
/// appended to `out`.
pub fn write_highlighted(excerpt: &Excerpt<'_>, out: &mut Vec<u8>, label: &[u8]) -> Result<(), &'static str> {
    let spans = styled_spans(excerpt.source())?;
    for index in 0..excerpt.len() {
        excerpt.write_gutter(out, index);
        write_row(out, excerpt, index, &spans);
        out.push(b'\n');
        if index == excerpt.failing_index() {
            excerpt.write_caret_row(out, label);
        }
    }
    Ok(())
}

fn write_row(out: &mut Vec<u8>, excerpt: &Excerpt<'_>, index: usize, spans: &[(usize, usize, &str)]) {
    let row = excerpt.span(index);
    let source = excerpt.source();
    let mut cursor = row.start;
    let first = spans.partition_point(|(_, end, _)| *end <= row.start);
    for (start, end, role) in spans[first..].iter().take_while(|(start, _, _)| *start < row.end) {
        let from = (*start).max(cursor);
        let to = (*end).min(row.end);
        out.extend_from_slice(&source[cursor..from]);
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(role.as_bytes());
        out.push(b'm');
        out.extend_from_slice(&source[from..to]);
        out.extend_from_slice(RESET);
        cursor = to;
    }
    out.extend_from_slice(&source[cursor..row.end]);
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::path::Path;
    use std::string::String;

    use super::*;
    use crate::source_excerpt::read_source;

    const ROOT: &str = env!("CARGO_MANIFEST_DIR");
    const GOLDEN_FAILING_ROW: &str = concat!(
        "     \x1b[35mlet\x1b[0m full = \x1b[1;34mformat\x1b[0m\x1b[1;34m!\x1b[0m\x1b[90m(\x1b[0m",
        "\x1b[3;34m\"stdout:\x1b[0m\x1b[1;35m\\n\x1b[0m\x1b[3;34m{}\x1b[0m\x1b[1;35m\\n\x1b[0m",
        "\x1b[3;34mstderr:\x1b[0m\x1b[1;35m\\n\x1b[0m\x1b[3;34m{}\"\x1b[0m\x1b[90m,\x1b[0m run\x1b[90m.\x1b[0mstdout",
        "\x1b[90m,\x1b[0m run\x1b[90m.\x1b[0mstderr\x1b[90m)\x1b[0m\x1b[90m;\x1b[0m"
    );

    fn real_excerpt_rows(colour: bool) -> (Vec<u8>, usize) {
        let source = read_source(Path::new(ROOT), b"tests/shard_process.rs").expect("real file");
        let at = memchr::memmem::find(&source, b"let full = format!(").expect("the line is in the real file");
        let line = 1 + u32::try_from(memchr::memchr_iter(b'\n', &source[..at]).count()).expect("fewer than 2^32 lines");
        let excerpt = Excerpt::new(&source, line, 5).expect("inside the file");
        let mut out = Vec::new();
        assert_eq!(excerpt.write(&mut out, b"here", colour), None);
        (out, excerpt.failing_index())
    }

    #[test]
    fn a_real_line_carries_keyword_macro_and_string_roles_byte_for_byte() {
        let (out, failing) = real_excerpt_rows(true);

        let text = String::from_utf8(out).expect("UTF-8");
        let row = text.lines().nth(failing).expect("failing row exists");
        let (gutter, code) = row.split_once('|').expect("gutter bar");
        assert!(gutter.starts_with('>'), "{gutter:?}");
        assert_eq!(code, GOLDEN_FAILING_ROW);
    }

    #[test]
    fn token_roles_never_use_a_state_colour() {
        let (out, _) = real_excerpt_rows(true);

        let state = ["32", "92", "31", "91", "33", "93", "36", "96"];
        for window in out.windows(2).enumerate().filter(|(_, pair)| pair == b"\x1b[").map(|(at, _)| at) {
            let end = out[window + 2..].iter().position(|byte| *byte == b'm').expect("an SGR ends");
            let parameters = core::str::from_utf8(&out[window + 2..window + 2 + end]).expect("ASCII");
            assert!(parameters.split(';').all(|part| !state.contains(&part)), "state colour in {parameters}");
        }
    }

    #[test]
    fn stripping_the_escapes_gives_back_the_plain_excerpt() {
        let (coloured, _) = real_excerpt_rows(true);
        let (plain, _) = real_excerpt_rows(false);

        let mut stripped = Vec::new();
        let mut bytes = coloured.iter().copied();
        while let Some(byte) = bytes.next() {
            if byte == 0x1b {
                bytes.by_ref().find(|next| *next == b'm');
            } else {
                stripped.push(byte);
            }
        }
        assert_eq!(stripped, plain);
        assert_eq!(memchr::memchr_iter(0x1b, &plain).count(), 0);
    }

    #[test]
    fn every_capture_name_in_the_table_is_one_the_grammar_query_defines() {
        let missing: Vec<&str> = ROLES.iter().map(|(name, _)| *name).filter(|name| !tree_sitter_rust::HIGHLIGHTS_QUERY.contains(&format!("@{name}"))).collect();

        assert_eq!(missing, Vec::<&str>::new());
        assert_eq!(ROLES.len(), 21);
    }
}
