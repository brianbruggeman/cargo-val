//! The timeline projection: how many tests ran at once over the run, `(state, width) -> bytes` into the caller's frame.
//!
//! Composes [`Line`] and the colour helpers of the rich projection; it reads the per-test start and settle times the
//! one render [`State`] keeps in its timeline arena and nothing else, so it allocates nothing and reads no clock. The
//! review screen's `s` key calls [`State::project_timeline`]; `--timeline` in plain mode calls the same function after
//! the closing block. Colour draws solid blocks in role colours, failing over passing; without colour (`NO_COLOR`, or plain output) one solid layer draws the total.

use crate::line::Line;
use crate::render::rich::{Tone, boundary, columns, ellipsis_end, paint, spaces};
use crate::render::{Identity, State, TIMED_HEAD, names, seconds};

const LEFT: usize = 9;
const MAX_CHART: usize = 66;
const MIN_WIDTH: usize = 40;
const COMPACT_WIDTH: usize = 60;
const PLOT_ROWS: usize = 8;
const PLOT_ROWS_EIGHTHS: u8 = 64;
const PLOT_EIGHTHS: u64 = PLOT_ROWS_EIGHTHS as u64;
const LONGEST: usize = 3;
const TICK_GAP: u64 = 8;
const TICK_STEPS: [u64; 9] = [1, 5, 10, 30, 60, 300, 600, 1800, 3600];
const SENTENCE: usize = 256;
const LIVE_FLOOR_MS: u64 = 60_000;
const LOWER: [&str; 9] = [" ", "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}", "\u{2585}", "\u{2586}", "\u{2587}", "\u{2588}"];
const BLOCK: &str = "\u{2588}";
const TICK: &str = "\u{252c}";
const RULE: &str = "\u{2500}";
const END: &str = "\u{2524}";

/// One settled test as the timeline arena stores it: start and end in milliseconds since `Begin`, whether it failed, its borrowed id.
type Timed<'a> = (u64, u64, bool, Identity<'a>);

const fn duration(timed: &Timed<'_>) -> u64 {
    timed.1 - timed.0
}

impl<const RERUN: usize> State<'_, RERUN> {
    fn timed_at(&self, offset: usize) -> (Timed<'_>, usize) {
        let mut word = [0; 4];
        word.copy_from_slice(&self.timed[offset..offset + 4]);
        let start = u64::from(u32::from_le_bytes(word));
        word.copy_from_slice(&self.timed[offset + 4..offset + 8]);
        let end = u64::from(u32::from_le_bytes(word));
        let (id, consumed) = names(&self.timed[offset + TIMED_HEAD..self.timed_used]);
        let timed = (start, end.max(start), self.timed[offset + 8] != 0, id);
        (timed, offset + TIMED_HEAD + consumed)
    }

    fn timed_records(&self) -> impl Iterator<Item = Timed<'_>> {
        let mut offset = 0;
        core::iter::from_fn(move || {
            (offset < self.timed_used).then(|| {
                let (timed, next) = self.timed_at(offset);
                offset = next;
                timed
            })
        })
    }

    /// Each in-flight test as `(start, now)` in milliseconds since `Begin`; empty once the run has finished, so the finished plot is the settled-only plot.
    fn running_spans(&self) -> impl Iterator<Item = (u64, u64)> {
        let origin = self.origin_ms.unwrap_or(self.now_ms);
        let now = self.elapsed_ms();
        let mut offset = 0;
        core::iter::from_fn(move || {
            (self.wall_ms.is_none() && offset < self.inflight_used).then(|| {
                let (started, _, _, next) = self.inflight_at(offset);
                offset = next;
                (started.saturating_sub(origin).min(now), now)
            })
        })
    }

    fn elapsed_ms(&self) -> u64 {
        self.now_ms.saturating_sub(self.origin_ms.unwrap_or(self.now_ms))
    }

    /// The axis end: the closing block's wall time once the run finished; while live, the previous run's wall time (or `LIVE_FLOOR_MS` without one), doubled each time the run crosses it so the plot never overflows.
    fn timeline_wall(&self) -> (u64, bool) {
        if let Some(wall) = self.wall_ms {
            return (wall.max(1), false);
        }
        let elapsed = self.elapsed_ms();
        let mut end = self.previous.map_or(LIVE_FLOOR_MS, |(wall, _)| wall).max(1);
        while end < elapsed {
            end = end.saturating_mul(2);
        }
        (end, true)
    }

    /// Writes the timeline of the run so far into `frame`, `width` terminal columns wide.
    ///
    /// Eight plot rows whatever `jobs` is, then the axis, the legend, one computed takeaway sentence and the three
    /// longest tests drawn on the same time scale. Below 60 columns the labels shorten; below 40 one line says so.
    /// The run's own colour choice applies: with colour off, one solid layer draws the total of passing and failing tests.
    ///
    /// # Errors
    ///
    /// Returns the number of bytes cut when the frame was too small.
    pub fn project_timeline<const FRAME: usize>(&self, width: usize, frame: &mut Line<FRAME>) -> Result<(), usize> {
        if width < MIN_WIDTH {
            frame.text(b"timeline needs ");
            frame.number(MIN_WIDTH);
            frame.text(b" columns, have ");
            frame.number(width);
            frame.text(b"\n");
            return Self::checked(frame);
        }
        let chart = (width - LEFT - 1).min(MAX_CHART);
        let (wall, live) = self.timeline_wall();
        self.timeline_title(frame, width, if live { self.elapsed_ms().max(1) } else { wall }, live);
        self.timeline_plot(frame, chart, wall);
        Self::timeline_axis(frame, (chart, wall), live);
        self.timeline_legend(frame, width);
        self.timeline_takeaway(frame, width, (wall, live));
        self.timeline_longest(frame, width, (chart, wall));
        Self::checked(frame)
    }

    fn timeline_title<const FRAME: usize>(&self, frame: &mut Line<FRAME>, width: usize, wall: u64, live: bool) {
        let mut right = Line::<48>::new();
        right.text(if live { b"so far " } else { b"wall " });
        seconds(&mut right, wall);
        let compact = width < COMPACT_WIDTH;
        let left: &[u8] = if compact { b" where the time went" } else { b" cargo val where the time went" };
        frame.text(left);
        spaces(frame, width.saturating_sub(columns(left) + columns(right.bytes()) + 1));
        frame.text(right.bytes());
        frame.text(b"\n ");
        frame.text(if compact { b"tests running at once, max " } else { b"how many tests ran at once, over time, up to " });
        frame.number(self.jobs);
        frame.text(if compact { b"\n tests at once\n" } else { b" at once\n tests at once\n" });
    }

    fn timeline_plot<const FRAME: usize>(&self, frame: &mut Line<FRAME>, chart: usize, wall: u64) {
        let mut pass = [0_u8; MAX_CHART];
        let mut stacked = [0_u8; MAX_CHART];
        self.timeline_levels(chart, wall, (&mut pass, &mut stacked));
        for row in 0..PLOT_ROWS {
            self.timeline_label(frame, row);
            let low = (PLOT_ROWS - 1 - row) * 8;
            for column in 0..chart {
                self.timeline_cell(frame, low, (pass[column], stacked[column]));
            }
            frame.text(b"\n");
        }
    }

    fn timeline_label<const FRAME: usize>(&self, frame: &mut Line<FRAME>, row: usize) {
        let label = match row {
            0 => Some(self.jobs),
            4 => Some(self.jobs / 2),
            7 => Some(0),
            _ => None,
        };
        let mut text = Line::<20>::new();
        if let Some(value) = label {
            text.number(value);
        }
        spaces(frame, LEFT - 1 - text.bytes().len());
        paint(frame, self.color, Tone::Dim, text.bytes());
        frame.text(b" ");
    }

    /// Per column: eighths of plot height held by passing tests, and by passing plus failing tests.
    fn timeline_levels(&self, chart: usize, wall: u64, (pass, stacked): (&mut [u8; MAX_CHART], &mut [u8; MAX_CHART])) {
        let mut overlap = [[0_u64; MAX_CHART]; 2];
        for (start, stop, failed, _) in self.timed_records() {
            hold(&mut overlap[usize::from(failed)], (start, stop), (chart, wall));
        }
        for (start, now) in self.running_spans() {
            hold(&mut overlap[0], (start, now), (chart, wall));
        }
        let jobs = self.jobs.max(1) as u64;
        for column in 0..chart {
            let (low, high) = slice_of(column, chart, wall);
            let span = (high - low).max(1);
            let eighths = |held_ms: u64| u8::try_from((held_ms * PLOT_EIGHTHS).div_ceil(jobs * span)).unwrap_or(u8::MAX).min(PLOT_ROWS_EIGHTHS);
            pass[column] = eighths(overlap[0][column]);
            stacked[column] = eighths(overlap[0][column] + overlap[1][column]).max(pass[column]);
        }
    }

    fn timeline_cell<const FRAME: usize>(&self, frame: &mut Line<FRAME>, low: usize, (pass, stacked): (u8, u8)) {
        let fill = usize::from(stacked).saturating_sub(low).min(8);
        if fill == 0 {
            frame.text(b" ");
            return;
        }
        let passing = usize::from(pass).saturating_sub(low).min(fill);
        let failing = fill - passing >= passing;
        paint(frame, self.color, if failing { Tone::Red } else { Tone::Green }, LOWER[fill].as_bytes());
    }

    fn timeline_axis<const FRAME: usize>(frame: &mut Line<FRAME>, (chart, wall): (usize, u64), live: bool) {
        let step = usize::try_from(tick_step(chart, wall)).unwrap_or(1);
        let mut ticks = [false; MAX_CHART];
        for tick in (0..=wall / 1000).step_by(step) {
            ticks[column_of(tick * 1000, chart, wall)] = true;
        }
        spaces(frame, LEFT);
        for (column, tick) in ticks.iter().enumerate().take(chart) {
            let glyph = if column + 1 == chart { END } else if *tick { TICK } else { RULE };
            frame.text(glyph.as_bytes());
        }
        frame.text(b"\n seconds ");
        let mut end = Line::<24>::new();
        if live {
            end.text(b"~");
        }
        seconds(&mut end, wall);
        let reserved = chart.saturating_sub(columns(end.bytes()) + 1);
        let mut written = 0;
        for tick in (0..=wall / 1000).step_by(step) {
            let mut label = Line::<24>::new();
            label.number(usize::try_from(tick).unwrap_or(0));
            label.text(b"s");
            let column = column_of(tick * 1000, chart, wall);
            if column >= written && column + label.bytes().len() <= reserved {
                spaces(frame, column - written);
                frame.text(label.bytes());
                written = column + label.bytes().len();
            }
        }
        spaces(frame, chart.saturating_sub(columns(end.bytes()) + written));
        frame.text(end.bytes());
        frame.text(b"\n");
    }

    fn timeline_legend<const FRAME: usize>(&self, frame: &mut Line<FRAME>, width: usize) {
        let compact = width < COMPACT_WIDTH;
        frame.text(b" ");
        paint(frame, self.color, Tone::Green, BLOCK.as_bytes());
        if !self.color {
            frame.text(b" tests running\n");
            return;
        }
        frame.text(if compact { b" pass  " } else { b" passing tests  " });
        paint(frame, self.color, Tone::Red, BLOCK.as_bytes());
        frame.text(if compact { b" fail\n" } else { b" failing tests\n" });
    }

    fn timeline_takeaway<const FRAME: usize>(&self, frame: &mut Line<FRAME>, width: usize, (wall, live): (u64, bool)) {
        let mut sentence = Line::<SENTENCE>::new();
        if live {
            self.live_sentence(&mut sentence);
        } else {
            self.final_sentence(&mut sentence, wall);
        }
        wrap(frame, sentence.bytes(), width - 2);
        if self.untimed != 0 {
            frame.text(b" +");
            frame.number(self.untimed);
            frame.text(b" tests not tracked\n");
        }
    }

    fn live_sentence<const LEN: usize>(&self, sentence: &mut Line<LEN>) {
        sentence.text(b"Running now: ");
        sentence.number(self.inflight_count);
        sentence.text(if self.inflight_count == 1 { b" test." } else { b" tests." });
        if self.peak != 0 {
            sentence.text(b" Peak so far: ");
            self.peak_clause(sentence);
        }
        sentence.text(b" The summary appears when the run ends.");
    }

    fn peak_clause<const LEN: usize>(&self, sentence: &mut Line<LEN>) {
        sentence.number(self.peak);
        sentence.text(b" at once at ");
        seconds(sentence, self.peak_ms);
        sentence.text(b".");
    }

    /// After `busy_ms` no more than `tail_limit` tests ran at once; `tail` of them were still running then, for the rest of the wall time.
    fn final_sentence<const LEN: usize>(&self, sentence: &mut Line<LEN>, wall: u64) {
        let (tail, slowest) = self.timeline_tail();
        if self.timed_count == 0 {
            sentence.text(b"No settled test has a recorded time.");
            return;
        }
        if self.peak <= self.tail_limit() {
            self.under_subscribed_sentence(sentence);
            return;
        }
        let lasted = wall.saturating_sub(self.busy_ms);
        if tail == 0 {
            sentence.text(b"No long tail: more than ");
            sentence.number(self.tail_limit());
            sentence.text(b" tests ran at once until the last one finished at ");
            seconds(sentence, self.busy_ms);
            sentence.text(b".");
        } else {
            sentence.text(b"After ");
            seconds(sentence, self.busy_ms);
            sentence.text(b" only ");
            sentence.number(tail);
            sentence.text(if tail == 1 { b" test was still running, for " } else { b" tests were still running, for " });
            seconds(sentence, lasted);
            sentence.text(b" (");
            sentence.number(usize::try_from((lasted * 100 + wall / 2) / wall).unwrap_or(usize::MAX));
            sentence.text(b"% of the run).");
        }
        if self.peak != 0 {
            sentence.text(b" Peak was ");
            self.peak_clause(sentence);
        }
        if let (true, Some(slowest)) = (tail != 0, slowest) {
            sentence.text(b" Next slowest test ");
            seconds(sentence, slowest);
            sentence.text(b".");
        }
    }

    /// A run that never went above the tail limit has no tail to name, so it says how busy the jobs got and the slowest test.
    fn under_subscribed_sentence<const LEN: usize>(&self, sentence: &mut Line<LEN>) {
        let slowest = self.timed_records().map(|timed| duration(&timed)).max().unwrap_or(0);
        sentence.text(b"At most ");
        sentence.number(self.peak);
        sentence.text(b" of ");
        sentence.number(self.jobs);
        sentence.text(b" jobs were busy at once (at ");
        seconds(sentence, self.peak_ms);
        sentence.text(b"). Slowest test ");
        seconds(sentence, slowest);
        sentence.text(b".");
    }

    /// How many tests were still running after `busy_ms`, and the longest test that had already finished by then.
    fn timeline_tail(&self) -> (usize, Option<u64>) {
        let (mut tail, mut slowest) = (0, None::<u64>);
        for timed in self.timed_records() {
            if timed.1 > self.busy_ms {
                tail += 1;
            } else {
                slowest = slowest.max(Some(duration(&timed)));
            }
        }
        (tail, slowest)
    }

    fn timeline_rank<'a>(&'a self, longest: &mut [Option<Timed<'a>>; LONGEST]) {
        for timed in self.timed_records() {
            let slot = longest.iter().position(|held| held.is_none_or(|held| duration(&timed) > duration(&held)));
            if let Some(slot) = slot {
                longest[slot..].rotate_right(1);
                longest[slot] = Some(timed);
            }
        }
    }

    fn timeline_longest<const FRAME: usize>(&self, frame: &mut Line<FRAME>, width: usize, (chart, wall): (usize, u64)) {
        let mut longest = [None; LONGEST];
        self.timeline_rank(&mut longest);
        if longest[0].is_none() {
            return;
        }
        frame.text(b" longest tests, same time scale\n");
        for (index, timed) in longest.iter().flatten().enumerate() {
            let (start, stop, failed, id) = *timed;
            let first = column_of(start, chart, wall).min(chart - 1);
            let last = column_of(stop.min(wall).saturating_sub(1).max(start), chart, wall).clamp(first, chart - 1);
            frame.number(index + 1);
            spaces(frame, LEFT - 1 + first);
            let tone = if failed { Tone::Red } else { Tone::Green };
            for _ in first..=last {
                paint(frame, self.color, tone, BLOCK.as_bytes());
            }
            frame.text(b"\n");
            spaces(frame, 3);
            seconds(frame, duration(timed));
            frame.text(b"  ");
            let room = width.saturating_sub(3 + 9 + 2 + columns(id.0) + 2 + 1);
            frame.text(id.0);
            frame.text(b"  ");
            ellipsis_end(frame, id.1, room.max(8));
            frame.text(b"\n");
        }
    }
}

fn hold(held: &mut [u64; MAX_CHART], (start, stop): (u64, u64), (chart, wall): (usize, u64)) {
    let end = stop.min(wall);
    let first = column_of(start, chart, wall);
    let last = column_of(end.saturating_sub(1).max(start), chart, wall);
    for (column, held_ms) in held.iter_mut().enumerate().take(last.min(chart - 1) + 1).skip(first) {
        let (low, high) = slice_of(column, chart, wall);
        *held_ms += end.min(high).saturating_sub(start.max(low));
    }
}

fn column_of(time: u64, chart: usize, wall: u64) -> usize {
    usize::try_from(time.min(wall) * chart as u64 / wall).unwrap_or(0).min(chart - 1)
}

fn slice_of(column: usize, chart: usize, wall: u64) -> (u64, u64) {
    let chart = chart as u64;
    (column as u64 * wall / chart, (column as u64 + 1) * wall / chart)
}

fn tick_step(chart: usize, wall: u64) -> u64 {
    TICK_STEPS.into_iter().find(|step| step * 1000 * chart as u64 >= TICK_GAP * wall).unwrap_or(3600)
}

fn wrap<const FRAME: usize>(frame: &mut Line<FRAME>, text: &[u8], width: usize) {
    let mut rest = text;
    while !rest.is_empty() {
        let cut = if columns(rest) <= width {
            rest.len()
        } else {
            let limit = boundary(rest, width);
            rest[..limit].iter().rposition(|byte| *byte == b' ').unwrap_or(limit)
        };
        frame.text(b" ");
        frame.text(&rest[..cut]);
        frame.text(b"\n");
        rest = rest[cut..].trim_ascii_start();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{Event, Outcome};
    use crate::test_alloc::measure_allocations;

    const ORIGIN: u64 = 1_000;
    const SLOW: Identity<'static> = (b"cli_run", b"fixture::sleeps_sixteen_seconds");
    const FAILING: Identity<'static> = (b"cli_run", b"fixture::fails_after_ten_seconds");
    const SHORT: Identity<'static> = (b"summary", b"counts::adds_up");
    const QUICK: Identity<'static> = (b"summary", b"counts::rolls_over");
    const RUN: [(Identity<'static>, u64, u64, Outcome); 4] = [
        (SLOW, 0, 16_000, Outcome::Passed),
        (FAILING, 2_000, 12_000, Outcome::Failed),
        (SHORT, 0, 4_000, Outcome::Passed),
        (QUICK, 4_000, 8_000, Outcome::Passed),
    ];

    fn finished(failed: usize) -> Event<'static> {
        Event::Finished { passed: 3, failed, ignored: 0, not_run: 0, wall_ms: 20_000, manifest: None, discovered: 4, filtered: 0, exit: 1 }
    }

    fn run_events() -> impl Iterator<Item = Event<'static>> {
        let mut moments: [(u64, Event<'static>); 8] = core::array::from_fn(|index| {
            let (id, start, end, outcome) = RUN[index / 2];
            if index % 2 == 0 {
                (start, Event::Started { id, solo: false, at_ms: ORIGIN + start, limit_ms: None })
            } else {
                (end, Event::Settled { id, outcome, at_ms: Some(ORIGIN + end), solo: false })
            }
        });
        moments.sort_by_key(|(time, event)| (*time, matches!(event, Event::Started { .. })));
        core::iter::once(Event::Begin { version: b"0.1.0", shard: false, jobs: 3, github: false, why: None, json: false, at_ms: ORIGIN })
            .chain(moments.into_iter().map(|(_, event)| event))
            .chain(core::iter::once(finished(1)))
    }

    fn frame_of(color: bool, width: usize, arena: &mut [u8], ledger: &mut [u32], timed: &mut [u8]) -> Line<8192> {
        let mut inflight = [0_u8; 512];
        let mut state = State::<64>::new(arena, ledger).with_inflight(&mut inflight).with_timeline(timed).with_color(color);
        run_events().for_each(|event| state.apply(&event));
        let mut frame = Line::new();
        state.project_timeline(width, &mut frame).expect("the frame holds one timeline");
        frame
    }

    fn render(color: bool, width: usize) -> std::string::String {
        let (mut arena, mut ledger, mut timed) = ([0_u8; 512], [0_u32; 4], [0_u8; 512]);
        let frame = frame_of(color, width, &mut arena, &mut ledger, &mut timed);
        std::string::String::from_utf8(frame.bytes().to_vec()).expect("the timeline is utf-8")
    }

    fn golden(name: &str, expected: &str, color: bool, width: usize) {
        assert_eq!(render(color, width), expected, "{name}");
    }

    #[test]
    fn eighty_columns_with_colour_match_the_golden() {
        golden("80 colour", include_str!("timeline_golden/80_colour.txt"), true, 80);
    }

    #[test]
    fn eighty_columns_with_no_color_match_the_golden() {
        golden("80 plain", include_str!("timeline_golden/80_plain.txt"), false, 80);
    }

    #[test]
    fn sixty_columns_with_colour_match_the_golden() {
        golden("60 colour", include_str!("timeline_golden/60_colour.txt"), true, 60);
    }

    #[test]
    fn sixty_columns_with_no_color_match_the_golden() {
        golden("60 plain", include_str!("timeline_golden/60_plain.txt"), false, 60);
    }

    #[test]
    fn forty_columns_with_colour_match_the_golden() {
        golden("40 colour", include_str!("timeline_golden/40_colour.txt"), true, 40);
    }

    #[test]
    fn forty_columns_with_no_color_match_the_golden() {
        golden("40 plain", include_str!("timeline_golden/40_plain.txt"), false, 40);
    }

    fn live(previous: Option<u64>, elapsed_ms: u64) -> std::string::String {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed);
        let mut events = run_events().take(7);
        events.next().iter().for_each(|begin| state.apply(begin));
        if let Some(wall_ms) = previous {
            state.apply(&Event::Previous { wall_ms, tests: 383, build: (0, 0), matched: false });
        }
        events.for_each(|event| state.apply(&event));
        state.apply(&Event::Tick { now_ms: ORIGIN + elapsed_ms });
        let mut frame = Line::<8192>::new();
        state.project_timeline(80, &mut frame).expect("the frame holds one timeline");
        std::string::String::from_utf8(frame.bytes().to_vec()).expect("the timeline is utf-8")
    }

    fn axis_rows(text: &str) -> std::vec::Vec<&str> {
        let start = text.lines().position(|line| line.contains('\u{252c}')).expect("the tick row is drawn");
        text.lines().skip(start).take(2).collect()
    }

    #[test]
    fn a_live_frame_with_a_previous_run_matches_the_golden() {
        assert_eq!(live(Some(30_000), 9_000), include_str!("timeline_golden/80_live_previous.txt"));
    }

    #[test]
    fn a_live_frame_without_a_previous_run_matches_the_golden() {
        assert_eq!(live(None, 9_000), include_str!("timeline_golden/80_live_fresh.txt"));
    }

    #[test]
    fn a_no_color_frame_stipples_the_in_flight_bar_while_its_timeline_projection_stays_one_solid_layer() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed).with_color(false);
        let mut events = run_events().take(7);
        events.next().iter().for_each(|begin| state.apply(begin));
        state.apply(&Event::Resize { cols: 80 });
        state.apply(&Event::Built { done: 1, total: Some(1), exact: true, binaries: 1, at_ms: 0 });
        state.apply(&Event::Listed { binaries: 1, of: Some(1), tests: 4 });
        events.for_each(|event| state.apply(&event));
        state.apply(&Event::Tick { now_ms: ORIGIN + 9_000 });
        let (mut rich, mut timeline) = (Line::<8192>::new(), Line::<8192>::new());

        state.project_rich(false, &mut rich).expect("the frame holds one redraw");
        state.project_timeline(80, &mut timeline).expect("the frame holds one timeline");

        let (rich, timeline) = (std::string::String::from_utf8(rich.bytes().to_vec()).expect("utf-8"), std::string::String::from_utf8(timeline.bytes().to_vec()).expect("utf-8"));
        assert!(rich.contains('\u{2592}'), "an in-flight test stipples the run-frame bar under NO_COLOR:\n{rich}");
        assert!(!timeline.contains('\u{2592}'), "the timeline plot is one solid layer:\n{timeline}");
    }

    #[test]
    fn a_live_axis_ends_at_the_previous_wall_time_marked_approximate() {
        let text = live(Some(258_230), 9_000);

        assert!(axis_rows(&text)[1].trim_end().ends_with("~258.230s"), "{text}");
    }

    #[test]
    fn ten_live_samples_below_the_end_draw_byte_identical_axis_rows() {
        for previous in [Some(30_000), None] {
            let first = live(previous, 1_000);
            let expected = axis_rows(&first);
            for elapsed in (2..=10).map(|second| second * 1_000 - 1) {
                let text = live(previous, elapsed);
                assert_eq!(axis_rows(&text), expected, "previous={previous:?} elapsed={elapsed}");
            }
        }
    }

    #[test]
    fn a_live_axis_without_a_previous_run_starts_at_sixty_seconds_and_doubles_when_crossed() {
        let ends: std::vec::Vec<bool> = [(59_999, "~60.000s"), (60_000, "~60.000s"), (60_001, "~120.000s"), (130_000, "~240.000s")]
            .iter()
            .map(|(elapsed, end)| axis_rows(&live(None, *elapsed))[1].trim_end().ends_with(end))
            .collect();

        assert_eq!(ends, [true; 4]);
    }

    #[test]
    fn a_live_run_past_the_previous_wall_time_doubles_it_so_the_plot_never_overflows() {
        let text = live(Some(30_000), 31_000);

        assert!(axis_rows(&text)[1].trim_end().ends_with("~60.000s"), "{text}");
    }

    #[test]
    fn the_axis_end_is_the_closing_block_wall_time_at_every_width() {
        let mut closing = Line::<128>::new();
        seconds(&mut closing, 20_000);
        let wall = std::str::from_utf8(closing.bytes()).expect("seconds are ascii");

        for width in [80, 60, 40] {
            let text = render(false, width);
            let axis = text.lines().find(|line| line.starts_with(" seconds")).expect("the axis label row is drawn");
            assert!(axis.trim_end().ends_with(wall), "{width}: {axis:?} must end with {wall}");
        }
    }

    fn takeaway_of(text: &str) -> std::string::String {
        let lines: std::vec::Vec<&str> = text.lines().skip_while(|line| !line.starts_with(" \u{2588} ")).skip(1).take_while(|line| !line.starts_with(" longest")).map(str::trim).collect();
        lines.join(" ")
    }

    // fixture, jobs 3 so the tail limit is 1: concurrency is 2 on [0,2s), 3 on [2s,8s), 2 on [8s,12s), 1 on [12s,16s), 0 to the 20s wall.
    // the last instant above 1 is 12s; SLOW alone runs past it for 20-12=8s = 40%; the peak 3 first happens at 2s; FAILING (10s) is the longest finished by 12s.
    #[test]
    fn after_twelve_seconds_only_one_test_was_still_running_for_eight_seconds_forty_percent_of_the_run() {
        let text = render(false, 80);

        assert_eq!(
            takeaway_of(&text),
            "After 12.000s only 1 test was still running, for 8.000s (40% of the run). Peak was 3 at once at 2.000s. Next slowest test 10.000s."
        );
        assert!(!text.contains("held jobs"), "{text}");
    }

    #[test]
    fn a_live_run_says_what_is_running_now_and_the_peak_so_far() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed);
        run_events().take(4).for_each(|event| state.apply(&event));
        let mut frame = Line::<8192>::new();

        state.project_timeline(80, &mut frame).expect("the frame holds one timeline");

        let text = std::str::from_utf8(frame.bytes()).expect("the timeline is utf-8");
        assert_eq!(takeaway_of(text), "Running now: 3 tests. Peak so far: 3 at once at 2.000s. The summary appears when the run ends.");
    }

    #[test]
    fn a_run_whose_last_test_ends_above_the_tail_limit_has_no_long_tail() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 4, github: false, why: None, json: false, at_ms: ORIGIN });
        for id in [SLOW, FAILING, SHORT] {
            state.apply(&Event::Started { id, solo: false, at_ms: ORIGIN, limit_ms: None });
        }
        for id in [SLOW, FAILING, SHORT] {
            state.apply(&Event::Settled { id, outcome: Outcome::Passed, at_ms: Some(ORIGIN + 6_000), solo: false });
        }
        state.apply(&finished(0));
        let mut frame = Line::<8192>::new();

        state.project_timeline(80, &mut frame).expect("the frame holds one timeline");

        let text = std::str::from_utf8(frame.bytes()).expect("the timeline is utf-8");
        assert!(takeaway_of(text).starts_with("No long tail: more than 2 tests ran at once until the last one finished at 6.000s."), "{text}");
    }

    fn takeaway_after(jobs: usize, spans: &[(Identity<'static>, u64, u64)]) -> std::string::String {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs, github: false, why: None, json: false, at_ms: ORIGIN });
        let mut moments: std::vec::Vec<(u64, Event<'static>)> = std::vec::Vec::new();
        for (id, start, end) in spans {
            moments.push((*start, Event::Started { id: *id, solo: false, at_ms: ORIGIN + start, limit_ms: None }));
            moments.push((*end, Event::Settled { id: *id, outcome: Outcome::Passed, at_ms: Some(ORIGIN + end), solo: false }));
        }
        moments.sort_by_key(|(time, event)| (*time, matches!(event, Event::Started { .. })));
        for (_, event) in &moments {
            state.apply(event);
        }
        state.apply(&finished(0));
        let mut frame = Line::<8192>::new();
        state.project_timeline(80, &mut frame).expect("the frame holds one timeline");
        takeaway_of(std::str::from_utf8(frame.bytes()).expect("the timeline is utf-8"))
    }

    // jobs 10, tail limit 5: SHORT runs 0-3s and SLOW 1-5s, so concurrency is 1, then 2 from 1s, never near 5; the slowest test lasted 4s.
    #[test]
    fn a_run_that_never_exceeded_the_tail_limit_names_its_peak_and_slowest_test_instead_of_a_tail() {
        let sentence = takeaway_after(10, &[(SHORT, 0, 3_000), (SLOW, 1_000, 5_000)]);

        assert_eq!(sentence, "At most 2 of 10 jobs were busy at once (at 1.000s). Slowest test 4.000s.");
    }

    // jobs 4, tail limit 2: the peak of 2 equals the limit and never exceeds it, so no tail exists.
    #[test]
    fn a_peak_exactly_equal_to_the_tail_limit_is_still_not_a_tail() {
        let sentence = takeaway_after(4, &[(SHORT, 0, 3_000), (SLOW, 1_000, 5_000)]);

        assert_eq!(sentence, "At most 2 of 4 jobs were busy at once (at 1.000s). Slowest test 4.000s.");
    }

    const MIXED: [(Identity<'static>, Outcome); 4] = [
        ((b"mixed", b"passes_first"), Outcome::Passed),
        ((b"mixed", b"passes_second"), Outcome::Passed),
        ((b"mixed", b"fails_first"), Outcome::Failed),
        ((b"mixed", b"fails_second"), Outcome::Failed),
    ];

    // 32 jobs make one test two eighths tall, so the bottom row cell holds 2 eighths per running test
    fn bottom_cell(passing: &[usize], failing: &[usize]) -> std::string::String {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed).with_color(true);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 32, github: false, why: None, json: false, at_ms: ORIGIN });
        let running: std::vec::Vec<(Identity<'static>, Outcome)> = passing.iter().chain(failing).map(|index| MIXED[*index]).collect();
        for (id, _) in &running {
            state.apply(&Event::Started { id: *id, solo: false, at_ms: ORIGIN, limit_ms: None });
        }
        for (id, outcome) in &running {
            state.apply(&Event::Settled { id: *id, outcome: *outcome, at_ms: Some(ORIGIN + 10_000), solo: false });
        }
        state.apply(&Event::Finished { passed: passing.len(), failed: failing.len(), ignored: 0, not_run: 0, wall_ms: 10_000, manifest: None, discovered: 4, filtered: 0, exit: 1 });
        let mut frame = Line::<8192>::new();
        state.project_timeline(80, &mut frame).expect("the frame holds one timeline");
        let text = std::string::String::from_utf8(frame.bytes().to_vec()).expect("the timeline is utf-8");
        let (_, cells) = text.split_once("\u{1b}[2m0\u{1b}[0m ").expect("the bottom plot row is labelled 0");
        std::string::String::from(cells)
    }

    fn colour_and_glyph(cell: &str) -> (std::string::String, std::string::String) {
        let split = cell.find('m').expect("a coloured cell has an escape") + 1;
        (std::string::String::from(&cell[..split]), cell[split..].chars().take(1).collect())
    }

    #[test]
    fn a_cell_holding_more_passing_eighths_than_failing_is_drawn_passing() {
        let (colour, glyph) = colour_and_glyph(&bottom_cell(&[0, 1], &[2]));
        let (green, _) = colour_and_glyph(&bottom_cell(&[0], &[]));

        assert_eq!((colour, glyph.as_str()), (green, LOWER[6]));
    }

    #[test]
    fn a_cell_holding_more_failing_eighths_than_passing_is_drawn_failing() {
        let (colour, glyph) = colour_and_glyph(&bottom_cell(&[0], &[2, 3]));
        let (red, _) = colour_and_glyph(&bottom_cell(&[], &[2]));

        assert_eq!((colour, glyph.as_str()), (red, LOWER[6]));
    }

    #[test]
    fn a_cell_holding_equal_passing_and_failing_eighths_is_drawn_failing() {
        let (colour, glyph) = colour_and_glyph(&bottom_cell(&[0], &[2]));
        let (red, _) = colour_and_glyph(&bottom_cell(&[], &[2]));
        let (green, _) = colour_and_glyph(&bottom_cell(&[0], &[]));

        assert_eq!((colour.as_str(), glyph.as_str()), (red.as_str(), LOWER[4]));
        assert_ne!(red, green, "the two layers are told apart by colour");
    }

    #[test]
    fn without_colour_the_plot_is_one_solid_layer_and_never_stipples() {
        let text = render(false, 80);

        assert!(!text.contains('\u{2592}'), "{text}");
        assert!(text.contains(" \u{2588} tests running\n"), "{text}");
    }

    fn begun<'a>(arena: &'a mut [u8], ledger: &'a mut [u32], timed: &'a mut [u8], inflight: &'a mut [u8]) -> State<'a, 64> {
        let mut state = State::<64>::new(arena, ledger).with_inflight(inflight).with_timeline(timed);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 3, github: false, why: None, json: false, at_ms: ORIGIN });
        state
    }

    #[test]
    fn a_settle_without_a_time_is_counted_as_untimed() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = begun(&mut arena, &mut ledger, &mut timed, &mut inflight);
        state.apply(&Event::Started { id: SHORT, solo: false, at_ms: ORIGIN, limit_ms: None });

        state.apply(&Event::Settled { id: SHORT, outcome: Outcome::Passed, at_ms: None, solo: false });

        assert_eq!((state.untimed(), state.timed_records().count()), (1, 0));
    }

    #[test]
    fn a_settle_for_an_id_that_never_started_is_counted_as_untimed() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = begun(&mut arena, &mut ledger, &mut timed, &mut inflight);

        state.apply(&Event::Settled { id: QUICK, outcome: Outcome::Failed, at_ms: Some(ORIGIN + 500), solo: false });

        assert_eq!((state.untimed(), state.timed_records().count()), (1, 0));
    }

    #[test]
    fn an_ignored_test_never_held_a_job_so_it_is_not_untimed() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = begun(&mut arena, &mut ledger, &mut timed, &mut inflight);

        state.apply(&Event::Settled { id: SHORT, outcome: Outcome::Ignored, at_ms: None, solo: false });
        state.apply(&Event::Settled { id: QUICK, outcome: Outcome::Ignored, at_ms: Some(ORIGIN + 10), solo: false });

        assert_eq!((state.untimed(), state.timed_records().count()), (0, 0));
    }

    #[test]
    fn ten_thousand_settled_events_append_their_times_with_zero_allocations() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], std::vec![0_u8; 10_000 * 40], [0_u8; 512]);
        let mut state = begun(&mut arena, &mut ledger, &mut timed, &mut inflight);

        let ((), allocations) = measure_allocations(|| {
            for round in 0..10_000_u64 {
                state.apply(&Event::Started { id: SHORT, solo: false, at_ms: ORIGIN + round * 2, limit_ms: None });
                state.apply(&Event::Settled { id: SHORT, outcome: Outcome::Passed, at_ms: Some(ORIGIN + round * 2 + 1), solo: false });
            }
        });

        assert_eq!((state.timed_records().count(), state.untimed()), (10_000, 0), "every settle appended, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
    }

    #[test]
    fn the_three_longest_tests_are_listed_with_their_seconds_longest_first() {
        let text = render(false, 80);
        let listed: std::vec::Vec<&str> = text.lines().filter(|line| line.starts_with("   ") && line.contains(".000s")).collect();

        assert_eq!(listed.len(), 3, "{text}");
        assert!(listed[0].contains("16.000s") && listed[0].contains("sleeps_sixteen_seconds"), "{listed:?}");
        assert!(listed[1].contains("10.000s") && listed[1].contains("fails_after_ten_seconds"), "{listed:?}");
        assert!(listed[2].contains("4.000s"), "{listed:?}");
    }

    #[test]
    fn the_plot_is_eight_rows_tall_whatever_the_job_count() {
        for jobs in [8, 16, 32] {
            let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
            let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed);
            state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs, github: false, why: None, json: false, at_ms: ORIGIN });
            state.apply(&finished(0));
            let mut frame = Line::<8192>::new();

            state.project_timeline(80, &mut frame).expect("the frame holds one timeline");

            let text = std::str::from_utf8(frame.bytes()).expect("the timeline is utf-8");
            let before_axis = text.lines().take_while(|line| !line.contains('\u{252c}')).count();
            assert_eq!(before_axis - 3, PLOT_ROWS, "jobs={jobs}\n{text}");
        }
    }

    #[test]
    fn below_the_minimum_width_one_line_says_so() {
        assert_eq!(render(false, 30), "timeline needs 40 columns, have 30\n");
    }

    #[test]
    fn tests_past_the_arena_are_counted_not_tracked() {
        let (mut arena, mut ledger, mut timed) = ([0_u8; 512], [0_u32; 4], [0_u8; 60]);
        let frame = frame_of(false, 80, &mut arena, &mut ledger, &mut timed);

        let text = std::str::from_utf8(frame.bytes()).expect("the timeline is utf-8");

        assert!(text.contains(" +3 tests not tracked\n"), "{text}");
    }

    #[test]
    fn ten_thousand_timeline_frames_project_with_zero_allocations() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed).with_color(true);
        run_events().for_each(|event| state.apply(&event));
        let mut frame = Line::<8192>::new();
        let mut projected = 0_usize;

        let ((), allocations) = measure_allocations(|| {
            for round in 0..10_000_usize {
                frame.clear();
                state.project_timeline([80, 60, 40][round % 3], &mut frame).expect("the frame holds one timeline");
                projected += frame.bytes().len();
            }
        });

        assert!(projected > 10_000 * 400, "every frame drew a chart, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
    }

    #[test]
    fn ten_thousand_live_timeline_frames_project_with_zero_allocations() {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed).with_color(true);
        run_events().take(4).for_each(|event| state.apply(&event));
        state.apply(&Event::Previous { wall_ms: 30_000, tests: 383, build: (0, 0), matched: false });
        let mut frame = Line::<8192>::new();
        let (mut drawn_live_axes, mut drawn_running) = (0_usize, 0_usize);

        let ((), allocations) = measure_allocations(|| {
            for round in 0..10_000_usize {
                state.apply(&Event::Tick { now_ms: ORIGIN + round as u64 * 2 });
                frame.clear();
                state.project_timeline([80, 60, 40][round % 3], &mut frame).expect("the frame holds one timeline");
                drawn_live_axes += usize::from(frame.bytes().windows(8).any(|window| window == b"~30.000s"));
                drawn_running += usize::from(frame.bytes().windows(2).any(|window| window == [0xe2, 0x96]));
            }
        });

        assert_eq!(state.inflight_len(), 3, "three tests stayed in flight for every frame");
        assert_eq!(drawn_live_axes, 10_000, "every frame drew the fixed live axis, so a zero is not an empty loop");
        assert!(drawn_running >= 9_999, "frames drew in-flight columns, so a zero is not an empty plot: {drawn_running}");
        assert_eq!(allocations, 0);
    }

    fn levels_with(running: bool) -> ([u8; MAX_CHART], [u8; MAX_CHART]) {
        let (mut arena, mut ledger, mut timed, mut inflight) = ([0_u8; 512], [0_u32; 4], [0_u8; 512], [0_u8; 512]);
        let mut state = State::<64>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_timeline(&mut timed);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: false, why: None, json: false, at_ms: ORIGIN });
        state.apply(&Event::Previous { wall_ms: 66_000, tests: 383, build: (0, 0), matched: false });
        state.apply(&Event::Started { id: SHORT, solo: false, at_ms: ORIGIN + 2_000, limit_ms: None });
        state.apply(&Event::Settled { id: SHORT, outcome: Outcome::Passed, at_ms: Some(ORIGIN + 9_000), solo: false });
        if running {
            state.apply(&Event::Started { id: SLOW, solo: false, at_ms: ORIGIN + 5_000, limit_ms: None });
        }
        state.apply(&Event::Tick { now_ms: ORIGIN + 20_000 });
        let (mut pass, mut stacked) = ([0_u8; MAX_CHART], [0_u8; MAX_CHART]);
        state.timeline_levels(66, 66_000, (&mut pass, &mut stacked));
        (pass, stacked)
    }

    #[test]
    fn an_in_flight_test_raises_the_columns_between_its_start_and_now_by_one_tests_worth_and_nothing_else() {
        let (base_pass, base_stacked) = levels_with(false);
        let (with_pass, with_stacked) = levels_with(true);

        for column in 0..66 {
            let inside = (5..20).contains(&column);
            let one_test = if inside { PLOT_ROWS_EIGHTHS / 8 } else { 0 };
            assert_eq!(with_pass[column] - base_pass[column], one_test, "pass column {column}");
            assert_eq!(with_stacked[column] - base_stacked[column], one_test, "stacked column {column}");
        }
    }

    #[test]
    fn a_live_frame_lists_only_settled_tests_among_the_longest_while_two_are_in_flight() {
        let text = live(Some(30_000), 9_000);

        assert!(text.contains("Running now: 2 tests."), "{text}");
        assert!(text.contains("counts::adds_up") && text.contains("counts::rolls_over"), "{text}");
        assert!(!text.contains("sleeps_sixteen_seconds") && !text.contains("fails_after_ten_seconds"), "{text}");
    }
}
