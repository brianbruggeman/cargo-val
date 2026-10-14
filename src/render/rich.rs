//! The rich projection of the run frame: the third reading of the one render [`State`], `(state, color) -> bytes`.
//!
//! Composes [`Line`] only; width, the clock and the colour choice are data already in the state or passed in, so the
//! frame is a pure function of them and allocates nothing. The in-place redraw prefix (cursor up over the rows the
//! previous frame drew, then clear to the end of the screen) is part of the bytes, so one redraw is one write.
//! Colour roles are the terminal's own 16 ANSI colours; without colour the bars keep their state as glyph textures.

use crate::line::Line;
use crate::render::{Event, Identity, Mode, State};
use crate::view_defaults::{BAR_COLUMNS, INFLIGHT_ROWS};

const GREEN: &[u8] = b"\x1b[32m";
const RED: &[u8] = b"\x1b[31m";
const YELLOW: &[u8] = b"\x1b[33m";
const CYAN: &[u8] = b"\x1b[36m";
const DIM: &[u8] = b"\x1b[2m";
const BOLD: &[u8] = b"\x1b[1m";
const RESET: &[u8] = b"\x1b[0m";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
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

fn paint<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, tone: Tone, text: &[u8]) {
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

fn bold<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, text: &[u8]) {
    if color {
        frame.text(BOLD);
    }
    frame.text(text);
    if color {
        frame.text(RESET);
    }
}

fn run<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, tone: Tone, (glyph, count): (&str, usize)) {
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

fn spaces<const FRAME: usize>(frame: &mut Line<FRAME>, count: usize) {
    for _ in 0..count {
        frame.text(b" ");
    }
}

fn digits(value: usize) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

fn padded<const FRAME: usize>(frame: &mut Line<FRAME>, value: usize, width: usize) {
    spaces(frame, width.saturating_sub(digits(value)));
    frame.number(value);
}

fn columns(text: &[u8]) -> usize {
    text.iter().filter(|byte| (**byte & 0xc0) != 0x80).count()
}

fn boundary(text: &[u8], mut at: usize) -> usize {
    while at > 0 && at < text.len() && (text[at] & 0xc0) == 0x80 {
        at -= 1;
    }
    at
}

fn ellipsis_end<const FRAME: usize>(frame: &mut Line<FRAME>, text: &[u8], width: usize) {
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
        match self.binaries_total {
            Some(total) if self.listed_binaries >= total => (self.listed_tests, true),
            Some(total) if self.listed_binaries != 0 => (self.listed_tests * total / self.listed_binaries, false),
            _ => (self.listed_tests, false),
        }
    }

    fn note_width(&self) -> usize {
        4 + digits(self.listed_binaries) + digits(self.binaries_total.unwrap_or(0)) + 7
    }

    fn room_for_bar(&self, (with_note, with_word): (bool, bool)) -> usize {
        let avail = usize::from(self.cols).saturating_sub(1);
        let word = if with_word { 6 } else { 0 };
        let note = if with_note { self.note_width() } else { 0 };
        avail.saturating_sub(BAR_COLUMN + 1 + 8 + word + 2 + digits(self.failed_count()) + 7 + note)
    }

    /// Narrowing order: the listed note goes first, then the word `tests`, then the bars shrink, then names ellipsise.
    fn layout(&self) -> (usize, bool, bool) {
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
        match *event {
            Event::Tick { .. } if self.mode == Mode::Running => self.project_rich(self.color, frame),
            Event::Begin { .. } | Event::Waiting { .. } => self.project_rich(self.color, frame),
            Event::Captured { .. } | Event::Warned { .. } | Event::BuildFailed { .. } | Event::Interrupted { .. } | Event::Finished { .. } => {
                self.erase_rich(frame);
                self.project(event, frame)
            }
            Event::Tick { .. } | Event::Running { .. } | Event::Exited { .. } | Event::Listed { .. } | Event::Built { .. } | Event::Resize { .. } | Event::Started { .. } | Event::Settled { .. } | Event::BlockOpened { .. } | Event::BlockClosed => Ok(()),
        }
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
        self.pinned_row(color, frame);
        frame.text(b"\n");
        self.build_row(color, frame, layout.0);
        self.rule_row(color, frame);
        self.test_row(color, frame, layout);
        let mut rows = 6;
        if self.failed_count() != 0 {
            self.tick_row(color, frame, layout.0);
            rows += 1;
        }
        frame.text(b"\n");
        rows + 1 + self.inflight_rows(color, frame)
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

    fn pinned_row<const FRAME: usize>(&self, color: bool, frame: &mut Line<FRAME>) {
        let Some(first) = self.failures().next() else {
            self.lock_note(color, frame);
            return;
        };
        let mut head = Line::<32>::new();
        head.text(b" FAIL ");
        head.number(self.failed_count());
        head.text(b"  ");
        if color {
            frame.text(RED);
        }
        bold(frame, color, head.bytes());
        let room = usize::from(self.cols).saturating_sub(1 + columns(head.bytes()));
        let reason = &self.reason[..self.reason_len];
        let name_room = if reason.is_empty() { room } else { (room * 3 / 5).max(12).min(room) };
        let used = ellipsis_middle(frame, first, name_room);
        if !reason.is_empty() && room > used + 3 {
            frame.text(b"  ");
            let mut tail = Line::<256>::new();
            ellipsis_end(&mut tail, reason, room - used - 2);
            paint(frame, color, Tone::Red, tail.bytes());
        }
        frame.text(b"\n");
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
        frame.text(b" ");
        padded(frame, self.done, 3);
        frame.text(if exact { b"/" } else { b"/~" });
        frame.number(total);
        if with_word {
            paint(frame, color, Tone::Dim, b" tests");
        }
        frame.text(b"  ");
        let mut count = Line::<24>::new();
        count.number(failed);
        count.text(b" failed");
        paint(frame, color, if failed == 0 { Tone::Dim } else { Tone::Red }, count.bytes());
        if with_note && !exact {
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
            Some(total) if self.listed_binaries < total => {
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

    use crate::failure_class::Exit;
    use crate::line::Line;
    use crate::render::{Event, Outcome, State, UNREAD};
    use crate::test_alloc::measure_allocations;

    const FAILING: (&[u8], &[u8]) = (b"sched_records", b"the_host_binary_format_yields_every_declared_record_with_flags_and_group");
    const SHARD: (&[u8], &[u8]) = (b"shard_process", b"a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly");
    const PANIC: &[u8] = b"thread 'x' panicked at tests/sched_records.rs:55:9:\nassertion failed: left == right\n";

    fn midrun<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32], inflight: &'storage mut [u8], cols: u16) -> State<'storage, 200> {
        let mut state = State::<200>::new(arena, ledger).with_inflight(inflight);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Resize { cols });
        state.apply(&Event::Built { done: 19, total: Some(19) });
        state.apply(&Event::Listed { binaries: 12, of: Some(19), tests: 240 });
        state.apply(&Event::Tick { now_ms: 1000 });
        for _ in 0..305 {
            state.apply(&Event::Settled { id: (b"filler", b"passes"), outcome: Outcome::Passed, at_ms: None, solo: false });
        }
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        state.apply(&Event::Captured { id: FAILING, stdout: (b"", 0), stderr: (PANIC, 0), exit: Exit::Code(101), excerpt: UNREAD });
        state.apply(&Event::Started { id: FAILING, solo: false, at_ms: 5800, limit_ms: None });
        state.apply(&Event::Started { id: SHARD, solo: false, at_ms: 8100, limit_ms: Some(120_000) });
        state.apply(&Event::Tick { now_ms: 47_100 });
        state
    }

    fn frame_at(cols: u16, color: bool) -> String {
        let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
        let mut state = midrun(&mut arena, &mut ledger, &mut inflight, cols);
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
    fn ten_thousand_rich_frames_at_each_width_allocate_nothing() {
        for cols in [80_u16, 120, 200] {
            let (mut arena, mut ledger, mut inflight) = ([0_u8; 1024], [0_u32; 8], [0_u8; 1024]);
            let mut state = midrun(&mut arena, &mut ledger, &mut inflight, cols);
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
            assert!(frame.bytes().len() > 400, "the last frame at {cols} columns is a real frame");
        }
    }
}
