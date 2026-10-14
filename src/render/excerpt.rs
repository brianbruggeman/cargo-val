//! A failure's source-excerpt rows as they are kept in the arena and as they are drawn on a screen.
//!
//! Rows arrive rendered by the driver (gutter, code, caret row; SGR when colour was allowed there) and are stored
//! after the failure's record with tabs expanded, so review and the pinned frame read one copy. Drawing clips every
//! row to the screen width in display columns, which keeps the row count the redraw erases exact, and drops SGR when
//! the screen has no colour.

use crate::line::Line;
use crate::view_defaults::{EXCERPT_ROWS, EXCERPT_ROW_BYTES};

const ESCAPE: u8 = 0x1b;
const RESET: &[u8] = b"\x1b[0m";
const CUT: &str = "\u{2026}";
const TAB_STOP: usize = 4;

pub(super) const KEPT_BYTES: usize = EXCERPT_ROWS * EXCERPT_ROW_BYTES;
const KEPT_ROWS: usize = EXCERPT_ROWS + 1;

const ZERO_WIDTH: [(u32, u32); 3] = [(0x0300, 0x036f), (0x200b, 0x200f), (0xfe00, 0xfe0f)];
const DOUBLE_WIDTH: [(u32, u32); 10] = [
    (0x1100, 0x115f),
    (0x2e80, 0xa4cf),
    (0xac00, 0xd7a3),
    (0xf900, 0xfaff),
    (0xfe30, 0xfe6f),
    (0xff00, 0xff60),
    (0xffe0, 0xffe6),
    (0x1f300, 0x1f64f),
    (0x1f900, 0x1f9ff),
    (0x20000, 0x3fffd),
];

/// Copies the leading whole rows of `rows` into `out` with tabs expanded, at most `KEPT_ROWS` rows (the numbered rows
/// and the caret row) and `KEPT_BYTES` bytes, and returns the bytes written.
pub(super) fn keep(rows: &[u8], out: &mut [u8]) -> usize {
    let budget = out.len().min(KEPT_BYTES);
    let mut stored = 0;
    for row in rows.split_inclusive(|byte| *byte == b'\n').take(KEPT_ROWS) {
        match expand(row, &mut out[stored..budget]) {
            Some(written) => stored += written,
            None => break,
        }
    }
    stored
}

fn expand(row: &[u8], out: &mut [u8]) -> Option<usize> {
    let (mut at, mut written, mut column) = (0_usize, 0_usize, 0_usize);
    while at < row.len() {
        let (length, cells) = if row[at] == ESCAPE {
            (escape_end(row, at) - at, 0)
        } else if row[at] == b'\t' {
            (1, TAB_STOP - column % TAB_STOP)
        } else {
            glyph(row, at)
        };
        let target = out.get_mut(written..)?;
        if row[at] == b'\t' {
            target.get_mut(..cells)?.fill(b' ');
            written += cells;
        } else {
            target.get_mut(..length)?.copy_from_slice(&row[at..at + length]);
            written += length;
        }
        column += cells;
        at += length;
    }
    Some(written)
}

/// Draws each stored row on its own line, `indent` spaces in, at most `width` columns wide. Returns the rows drawn.
pub(super) fn draw<const FRAME: usize>(frame: &mut Line<FRAME>, rows: &[u8], (indent, width, color): (usize, usize, bool)) -> u16 {
    let mut drawn = 0_u16;
    for row in rows.split(|byte| *byte == b'\n').filter(|row| !row.is_empty()) {
        for _ in 0..indent {
            frame.text(b" ");
        }
        clipped(frame, row, width.saturating_sub(indent), color);
        frame.text(b"\n");
        drawn = drawn.saturating_add(1);
    }
    drawn
}

fn clipped<const FRAME: usize>(frame: &mut Line<FRAME>, row: &[u8], room: usize, color: bool) {
    let limit = if visible(row) <= room { usize::MAX } else { room.saturating_sub(1) };
    let (mut shown, mut at, mut styled, mut cut) = (0_usize, 0_usize, false, false);
    while at < row.len() {
        if row[at] == ESCAPE {
            let end = escape_end(row, at);
            if color {
                frame.text(&row[at..end]);
                styled = true;
            }
            at = end;
            continue;
        }
        let (length, cells) = glyph(row, at);
        if shown + cells > limit {
            cut = true;
            break;
        }
        shown += cells;
        frame.text(&row[at..at + length]);
        at += length;
    }
    if cut {
        frame.text(CUT.as_bytes());
    }
    if color && styled {
        frame.text(RESET);
    }
}

/// The terminal columns `row` occupies: escapes take none, wide and emoji code points take two.
pub(super) fn visible(row: &[u8]) -> usize {
    let mut at = 0;
    let mut columns = 0;
    while at < row.len() {
        if row[at] == ESCAPE {
            at = escape_end(row, at);
            continue;
        }
        let (length, cells) = glyph(row, at);
        columns += cells;
        at += length;
    }
    columns
}

fn glyph(row: &[u8], at: usize) -> (usize, usize) {
    let length = usize::try_from(row[at].leading_ones()).unwrap_or(1).clamp(1, 4);
    let end = (at + length).min(row.len());
    match core::str::from_utf8(&row[at..end]).ok().and_then(|text| text.chars().next()) {
        Some(found) => (length, cells(found)),
        None => (1, 1),
    }
}

fn cells(found: char) -> usize {
    let point = u32::from(found);
    let within = |table: &[(u32, u32)]| table.iter().any(|(first, last)| (*first..=*last).contains(&point));
    if within(&ZERO_WIDTH) {
        0
    } else if within(&DOUBLE_WIDTH) {
        2
    } else {
        1
    }
}

fn escape_end(row: &[u8], start: usize) -> usize {
    if row.get(start + 1) != Some(&b'[') {
        return start + 1;
    }
    row[start + 2..].iter().position(|byte| (0x40..=0x7e).contains(byte)).map_or(row.len(), |found| start + 2 + found + 1)
}
