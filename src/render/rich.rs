//! The rich projection of the run frame: the third reading of the one render [`State`], `(state, color) -> bytes`.
//!
//! Composes [`Line`] only; width, the clock and the colour choice are data already in the state or passed in, so the
//! frame is a pure function of them and allocates nothing. The in-place redraw prefix (cursor up over the rows the
//! previous frame drew, then clear to the end of the screen) is part of the bytes, so one redraw is one write.
//! Colour roles are the terminal's own 16 ANSI colours; without colour the bars keep their state as glyph textures.

use crate::line::Line;
use crate::render::{Event, Identity, Mode, State, excerpt, review};
use crate::view_defaults::{BAR_COLUMNS, INFLIGHT_ROWS};

const GREEN: &[u8] = b"\x1b[32m";
const RED: &[u8] = b"\x1b[31m";
const YELLOW: &[u8] = b"\x1b[33m";
const CYAN: &[u8] = b"\x1b[36m";
const DIM: &[u8] = b"\x1b[2m";
const BOLD: &[u8] = b"\x1b[1m";
const RESET: &[u8] = b"\x1b[0m";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const COUNT_COLUMNS: usize = 8;
const BLOCK: &str = "\u{2588}";
const EIGHTHS: [&str; 8] = ["", "\u{258f}", "\u{258e}", "\u{258d}", "\u{258c}", "\u{258b}", "\u{258a}", "\u{2589}"];
const ELLIPSIS: &str = "\u{2026}";
const BAR_MIN: usize = 10;
const BAR_COLUMN: usize = 7;
const NAME_COLUMN: usize = 23;
const LIMIT_BAR: usize = 6;
const SLOW_MS: u64 = 30_000;
const JOINED: usize = 512;
const LABEL: &str = " failed at";
const SEARCHING: &[u8] = b"searching for tests";
const REVIEW_KEY: &[u8] = b"f review failures";
const TIMELINE_KEY: &[u8] = b"s timeline";
const CANCEL_KEY: &[u8] = b"q cancel";
const ARMED_KEY: &[u8] = b"q again to confirm, any key cancels";
const REVIEW_HINT: &[u8] = b"f review";
const HINT_GAP: usize = 2;
const HINT_MIN_ROOM: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Plain,
    Green,
    Red,
    Yellow,
    Cyan,
    Dim,
}

impl Tone {
    const fn code(self) -> &'static [u8] {
        match self {
            Self::Plain => b"",
            Self::Green => GREEN,
            Self::Red => RED,
            Self::Yellow => YELLOW,
            Self::Cyan => CYAN,
            Self::Dim => DIM,
        }
    }
}

pub(super) fn paint<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, tone: Tone, text: &[u8]) {
    if text.is_empty() {
        return;
    }
    if color && tone != Tone::Plain {
        frame.text(tone.code());
        frame.text(text);
        frame.text(RESET);
    } else {
        frame.text(text);
    }
}

pub(super) fn bold<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, text: &[u8]) {
    if color {
        frame.text(BOLD);
    }
    frame.text(text);
    if color {
        frame.text(RESET);
    }
}

pub(super) fn run<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, tone: Tone, (glyph, count): (&str, usize)) {
    if count == 0 {
        return;
    }
    if color && tone != Tone::Plain {
        frame.text(tone.code());
    }
    for _ in 0..count {
        frame.text(glyph.as_bytes());
    }
    if color && tone != Tone::Plain {
        frame.text(RESET);
    }
}

pub(super) fn spaces<const FRAME: usize>(frame: &mut Line<FRAME>, count: usize) {
    for _ in 0..count {
        frame.text(b" ");
    }
}

pub(super) fn digits(value: usize) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

pub(super) fn padded<const FRAME: usize>(frame: &mut Line<FRAME>, value: usize, width: usize) {
    spaces(frame, width.saturating_sub(digits(value)));
    frame.number(value);
}

pub(super) fn columns(text: &[u8]) -> usize {
    text.iter().filter(|byte| (**byte & 0xc0) != 0x80).count()
}

pub(super) fn boundary(text: &[u8], mut at: usize) -> usize {
    while at > 0 && at < text.len() && (text[at] & 0xc0) == 0x80 {
        at -= 1;
    }
    at
}

pub(super) fn ellipsis_end<const FRAME: usize>(frame: &mut Line<FRAME>, text: &[u8], width: usize) {
    if columns(text) <= width {
        frame.text(text);
        return;
    }
    frame.text(&text[..boundary(text, width.saturating_sub(1))]);
    frame.text(ELLIPSIS.as_bytes());
}

/// `binary::name`, middle-ellipsised to `width` columns; returns the columns written.
fn ellipsis_middle<const FRAME: usize>(frame: &mut Line<FRAME>, (binary, name): Identity<'_>, width: usize) -> usize {
    let total = columns(binary) + 2 + columns(name);
    if total <= width {
        frame.text(binary);
        frame.text(b"::");
        frame.text(name);
        return total;
    }
    let mut joined = Line::<JOINED>::new();
    joined.text(binary);
    joined.text(b"::");
    joined.text(name);
    let text = joined.bytes();
    let keep = width.saturating_sub(1);
    let head = (keep * 45).div_ceil(100);
    let tail_start = boundary(text, text.len().saturating_sub(keep - head));
    frame.text(&text[..boundary(text, head)]);
    frame.text(ELLIPSIS.as_bytes());
    frame.text(&text[tail_start..]);
    width
}

fn seconds_tenths<const FRAME: usize>(frame: &mut Line<FRAME>, millis: u64) {
    let tenths = usize::try_from(millis / 100).unwrap_or(usize::MAX);
    frame.number(tenths / 10);
    frame.text(b".");
    frame.number(tenths % 10);
    frame.text(b"s");
}

fn clock<const FRAME: usize>(frame: &mut Line<FRAME>, elapsed_ms: u64) {
    let total = usize::try_from(elapsed_ms / 1000).unwrap_or(usize::MAX);
    frame.number(total / 60);
    frame.text(b":");
    if total % 60 < 10 {
        frame.text(b"0");
    }
    frame.number(total % 60);
}

/// Settled eighths, then in-flight cells, then the track. Colour draws solid blocks in role colours; no colour draws textures.
fn bar<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, width: usize, (settled, live): (usize, usize), tone: Tone) {
    let eighths = settled.min(width * 8);
    let full = eighths / 8;
    let edge = usize::from(eighths % 8 != 0);
    let live_cells = live.min(width - full - edge);
    let (live_glyph, track_glyph) = if color { (BLOCK, BLOCK) } else { ("\u{2592}", "\u{2591}") };
    run(frame, color, tone, (BLOCK, full));
    if edge == 1 {
        paint(frame, color, tone, EIGHTHS[eighths % 8].as_bytes());
    }
    run(frame, color, Tone::Cyan, (live_glyph, live_cells));
    run(frame, color, Tone::Dim, (track_glyph, width - full - edge - live_cells));
}

impl<const RERUN: usize> State<'_, RERUN> {
    fn failed_count(&self) -> usize {
        self.stored + self.unstored
    }

    fn estimate(&self) -> (usize, bool) {
        match (self.binaries_total, self.previous) {
            (Some(total), _) if self.listed_binaries >= total => (self.listed_tests, true),
            (_, Some((_, tests))) => (tests as usize, false),
            _ => (self.listed_tests, false),
        }
    }

    // a count of tests is shown only once some binary is listed and it holds tests, so `~0` is never printed
    fn tests_known(&self) -> bool {
        let (estimate, exact) = self.estimate();
        self.listed_binaries > 0 && (exact || estimate > 0 || self.done > 0)
    }

    fn note_width(&self) -> usize {
        4 + digits(self.listed_binaries) + digits(self.binaries_total.unwrap_or(0)) + 7
    }

    fn searching_label(&self) -> Line<48> {
        let mut label = Line::<48>::new();
        match self.binaries_total.filter(|total| *total > 0) {
            Some(total) => {
                label.text(b"searching ");
                label.number(total);
                label.text(if total == 1 { b" test binary" } else { b" test binaries" });
            }
            None => label.text(SEARCHING),
        }
        label
    }

    fn searching_room(&self) -> usize {
        usize::from(self.cols).saturating_sub(1 + BAR_COLUMN + 1 + self.searching_label().bytes().len())
    }

    fn room_for_bar(&self, (with_note, with_word): (bool, bool)) -> usize {
        let avail = usize::from(self.cols).saturating_sub(1);
        let word = if with_word { 6 } else { 0 };
        let note = if with_note && self.tests_known() { self.note_width() } else { 0 };
        avail.saturating_sub(BAR_COLUMN + 1 + COUNT_COLUMNS + word + 2 + digits(self.failed_count()) + 7 + note)
    }

    /// Narrowing order: the listed note goes first, then the word `tests`, then the bars shrink, then names ellipsise.
    fn layout(&self) -> (usize, bool, bool) {
        if !self.tests_known() {
            return (self.searching_room().clamp(BAR_MIN, BAR_COLUMNS), false, false);
        }
        let options = [(true, true), (false, true), (false, false)];
        let chosen = options.into_iter().find(|choice| self.room_for_bar(*choice) >= BAR_COLUMNS).unwrap_or((false, false));
        (self.room_for_bar(chosen).clamp(BAR_MIN, BAR_COLUMNS), chosen.0, chosen.1)
    }

    /// Writes the redraw prefix and one full frame: cursor up over the previous frame, clear, hide the cursor, rows.
    ///
    /// Rows never exceed `cols - 1` columns, so the row count the next redraw moves up by is exact.
    ///
    /// # Errors
    ///
    /// Returns the bytes cut when the frame buffer was too small.
    pub fn project_rich<const FRAME: usize>(&mut self, color: bool, frame: &mut Line<FRAME>) -> Result<(), usize> {
        self.erase_rich(frame);
        frame.text(HIDE_CURSOR);
        self.drawn_rows = self.rich_rows(color, frame);
        match frame.lost() {
            0 => Ok(()),
            lost => Err(lost),
        }
    }

    /// The live-run projection of one event: the frame on a tick, the plain lines for events that outlive the frame, nothing otherwise.
    ///
    /// # Errors
    ///
    /// Returns the bytes cut when the frame buffer was too small.
    pub fn project_live<const FRAME: usize>(&mut self, event: &Event<'_>, frame: &mut Line<FRAME>) -> Result<(), usize> {
        if self.shows_screen() {
            return self.project_screen(event, frame);
        }
        if self.pause_mouse() {
            frame.text(review::MOUSE_OFF);
        }
        match *event {
            Event::Tick { .. } if matches!(self.mode, Mode::Running | Mode::QuitArmed) => self.project_rich(self.color, frame),
            Event::Begin { .. } | Event::Previous { .. } | Event::Waiting { .. } => self.project_rich(self.color, frame),
            Event::Key(_) if matches!(self.mode, Mode::Running | Mode::QuitArmed) => self.project_rich(self.color, frame),
            Event::BuildFailed { .. } | Event::Interrupted { .. } | Event::Running { .. } | Event::Finished { .. } => {
                self.erase_rich(frame);
                self.project(event, frame)
            }
            Event::Captured { .. } | Event::Warned { .. } | Event::Tick { .. } | Event::Key(_) | Event::Notice { .. } | Event::Retried { .. } | Event::Reran { .. } | Event::Exited { .. } | Event::Listed { .. } | Event::Built { .. } | Event::Resize { .. } | Event::Started { .. } | Event::Settled { .. } | Event::BlockOpened { .. } | Event::BlockClosed => Ok(()),
        }
    }

    /// Marks mouse reporting off while the terminal belongs to something else; the next review frame turns it on again. Returns whether it was on, so the caller writes the disable sequence.
    pub const fn pause_mouse(&mut self) -> bool {
        let was_on = self.mouse_on();
        if was_on {
            self.mouse = Some(false);
            self.origin = 0;
            self.hover = false;
        }
        was_on
    }

    const fn shows_screen(&self) -> bool {
        matches!(self.mode, Mode::Review { .. } | Mode::Timeline { .. } | Mode::Find { .. } | Mode::Skipped { .. } | Mode::Warnings { .. })
    }

    /// While a review screen owns the terminal, input and ledger events redraw it and everything else waits for the close.
    fn project_screen<const FRAME: usize>(&mut self, event: &Event<'_>, frame: &mut Line<FRAME>) -> Result<(), usize> {
        match *event {
            Event::Key(_) | Event::Tick { .. } | Event::Reran { .. } | Event::Retried { .. } | Event::Resize { .. } | Event::Notice { .. } => self.project_review(frame),
            Event::Interrupted { .. } | Event::BuildFailed { .. } => {
                self.erase_rich(frame);
                self.project(event, frame)
            }
            _ => Ok(()),
        }
    }

    fn project_review<const FRAME: usize>(&mut self, frame: &mut Line<FRAME>) -> Result<(), usize> {
        let previous = self.drawn_rows;
        self.erase_rich(frame);
        if self.mouse == Some(false) {
            self.mouse = Some(true);
            frame.text(review::MOUSE_ON);
        }
        let before = frame.bytes().len();
        let drawn = self.screen(|id| self.detail(id), usize::from(self.cols), self.color, frame);
        let rows = memchr::memchr_iter(b'\n', &frame.bytes()[before..]).count();
        self.drawn_rows = u16::try_from(rows).unwrap_or(u16::MAX);
        if self.mouse_on() && (self.drawn_rows != previous || self.origin == 0) {
            self.origin = 0;
            frame.text(review::CURSOR_QUERY);
        }
        drawn
    }

    /// Moves the cursor back over the live frame and clears it, so the closing block replaces it in scrollback.
    pub fn erase_rich<const FRAME: usize>(&mut self, frame: &mut Line<FRAME>) {
        if self.drawn_rows != 0 {
            frame.text(b"\x1b[");
            frame.number(usize::from(self.drawn_rows));
            frame.text(b"A\r\x1b[J");
        }
        self.drawn_rows = 0;
    }

    fn rich_rows<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) -> u16 {
        let layout = self.layout();
        self.header_row(color, frame);
        let pinned = self.pinned_row(color, frame);
        frame.text(b"\n");
        self.build_row(color, frame, layout.0);
        self.rule_row(color, frame);
        self.test_row(color, frame, layout);
        let mut rows = 6 + pinned;
        if self.failed_count() != 0 {
            self.tick_row(color, frame, layout.0);
            rows += 1;
        }
        frame.text(b"\n");
        rows + 1 + self.inflight_rows(color, frame) + self.keys_row(color, frame)
    }

    /// The last row of the frame: the keys that act now, a blank row above it. Absent when no key lane reads the terminal.
    fn keys_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) -> u16 {
        if self.keys.is_none() {
            return 0;
        }
        let mut row = Line::<96>::new();
        let hints = [(self.failed_count() != 0, REVIEW_KEY), (true, TIMELINE_KEY), (true, CANCEL_KEY)];
        let limit = usize::from(self.cols).saturating_sub(1);
        if self.mode == Mode::QuitArmed {
            ellipsis_end(&mut row, ARMED_KEY, limit.saturating_sub(1));
        }
        for (_, hint) in hints.into_iter().filter(|(shown, _)| *shown && self.mode != Mode::QuitArmed) {
            let gap = if row.bytes().is_empty() { 0 } else { 2 };
            if 1 + columns(row.bytes()) + gap + columns(hint) > limit {
                break;
            }
            spaces(&mut row, gap);
            row.text(hint);
        }
        frame.text(b"\n ");
        paint(frame, color, Tone::Dim, row.bytes());
        frame.text(b"\n");
        2
    }

    fn header_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) {
        let elapsed = self.now_ms.saturating_sub(self.origin_ms.unwrap_or(self.now_ms));
        let mut stamp = Line::<24>::new();
        clock(&mut stamp, elapsed);
        let mut jobs = Line::<32>::new();
        jobs.number(self.jobs);
        jobs.text(b" jobs");
        frame.text(b" ");
        bold(frame, color, b"cargo val");
        frame.text(b"  ");
        paint(frame, color, Tone::Dim, jobs.bytes());
        let used = 1 + 9 + 2 + columns(jobs.bytes()) + columns(stamp.bytes());
        spaces(frame, usize::from(self.cols).saturating_sub(used + 1));
        bold(frame, color, stamp.bytes());
        frame.text(b"\n");
    }

    fn pinned_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) -> u16 {
        let Some(first) = self.failures().next() else {
            self.lock_note(color, frame);
            return 0;
        };
        let mut head = Line::<32>::new();
        head.text(b" FAIL ");
        head.number(self.failed_count());
        head.text(b"  ");
        if color {
            frame.text(RED);
        }
        bold(frame, color, head.bytes());
        let line_room = usize::from(self.cols).saturating_sub(1 + columns(head.bytes()));
        let hint = self.keys.is_some() && line_room >= HINT_MIN_ROOM;
        let room = line_room - if hint { HINT_GAP + REVIEW_HINT.len() } else { 0 };
        let reason = &self.reason[..self.reason_len];
        let name_room = if reason.is_empty() { room } else { (room * 3 / 5).max(12).min(room) };
        let used = ellipsis_middle(frame, first, name_room);
        if !reason.is_empty() && room > used + 3 {
            frame.text(b"  ");
            let mut tail = Line::<256>::new();
            ellipsis_end(&mut tail, reason, room - used - 2);
            paint(frame, color, Tone::Red, tail.bytes());
        }
        if hint {
            spaces(frame, HINT_GAP);
            paint(frame, color, Tone::Dim, REVIEW_HINT);
        }
        frame.text(b"\n");
        excerpt::draw(frame, self.excerpt_at(0), (1, usize::from(self.cols).saturating_sub(1), color))
    }

    fn lock_note<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) {
        if self.lock_len != 0 {
            let mut note = Line::<96>::new();
            note.text(b" waiting for cargo's ");
            note.text(&self.lock[..self.lock_len]);
            note.text(b" lock");
            paint(frame, color, Tone::Dim, note.bytes());
        }
        frame.text(b"\n");
    }

    fn build_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>, bar_width: usize) {
        paint(frame, color, Tone::Dim, b" build ");
        let (done, total) = (usize::from(self.built), self.units.map(usize::from));
        let settled = total.map_or(0, |total| done * bar_width * 8 / total.max(1));
        bar(frame, color, bar_width, (settled, 0), Tone::Plain);
        frame.text(b" ");
        padded(frame, done, 2);
        frame.text(b"/");
        match total {
            Some(total) => frame.number(total),
            None => frame.text(b"?"),
        }
        frame.text(b" compile units\n");
    }

    fn rule_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) {
        frame.text(b" ");
        run(frame, color, Tone::Dim, ("\u{2500}", usize::from(self.cols).saturating_sub(2)));
        frame.text(b"\n");
    }

    fn test_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>, (bar_width, with_note, with_word): (usize, bool, bool)) {
        let (estimate, exact) = self.estimate();
        let total = estimate.max(self.done);
        let failed = self.failed_count();
        let passed = self.done.saturating_sub(failed);
        paint(frame, color, Tone::Dim, b" test  ");
        let settled = (passed * bar_width * 8).checked_div(total).unwrap_or(0);
        let live = if total == 0 || self.inflight_count == 0 { 0 } else { (self.inflight_count * bar_width).div_ceil(total).max(1) };
        bar(frame, color, bar_width, (settled, live), Tone::Green);
        if !self.tests_known() {
            if self.searching_room() >= BAR_MIN {
                frame.text(b" ");
                paint(frame, color, Tone::Dim, self.searching_label().bytes());
            }
            frame.text(b"\n");
            return;
        }
        frame.text(b" ");
        let mut count = Line::<48>::new();
        count.number(self.done);
        count.text(if exact { b"/" } else { b"/~" });
        count.number(total);
        spaces(frame, COUNT_COLUMNS.saturating_sub(columns(count.bytes())));
        frame.text(count.bytes());
        if with_word {
            paint(frame, color, Tone::Dim, b" tests");
        }
        frame.text(b"  ");
        let mut failures = Line::<24>::new();
        failures.number(failed);
        failures.text(b" failed");
        paint(frame, color, if failed == 0 { Tone::Dim } else { Tone::Red }, failures.bytes());
        if with_note && !exact && self.tests_known() {
            let mut note = Line::<64>::new();
            note.text(b"  ~ ");
            note.number(self.listed_binaries);
            note.text(b"/");
            note.number(self.binaries_total.unwrap_or(0));
            note.text(b" listed");
            paint(frame, color, Tone::Dim, note.bytes());
        }
        frame.text(b"\n");
    }

    fn tick_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>, bar_width: usize) {
        let total = self.estimate().0.max(self.done).max(1);
        let landed = |column: usize| column >= BAR_COLUMN && self.failure_positions().any(|position| BAR_COLUMN + (position * bar_width / total).min(bar_width - 1) == column);
        let mut column = 0;
        while column < BAR_COLUMN + bar_width {
            if landed(column) {
                run(frame, color, Tone::Red, ("\u{2573}", 1));
                column += 1;
                continue;
            }
            let mut end = column;
            while end < BAR_COLUMN + bar_width && !landed(end) {
                end += 1;
            }
            let label = LABEL.get(column..end.min(LABEL.len())).unwrap_or("");
            paint(frame, color, Tone::Dim, label.as_bytes());
            spaces(frame, end - column - label.len());
            column = end;
        }
        frame.text(b"\n");
    }

    fn idle_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) {
        let mut row = Line::<96>::new();
        match self.binaries_total {
            Some(total) if self.listed_binaries > 0 && self.listed_binaries < total => {
                row.text(b"   building ");
                row.number(self.listed_binaries);
                row.text(b"/");
                row.number(total);
                row.text(b" test binaries\n");
            }
            _ if self.done == 0 => row.text(b"   waiting for the first test binary\n"),
            _ => row.text(b"   no tests running\n"),
        }
        paint(frame, color, Tone::Dim, row.bytes());
    }

    fn inflight_rows<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) -> u16 {
        let mut head = Line::<96>::new();
        head.text(b" running ");
        head.number(self.inflight_count);
        head.text(if self.inflight_count == 1 { b" test" } else { b" tests" });
        head.text("  \u{b7}  up to ".as_bytes());
        head.number(self.jobs);
        head.text(b" at once");
        paint(frame, color, Tone::Dim, head.bytes());
        frame.text(b"\n");
        if self.inflight_count == 0 {
            self.idle_row(color, frame);
            return 2;
        }
        let shown = self.inflight_count.min(INFLIGHT_ROWS);
        let mut offset = 0;
        for _ in 0..shown {
            let (start, limit, id, next) = self.inflight_at(offset);
            self.inflight_row(color, frame, (start, limit, id));
            offset = next;
        }
        let hidden = self.inflight_count - shown;
        if hidden != 0 {
            spaces(frame, NAME_COLUMN);
            let mut more = Line::<32>::new();
            more.text(b"+");
            more.number(hidden);
            more.text(b" more");
            paint(frame, color, Tone::Dim, more.bytes());
            frame.text(b"\n");
        }
        u16::try_from(1 + shown + usize::from(hidden != 0)).unwrap_or(u16::MAX)
    }

    fn inflight_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>, (start, limit, id): (u64, Option<u32>, Identity<'_>)) {
        let age = self.now_ms.saturating_sub(start);
        let (warn, over) = limit.map_or((age >= SLOW_MS, false), |limit| (age * 10 >= u64::from(limit) * 8, age >= u64::from(limit)));
        let tone = if over { Tone::Red } else if warn { Tone::Yellow } else { Tone::Cyan };
        let mut stamp = Line::<24>::new();
        seconds_tenths(&mut stamp, age);
        spaces(frame, 8usize.saturating_sub(columns(stamp.bytes()) + 1));
        paint(frame, color, tone, stamp.bytes());
        frame.text(b" ");
        match limit {
            Some(limit) => Self::limit_cells(color, frame, (age, limit, tone)),
            None => spaces(frame, NAME_COLUMN - 9),
        }
        ellipsis_middle(frame, id, usize::from(self.cols).saturating_sub(1 + NAME_COLUMN));
        frame.text(b"\n");
    }

    fn limit_cells<const FRAME: usize>(color: bool, frame: &mut Line<FRAME>, (age, limit, tone): (u64, u32, Tone)) {
        let mut text = Line::<16>::new();
        text.text(b"/");
        text.number(usize::try_from(limit / 1000).unwrap_or(usize::MAX));
        text.text(b"s");
        paint(frame, color, Tone::Dim, text.bytes());
        spaces(frame, 6usize.saturating_sub(columns(text.bytes())));
        let cells = LIMIT_BAR as u64;
        let filled = usize::try_from((age * cells / u64::from(limit).max(1)).min(cells)).unwrap_or(LIMIT_BAR);
        bar(frame, color, LIMIT_BAR, (filled * 8, 0), tone);
        spaces(frame, NAME_COLUMN - 15 - LIMIT_BAR);
    }
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use crate::failure_class::Exit;
    use crate::line::Line;
    use crate::render::{Event, ExcerptRows, Outcome, State, UNREAD, excerpt};
    use crate::test_alloc::measure_allocations;

    const FAILING: (&[u8], &[u8]) = (b"sched_records", b"the_host_binary_format_yields_every_declared_record_with_flags_and_group");
    const SHARD: (&[u8], &[u8]) = (b"shard_process", b"a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly");
    const PANIC: &[u8] = b"thread 'x' panicked at tests/sched_records.rs:55:9:\nassertion failed: left == right\n";

    const PARTIAL: (usize, usize) = (12, 240);
    const COMPLETE: (usize, usize) = (19, 383);

    fn midrun<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32], inflight: &'storage mut [u8], cols: u16) -> State<'storage, 200> {
        midrun_with(arena, ledger, inflight, (cols, Some(380), PARTIAL), UNREAD)
    }

    fn midrun_with<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32], inflight: &'storage mut [u8], (cols, previous, (binaries, tests)): (u16, Option<u32>, (usize, usize)), rows: ExcerptRows<'_>) -> State<'storage, 200> {
        let mut state = State::<200>::new(arena, ledger).with_inflight(inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 });
        if let Some(tests) = previous {
            state.apply(&Event::Previous { wall_ms: 258_230, tests });
        }
        state.apply(&Event::Resize { cols });
        state.apply(&Event::Built { done: 19, total: Some(19) });
        state.apply(&Event::Listed { binaries, of: Some(19), tests });
        state.apply(&Event::Tick { now_ms: 1000 });
        for _ in 0..305 {
            state.apply(&Event::Settled { id: (b"filler", b"passes"), outcome: Outcome::Passed, at_ms: None, solo: false });
        }
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        state.apply(&Event::Captured { id: FAILING, stdout: (b"", 0), stderr: (PANIC, 0), exit: Exit::Code(101), excerpt: rows });
        state.apply(&Event::Started { id: FAILING, solo: false, at_ms: 5800, limit_ms: None });
        state.apply(&Event::Started { id: SHARD, solo: false, at_ms: 8100, limit_ms: Some(120_000) });
        state.apply(&Event::Tick { now_ms: 47_100 });
        state
    }

    fn frame_at(cols: u16, color: bool) -> String {
        frame_with((cols, Some(380), PARTIAL), color)
    }

    fn frame_with(scenario: (u16, Option<u32>, (usize, usize)), color: bool) -> String {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = midrun_with(&mut arena, &mut ledger, &mut inflight, scenario, UNREAD);
        let mut frame = Line::<4096>::new();
        state.project_rich(color, &mut frame).expect("the frame holds one redraw");
        String::from_utf8(frame.bytes().to_vec()).expect("the frame is utf-8")
    }

    #[test]
    fn eighty_columns_with_colour_draw_solid_bars_in_role_colours() {
        let expected = "\x1b[?25l \x1b[1mcargo val\x1b[0m  \x1b[2m8 jobs\x1b[0m                                                         \x1b[1m0:47\x1b[0m\n\
\x1b[31m\x1b[1m FAIL 1  \x1b[0msched_records::the_\u{2026}d_with_flags_and_group  \x1b[31massertion failed: left ==\u{2026}\x1b[0m\n\
\n\
\x1b[2m build \x1b[0m██████████████████████████████ 19/19 compile units\n \
\x1b[2m──────────────────────────────────────────────────────────────────────────────\x1b[0m\n\
\x1b[2m test  \x1b[0m\x1b[32m████████████████████████\x1b[0m\x1b[36m█\x1b[0m\x1b[2m█████\x1b[0m 306/~380\x1b[2m tests\x1b[0m  \x1b[31m1 failed\x1b[0m\x1b[2m  ~ 12/19 listed\x1b[0m\n\
\x1b[2m failed at\x1b[0m                     \x1b[31m\u{2573}\x1b[0m     \n\
\n\
\x1b[2m running 2 tests  \u{b7}  up to 8 at once\x1b[0m\n  \
\x1b[33m41.3s\x1b[0m               sched_records::the_host_b\u{2026}ed_record_with_flags_and_group\n  \
\x1b[36m39.0s\x1b[0m \x1b[2m/120s\x1b[0m \x1b[36m█\x1b[0m\x1b[2m█████\x1b[0m  shard_process::a_per_test\u{2026}d_and_cargo_val_exits_promptly\n";

        assert_eq!(frame_at(80, true), expected);
    }

    #[test]
    fn eighty_columns_without_colour_keep_every_state_as_a_glyph_texture() {
        let expected = "\x1b[?25l cargo val  8 jobs                                                         0:47\n\
\x20FAIL 1  sched_records::the_\u{2026}d_with_flags_and_group  assertion failed: left ==\u{2026}\n\
\n\
\x20build ██████████████████████████████ 19/19 compile units\n\
\x20──────────────────────────────────────────────────────────────────────────────\n\
\x20test  ████████████████████████▒░░░░░ 306/~380 tests  1 failed  ~ 12/19 listed\n\
\x20failed at                     \u{2573}     \n\
\n\
\x20running 2 tests  \u{b7}  up to 8 at once\n\
\x20 41.3s               sched_records::the_host_b\u{2026}ed_record_with_flags_and_group\n\
\x20 39.0s /120s █░░░░░  shard_process::a_per_test\u{2026}d_and_cargo_val_exits_promptly\n";

        assert_eq!(frame_at(80, false), expected);
    }

    #[test]
    fn sixty_columns_drop_the_listed_note_and_ellipsise_names() {
        let expected = "\x1b[?25l cargo val  8 jobs                                     0:47\n\
\x20FAIL 1  sched_records:\u{2026}flags_and_group  assertion failed:\u{2026}\n\
\n\
\x20build ██████████████████████████████ 19/19 compile units\n\
\x20──────────────────────────────────────────────────────────\n\
\x20test  ████████████████████████▒░░░░░ 306/~380  1 failed\n\
\x20failed at                     \u{2573}     \n\
\n\
\x20running 2 tests  \u{b7}  up to 8 at once\n\
\x20 41.3s               sched_records::t\u{2026}ith_flags_and_group\n\
\x20 39.0s /120s █░░░░░  shard_process::a\u{2026}_val_exits_promptly\n";

        assert_eq!(frame_at(60, false), expected);
    }

    #[test]
    fn forty_columns_shrink_the_bars_and_keep_the_counts_whole() {
        let expected = "\x1b[?25l cargo val  8 jobs                 0:47\n\
\x20FAIL 1  sched_re\u{2026}and_group  assertion\u{2026}\n\
\n\
\x20build █████████████ 19/19 compile units\n\
\x20──────────────────────────────────────\n\
\x20test  ██████████▍▒░ 306/~380  1 failed\n\
\x20failed at       \u{2573}  \n\
\n\
\x20running 2 tests  \u{b7}  up to 8 at once\n\
\x20 41.3s               sched_r\u{2026}nd_group\n\
\x20 39.0s /120s █░░░░░  shard_p\u{2026}promptly\n";

        assert_eq!(frame_at(40, false), expected);
    }

    fn test_row(frame: &str) -> &str {
        frame.lines().find(|line| line.starts_with(" test  ")).expect("the frame has a test row")
    }

    #[test]
    fn a_previous_run_and_a_partial_listing_show_the_previous_total_with_the_listed_note_at_eighty_columns() {
        let row = " test  ████████████████████████▒░░░░░ 306/~380 tests  1 failed  ~ 12/19 listed";

        assert_eq!(test_row(&frame_with((80, Some(380), PARTIAL), false)), row);
    }

    #[test]
    fn a_previous_run_and_a_partial_listing_drop_the_note_but_keep_the_estimate_at_sixty_columns() {
        let row = " test  ████████████████████████▒░░░░░ 306/~380  1 failed";

        assert_eq!(test_row(&frame_with((60, Some(380), PARTIAL), false)), row);
    }

    #[test]
    fn a_complete_listing_shows_the_exact_total_and_no_tilde_or_note_at_eighty_columns() {
        let row = " test  ███████████████████████▉▒░░░░░  306/383 tests  1 failed";

        assert_eq!(test_row(&frame_with((80, Some(380), COMPLETE), false)), row);
    }

    #[test]
    fn a_complete_listing_shows_the_exact_total_at_sixty_columns() {
        let row = " test  ███████████████████████▉▒░░░░░  306/383  1 failed";

        assert_eq!(test_row(&frame_with((60, Some(380), COMPLETE), false)), row);
    }

    #[test]
    fn no_previous_run_and_a_partial_listing_estimate_from_the_tests_listed_so_far_at_eighty_columns() {
        let row = " test  █████████████████████████████▉ 306/~306 tests  1 failed  ~ 12/19 listed";

        assert_eq!(test_row(&frame_with((80, None, PARTIAL), false)), row);
    }

    #[test]
    fn no_previous_run_and_a_partial_listing_at_sixty_columns_drop_the_note() {
        let row = " test  █████████████████████████████▉ 306/~306  1 failed";

        assert_eq!(test_row(&frame_with((60, None, PARTIAL), false)), row);
    }

    #[test]
    fn a_colour_frame_has_no_texture_glyphs_and_no_colour_frame_has_no_escape_after_the_cursor_hide() {
        let coloured = frame_at(80, true);
        let plain = frame_at(80, false);

        assert!(!coloured.contains(['\u{2591}', '\u{2592}', '\u{2593}']));
        assert!(plain.contains('\u{2591}') && plain.contains('\u{2588}'));
        assert!(!plain["\x1b[?25l".len()..].contains('\x1b'));
    }

    #[test]
    fn a_second_redraw_moves_up_over_the_rows_the_first_drew_and_the_erase_clears_them() {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = midrun(&mut arena, &mut ledger, &mut inflight, 80);
        let mut first = Line::<4096>::new();
        state.project_rich(false, &mut first).expect("fits");
        let mut second = Line::<4096>::new();

        state.project_rich(false, &mut second).expect("fits");
        let mut erase = Line::<64>::new();
        state.erase_rich(&mut erase);

        let rows = memchr::memchr_iter(b'\n', first.bytes()).count();
        assert!(second.bytes().starts_with(format!("\x1b[{rows}A\r\x1b[J").as_bytes()));
        assert_eq!(erase.bytes(), format!("\x1b[{rows}A\r\x1b[J").as_bytes());
    }

    #[test]
    fn an_idle_frame_counts_test_binaries_while_some_are_pending_and_waits_when_none_are_known() {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Resize { cols: 80 });
        let mut unknown = Line::<4096>::new();
        state.project_rich(false, &mut unknown).expect("fits");
        state.apply(&Event::Listed { binaries: 3, of: Some(19), tests: 61 });
        let mut pending = Line::<4096>::new();
        state.project_rich(false, &mut pending).expect("fits");

        let unknown = String::from_utf8_lossy(unknown.bytes());
        let pending = String::from_utf8_lossy(pending.bytes());
        assert!(unknown.contains("   waiting for the first test binary\n"), "{unknown}");
        assert!(pending.contains("   building 3/19 test binaries\n"), "{pending}");
    }

    #[test]
    fn an_exact_test_total_has_no_space_inside_the_fraction_and_the_clock_runs_from_the_begin_event() {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 10, github: false, why: None, json: false, at_ms: 4_000 });
        state.apply(&Event::Listed { binaries: 1, of: Some(1), tests: 2 });
        state.apply(&Event::Settled { id: (b"fixture", b"settles_at_once"), outcome: Outcome::Passed, at_ms: None, solo: false });
        state.apply(&Event::Tick { now_ms: 65_000 });
        let mut frame = Line::<4096>::new();
        state.project_rich(false, &mut frame).expect("fits");

        let text = String::from_utf8_lossy(frame.bytes());
        assert!(text.contains("  1/2 tests"), "{text}");
        assert!(text.contains(" 1:01\n"), "{text}");
    }

    #[test]
    fn a_lock_wait_becomes_a_note_in_the_frame_until_a_build_unit_lands() {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 10, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Waiting { lock: b"package cache" });
        let mut waiting = Line::<4096>::new();
        state.project_rich(false, &mut waiting).expect("fits");
        state.apply(&Event::Built { done: 1, total: Some(3) });
        let mut moving = Line::<4096>::new();
        state.project_rich(false, &mut moving).expect("fits");

        assert!(String::from_utf8_lossy(waiting.bytes()).contains(" waiting for cargo's package cache lock\n"));
        assert!(!String::from_utf8_lossy(moving.bytes()).contains("waiting for cargo's"));
    }

    #[test]
    fn begin_draws_a_live_frame_before_any_previous_run_or_listing_is_known() {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        let begin = Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 };
        let mut frame = Line::<4096>::new();
        state.apply(&begin);

        state.project_live(&begin, &mut frame).expect("the frame holds one redraw");

        let drawn = String::from_utf8_lossy(frame.bytes());
        assert!(drawn.contains("searching for tests"), "{drawn}");
        assert!(!drawn.contains(" failed"), "{drawn}");
        assert!(!drawn.contains("listed"), "{drawn}");
    }

    fn predata_row(cols: u16, previous: Option<u32>) -> String {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 });
        if let Some(tests) = previous {
            state.apply(&Event::Previous { wall_ms: 258_230, tests });
        }
        state.apply(&Event::Resize { cols });
        state.apply(&Event::Tick { now_ms: 1200 });
        let mut frame = Line::<4096>::new();
        state.project_rich(false, &mut frame).expect("the frame holds one redraw");
        let drawn = String::from_utf8_lossy(frame.bytes()).into_owned();
        String::from(test_row(&drawn))
    }

    #[test]
    fn no_data_and_no_binaries_show_only_searching_for_tests_at_eighty_columns() {
        assert_eq!(predata_row(80, None), " test  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ searching for tests");
    }

    #[test]
    fn no_data_and_no_binaries_show_only_searching_for_tests_at_sixty_columns() {
        assert_eq!(predata_row(60, None), " test  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ searching for tests");
    }

    #[test]
    fn a_previous_run_with_no_binaries_still_shows_only_searching_for_tests_at_eighty_columns() {
        assert_eq!(predata_row(80, Some(383)), " test  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ searching for tests");
    }

    #[test]
    fn a_previous_run_with_no_binaries_still_shows_only_searching_for_tests_at_sixty_columns() {
        assert_eq!(predata_row(60, Some(383)), " test  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ searching for tests");
    }

    #[test]
    fn no_binaries_at_forty_columns_shrink_the_bar_and_keep_the_phrase_whole() {
        assert_eq!(predata_row(40, None), " test  ░░░░░░░░░░░░ searching for tests");
    }

    fn listing_frame(cols: u16, previous: Option<u32>, (binaries, of, tests): (usize, usize, usize)) -> String {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 });
        if let Some(count) = previous {
            state.apply(&Event::Previous { wall_ms: 258_230, tests: count });
        }
        state.apply(&Event::Resize { cols });
        state.apply(&Event::Listed { binaries, of: Some(of), tests });
        state.apply(&Event::Tick { now_ms: 1200 });
        let mut frame = Line::<4096>::new();
        state.project_rich(false, &mut frame).expect("the frame holds one redraw");
        String::from_utf8_lossy(frame.bytes()).into_owned()
    }

    #[test]
    fn nineteen_binaries_known_up_front_read_searching_nineteen_test_binaries() {
        let drawn = listing_frame(80, None, (0, 19, 0));

        assert_eq!(test_row(&drawn), format!(" test  {} searching 19 test binaries", "░".repeat(30)));
    }

    #[test]
    fn one_binary_known_up_front_reads_in_the_singular() {
        let drawn = listing_frame(80, None, (0, 1, 0));

        assert_eq!(test_row(&drawn), format!(" test  {} searching 1 test binary", "░".repeat(30)));
    }

    #[test]
    fn three_hundred_twenty_two_binaries_with_none_listed_and_no_previous_run_print_no_zero_estimate() {
        let drawn = listing_frame(190, None, (0, 322, 0));

        assert_eq!(test_row(&drawn), format!(" test  {} searching 322 test binaries", "░".repeat(30)));
        assert!(!drawn.contains("~0"), "{drawn}");
        assert!(!drawn.contains("building 0/322"), "{drawn}");
        assert!(drawn.contains("waiting for the first test binary"), "{drawn}");
    }

    #[test]
    fn a_previous_run_does_not_bring_the_count_back_before_the_first_binary_is_listed() {
        let drawn = listing_frame(190, Some(383), (0, 322, 0));

        assert_eq!(test_row(&drawn), format!(" test  {} searching 322 test binaries", "░".repeat(30)));
    }

    #[test]
    fn listed_binaries_that_hold_no_tests_yet_keep_the_searching_row_with_the_same_total() {
        let drawn = listing_frame(190, None, (2, 322, 0));

        assert_eq!(test_row(&drawn), format!(" test  {} searching 322 test binaries", "░".repeat(30)));
        assert!(drawn.contains("building 2/322 test binaries"), "{drawn}");
    }

    #[test]
    fn once_tests_are_listed_with_no_previous_run_the_estimate_is_the_listed_count_and_the_note_keeps_the_same_total() {
        let drawn = listing_frame(190, None, (4, 322, 57));
        let row = test_row(&drawn);

        assert!(row.contains(" 0/~57 tests  0 failed  ~ 4/322 listed"), "{row}");
        assert!(!row.contains("~0"), "{row}");
    }

    #[test]
    fn a_shard_frame_lists_two_tests_in_flight_as_age_then_name_rows_and_a_settle_emits_no_result_line() {
        const PASSES: (&[u8], &[u8]) = (b"cli_run", b"fixture::passes");
        const IGNORED: (&[u8], &[u8]) = (b"cli_run", b"fixture::ignored_by_default");
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = State::<200>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: true, jobs: 10, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Resize { cols: 80 });
        state.apply(&Event::Started { id: PASSES, solo: false, at_ms: 0, limit_ms: None });
        state.apply(&Event::Started { id: IGNORED, solo: false, at_ms: 0, limit_ms: None });
        let tick = Event::Tick { now_ms: 12_000 };
        state.apply(&tick);
        let mut frame = Line::<4096>::new();

        state.project_live(&tick, &mut frame).expect("the frame holds one redraw");

        let drawn = String::from_utf8_lossy(frame.bytes()).into_owned();
        let rows: Vec<&str> = drawn.lines().filter(|line| line.contains("fixture::")).collect();
        assert!(drawn.contains(" running 2 tests  \u{b7}  up to 10 at once\n"), "{drawn}");
        assert_eq!(rows.len(), 2, "{drawn}");
        for name in ["cli_run::fixture::passes", "cli_run::fixture::ignored_by_default"] {
            assert!(rows.iter().any(|row| row.contains("12.0s") && row.ends_with(name)), "{name} is an in-flight row, age then name:\n{drawn}");
        }
        let settled = Event::Settled { id: PASSES, outcome: Outcome::Passed, at_ms: Some(12_500), solo: false };
        state.apply(&settled);
        let mut after_settle = Line::<512>::new();
        state.project_live(&settled, &mut after_settle).expect("a settle projects");
        assert_eq!(after_settle.bytes(), b"", "shard mode prints no PASS line for a settled test; the frame is the only surface");
    }

    #[test]
    fn ten_thousand_rich_frames_at_each_width_allocate_nothing() {
        let scenarios = [(80_u16, Some(380), PARTIAL), (120, Some(380), PARTIAL), (200, Some(380), PARTIAL), (80, None, PARTIAL), (80, Some(380), COMPLETE), (60, None, COMPLETE)];
        let mut total_frames = 0_usize;
        for scenario in scenarios {
            let cols = scenario.0;
            let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
            let mut state = midrun_with(&mut arena, &mut ledger, &mut inflight, scenario, UNREAD);
            let mut frame = Line::<16_384>::new();
            let mut frames = 0_usize;

            let ((), allocations) = measure_allocations(|| {
                for round in 0..10_000_u64 {
                    frame.clear();
                    state.apply(&Event::Tick { now_ms: 47_100 + round });
                    state.project_rich(round % 2 == 0, &mut frame).expect("the frame holds one redraw");
                    frames += 1;
                    total_frames += 1;
                }
            });

            assert_eq!(frames, 10_000, "the loop ran every frame at {cols} columns, so a zero is not an empty loop");
            assert_eq!(allocations, 0, "{cols} columns");
            assert!(frame.bytes().len() > 400, "the last frame at {cols} columns is a real frame");
        }
        assert_eq!(total_frames, 60_000, "every scenario ran its 10k frames");
    }

    const SOURCE_ROWS: &[u8] = b"    52 |     let records = host_binary_records(&binary);\n    53 |     assert_eq!(records.len(), expected.len());\n    54 |     assert_eq!(records.flags(), expected.flags());\n>   55 |     assert_eq!(records.group(), expected.group());\n      |     ^^^^^^^^^ panicked here\n";
    const STYLED_ROWS: &[u8] = b">   55 | \x1b[35massert_eq!\x1b[0m(records.group(), expected.group());\n";

    fn pinned_frame(cols: u16, color: bool, rows: ExcerptRows<'_>) -> (String, u16) {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 2048], [0_u32; 8], [0_u8; 1024]);
        let mut state = midrun_with(&mut arena, &mut ledger, &mut inflight, (cols, Some(380), PARTIAL), rows);
        let mut frame = Line::<4096>::new();
        state.project_rich(color, &mut frame).expect("the frame holds one redraw");
        (String::from_utf8(frame.bytes().to_vec()).expect("the frame is utf-8"), state.drawn_rows)
    }

    #[test]
    fn the_pinned_failure_shows_the_stored_excerpt_rows_at_eighty_and_forty_columns() {
        let (wide, _) = pinned_frame(80, false, Ok(SOURCE_ROWS));
        let (narrow, _) = pinned_frame(40, false, Ok(SOURCE_ROWS));

        let wide_expected = " FAIL 1  sched_records::the_\u{2026}d_with_flags_and_group  assertion failed: left ==\u{2026}\n     52 |     let records = host_binary_records(&binary);\n     53 |     assert_eq!(records.len(), expected.len());\n     54 |     assert_eq!(records.flags(), expected.flags());\n >   55 |     assert_eq!(records.group(), expected.group());\n       |     ^^^^^^^^^ panicked here\n\n";
        let narrow_expected = " FAIL 1  sched_re\u{2026}and_group  assertion\u{2026}\n     52 |     let records = host_binar\u{2026}\n     53 |     assert_eq!(records.len()\u{2026}\n     54 |     assert_eq!(records.flags\u{2026}\n >   55 |     assert_eq!(records.group\u{2026}\n       |     ^^^^^^^^^ panicked here\n\n";
        assert!(wide.contains(wide_expected), "{wide}");
        assert!(narrow.contains(narrow_expected), "{narrow}");
    }

    #[test]
    fn the_erase_moves_up_exactly_the_rows_a_frame_with_excerpt_rows_drew() {
        let mut checked = 0;
        for cols in [80_u16, 40] {
            for color in [false, true] {
                let (frame, drawn) = pinned_frame(cols, color, Ok(SOURCE_ROWS));
                let (bare, bare_drawn) = pinned_frame(cols, color, UNREAD);

                assert_eq!(usize::from(drawn), frame.matches('\n').count(), "{cols} columns, colour {color}");
                assert_eq!(drawn, bare_drawn + 5, "five excerpt rows join the bare frame at {cols} columns");
                assert_eq!(usize::from(bare_drawn), bare.matches('\n').count());
                checked += 1;
            }
        }
        assert_eq!(checked, 4);
    }

    #[test]
    fn the_erase_matches_the_drawn_rows_when_an_excerpt_has_tabs_and_wide_glyphs_at_forty_columns() {
        let rows = "    52 | \tlet name = \"日本語のテスト名前\";\n>   55 | \t\tassert!(\"漢字\".len() == 6);\n      | \t\t^^^^^^^^^ panicked here\n".as_bytes();
        let (frame, drawn) = pinned_frame(40, false, Ok(rows));
        let (_, bare_drawn) = pinned_frame(40, false, UNREAD);

        assert_eq!(usize::from(drawn), frame.matches('\n').count(), "{frame}");
        assert_eq!(drawn, bare_drawn + 3, "three excerpt rows join the bare frame");
        let widest = frame.lines().map(|line| excerpt::visible(line.as_bytes())).max().expect("the frame has rows");
        assert!(widest <= 40, "the widest drawn row is {widest} columns\n{frame}");
        let excerpt_rows: Vec<&str> = frame.lines().filter(|line| line.contains(" | ")).collect();
        assert_eq!(excerpt_rows.len(), 3, "{frame}");
        assert!(excerpt_rows.iter().all(|line| excerpt::visible(line.as_bytes()) <= 39), "{excerpt_rows:?}");
    }

    #[test]
    fn a_plain_screen_drops_the_stored_sgr_and_a_colour_screen_keeps_and_closes_it() {
        let (plain, _) = pinned_frame(80, false, Ok(STYLED_ROWS));
        let (colored, _) = pinned_frame(80, true, Ok(STYLED_ROWS));

        let after_hide = plain.strip_prefix("\x1b[?25l").expect("the frame opens by hiding the cursor");
        assert_eq!(after_hide.bytes().filter(|byte| *byte == 0x1b).count(), 0, "{plain}");
        assert!(plain.contains(" >   55 | assert_eq!(records.group(), expected.group());\n"), "{plain}");
        assert!(colored.contains(" >   55 | \x1b[35massert_eq!\x1b[0m(records.group(), expected.group());\x1b[0m\n"), "{colored}");
    }

    #[test]
    fn ten_thousand_frames_with_excerpt_rows_at_each_width_allocate_nothing() {
        for cols in [80_u16, 40, 200] {
            let (mut arena, mut ledger, mut inflight) = ([0_u8; 2048], [0_u32; 8], [0_u8; 1024]);
            let mut state = midrun_with(&mut arena, &mut ledger, &mut inflight, (cols, Some(380), PARTIAL), Ok(SOURCE_ROWS));
            let mut frame = Line::<16_384>::new();
            let mut frames = 0_usize;

            let ((), allocations) = measure_allocations(|| {
                for round in 0..10_000_u64 {
                    frame.clear();
                    state.apply(&Event::Tick { now_ms: 47_100 + round });
                    state.project_rich(round % 2 == 0, &mut frame).expect("the frame holds one redraw");
                    frames += 1;
                }
            });

            assert_eq!(frames, 10_000, "the loop ran every frame at {cols} columns, so a zero is not an empty loop");
            assert_eq!(allocations, 0, "{cols} columns");
            assert!(String::from_utf8_lossy(frame.bytes()).contains("panicked here"), "the last frame at {cols} columns drew the excerpt");
        }
    }

    fn keyed_state<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32], inflight: &'storage mut [u8], (cols, failing, keys): (u16, bool, bool)) -> State<'storage, 200> {
        let mut state = State::<200>::new(arena, ledger).with_inflight(inflight);
        state.set_keys_live(keys);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Resize { cols });
        state.apply(&Event::Built { done: 19, total: Some(19) });
        state.apply(&Event::Listed { binaries: 12, of: Some(19), tests: 240 });
        if failing {
            state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
            state.apply(&Event::Captured { id: FAILING, stdout: (b"", 0), stderr: (PANIC, 0), exit: Exit::Code(101), excerpt: UNREAD });
        }
        state.apply(&Event::Started { id: SHARD, solo: false, at_ms: 8100, limit_ms: Some(120_000) });
        state.apply(&Event::Tick { now_ms: 47_100 });
        state
    }

    fn keyed_frame(scenario: (u16, bool, bool), color: bool) -> (String, u16) {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = keyed_state(&mut arena, &mut ledger, &mut inflight, scenario);
        let mut frame = Line::<4096>::new();
        state.project_rich(color, &mut frame).expect("the frame holds one redraw");
        (String::from_utf8(frame.bytes().to_vec()).expect("the frame is utf-8"), state.drawn_rows)
    }

    fn last_rows(frame: &str, count: usize) -> Vec<&str> {
        let rows: Vec<&str> = frame.lines().collect();
        rows[rows.len() - count..].to_vec()
    }

    #[test]
    fn a_live_frame_with_no_failure_ends_in_the_timeline_and_cancel_keys_at_eighty_columns() {
        let (frame, _) = keyed_frame((80, false, true), false);

        assert_eq!(last_rows(&frame, 2), ["", " s timeline  q cancel"], "{frame}");
    }

    #[test]
    fn a_live_frame_with_no_failure_ends_in_the_timeline_and_cancel_keys_at_sixty_columns() {
        let (frame, _) = keyed_frame((60, false, true), false);

        assert_eq!(last_rows(&frame, 2), ["", " s timeline  q cancel"], "{frame}");
    }

    #[test]
    fn a_live_frame_with_one_failure_pins_the_review_hint_and_ends_in_the_full_key_row_at_eighty_columns() {
        let (frame, _) = keyed_frame((80, true, true), false);

        assert_eq!(frame.lines().nth(1), Some(" FAIL 1  sched_records::t\u{2026}ith_flags_and_group  assertion failed: lef\u{2026}  f review"), "{frame}");
        assert_eq!(last_rows(&frame, 2), ["", " f review failures  s timeline  q cancel"], "{frame}");
    }

    #[test]
    fn a_live_frame_with_one_failure_pins_the_review_hint_and_ends_in_the_full_key_row_at_sixty_columns() {
        let (frame, _) = keyed_frame((60, true, true), false);

        assert_eq!(frame.lines().nth(1), Some(" FAIL 1  sched_recor\u{2026}gs_and_group  assertion fai\u{2026}  f review"), "{frame}");
        assert_eq!(last_rows(&frame, 2), ["", " f review failures  s timeline  q cancel"], "{frame}");
    }

    #[test]
    fn the_key_row_drops_hints_from_the_right_and_never_cuts_a_word() {
        let widths = [(40_u16, " f review failures  s timeline"), (30, " f review failures"), (18, " ")];
        for (cols, expected) in widths {
            let (frame, _) = keyed_frame((cols, true, true), false);

            assert_eq!(last_rows(&frame, 1), [expected], "{cols} columns\n{frame}");
        }
    }

    #[test]
    fn the_key_row_is_dim_with_colour_and_has_no_escape_without_it() {
        let (coloured, _) = keyed_frame((80, true, true), true);
        let (plain, _) = keyed_frame((80, true, true), false);

        assert!(coloured.ends_with(" \x1b[2mf review failures  s timeline  q cancel\x1b[0m\n"), "{coloured}");
        assert!(coloured.contains("\x1b[2mf review\x1b[0m\n"), "{coloured}");
        assert!(!plain["\x1b[?25l".len()..].contains('\x1b'), "{plain}");
    }

    #[test]
    fn an_armed_quit_replaces_the_key_row_with_the_confirmation_prompt() {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = keyed_state(&mut arena, &mut ledger, &mut inflight, (80, true, true));
        state.apply(&Event::Key(crate::render::review::Key::Char(b'q')));
        let mut frame = Line::<4096>::new();

        state.project_rich(false, &mut frame).expect("the frame holds one redraw");

        let drawn = String::from_utf8_lossy(frame.bytes()).into_owned();
        assert_eq!(last_rows(&drawn, 1), [" q again to confirm, any key cancels"], "{drawn}");
    }

    #[test]
    fn without_a_live_key_lane_there_is_no_key_row_and_no_pinned_hint() {
        let (frame, _) = keyed_frame((80, true, false), false);

        assert!(!frame.contains("f review") && !frame.contains("q cancel"), "{frame}");
        assert!(frame.ends_with('\n') && !frame.ends_with("\n\n"), "no blank row trails the in-flight rows:\n{frame}");
    }

    #[test]
    fn the_erase_row_count_includes_the_key_row_exactly() {
        let mut checked = 0;
        for cols in [80_u16, 60, 40] {
            for failing in [false, true] {
                let (with_keys, drawn) = keyed_frame((cols, failing, true), false);
                let (without_keys, bare) = keyed_frame((cols, failing, false), false);

                assert_eq!(usize::from(drawn), with_keys.matches('\n').count(), "{cols} columns, failing {failing}");
                assert_eq!(drawn, bare + 2, "the blank row and the key row join the frame at {cols} columns");
                assert_eq!(usize::from(bare), without_keys.matches('\n').count());
                checked += 1;
            }
        }
        assert_eq!(checked, 6);
    }

    #[test]
    fn ten_thousand_key_frames_at_each_width_allocate_nothing() {
        let mut total_frames = 0_usize;
        for cols in [80_u16, 60, 40] {
            let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
            let mut state = keyed_state(&mut arena, &mut ledger, &mut inflight, (cols, true, true));
            let mut frame = Line::<16_384>::new();

            let ((), allocations) = measure_allocations(|| {
                for round in 0..10_000_u64 {
                    frame.clear();
                    state.apply(&Event::Tick { now_ms: 47_100 + round });
                    state.project_rich(round % 2 == 0, &mut frame).expect("the frame holds one redraw");
                    total_frames += 1;
                }
            });

            assert_eq!(allocations, 0, "{cols} columns");
            assert!(String::from_utf8_lossy(frame.bytes()).contains("f review"), "the last frame at {cols} columns drew the key row");
        }
        assert_eq!(total_frames, 30_000, "every width ran its 10k frames");
    }
}
