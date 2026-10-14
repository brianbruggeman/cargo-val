//! The render core: one state, a sans-IO transition, and a projection into a caller-provided buffer.
//!
//! Composes [`Line`] (the fixed buffer every cargo-val line is built in) and nothing else: no syscalls, no clock, no
//! terminal query, no allocation, no lock. Time, the github flag and the process mode arrive as event data. The driver
//! owns one `State` on its event loop and feeds it from the same places that used to call the plain writers: the events
//! are the `In` of the transition `(state, event) -> state` and of the projection `(state, event) -> bytes`; the bytes
//! are the `In` of the existing sink (`write_fd`, the status-queue path). Names, ids and captured output are borrowed
//! from the buffers the driver already owns and are never copied into the state.
//!
//! Plain is the first projection. Rich and agent read the same `State`; they add modes and events, not a second ledger.

use crate::line::Line;

/// A test id as two borrowed fields: the binary, then the test name.
pub type Identity<'a> = (&'a [u8], &'a [u8]);

const LABEL_WIDTH: usize = 9;
const FILTER_OVERHEAD: usize = b"(binary(=) & test(=))".len();
const FILTER_JOIN: &[u8] = b" | ";
const STDOUT_STREAM: &[u8] = b"stdout";
const STDERR_STREAM: &[u8] = b"stderr";

/// How one test ended, as the settle event reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Ignored,
}

/// The screen the run is in. Plain uses exactly these four; review, timeline and the rest join with the rich projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode<'a> {
    Running,
    BuildError { diagnostic: &'a [u8] },
    Interrupted { signal: u8 },
    Done { wall_ms: u64 },
}

/// Everything the driver reports to the renderer. `B` payloads borrow from buffers the driver owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    Begin { version: &'a [u8], shard: bool, jobs: usize, github: bool },
    Listed { binaries: usize, tests: usize },
    Started { id: Identity<'a>, solo: bool },
    Settled { id: Identity<'a>, outcome: Outcome, millis: Option<u64>, solo: bool },
    Captured { id: Identity<'a>, stdout: (&'a [u8], bool), stderr: (&'a [u8], bool) },
    Warned { id: Identity<'a>, what: &'a [u8] },
    BuildFailed { diagnostic: &'a [u8] },
    Interrupted { signal: u8 },
    Finished { passed: usize, failed: usize, ignored: usize, not_run: usize, wall_ms: u64, manifest: Option<&'a [u8]> },
}

impl<'a> Mode<'a> {
    /// The transition table: every (mode, event) pair is named, so a new mode or event is a compile error here.
    #[must_use]
    pub const fn next(self, event: &Event<'a>) -> Self {
        match (self, event) {
            (Self::Running, Event::BuildFailed { diagnostic }) => Self::BuildError { diagnostic },
            (Self::Running, Event::Interrupted { signal }) => Self::Interrupted { signal: *signal },
            (Self::Running, Event::Finished { failed: 0, wall_ms, .. }) => Self::Done { wall_ms: *wall_ms },
            (
                Self::Running,
                Event::Begin { .. }
                | Event::Listed { .. }
                | Event::Started { .. }
                | Event::Settled { .. }
                | Event::Captured { .. }
                | Event::Warned { .. }
                | Event::Finished { .. },
            ) => Self::Running,
            (Self::BuildError { .. } | Self::Interrupted { .. } | Self::Done { .. }, _) => self,
        }
    }
}

/// The render state: the screen, the projection inputs, and the failure ledger as borrowed ids.
#[derive(Debug)]
pub struct State<'a, const FAILURES: usize, const RERUN: usize> {
    mode: Mode<'a>,
    github: bool,
    failures: [Identity<'a>; FAILURES],
    stored: usize,
    unstored: usize,
}

impl<'a, const FAILURES: usize, const RERUN: usize> State<'a, FAILURES, RERUN> {
    #[must_use]
    pub const fn new() -> Self {
        Self { mode: Mode::Running, github: false, failures: [(&[], &[]); FAILURES], stored: 0, unstored: 0 }
    }

    #[must_use]
    pub const fn mode(&self) -> Mode<'a> {
        self.mode
    }

    #[must_use]
    pub fn failures(&self) -> &[Identity<'a>] {
        &self.failures[..self.stored]
    }

    /// Failures that arrived after the ledger was full; the closing block says how many.
    #[must_use]
    pub const fn unstored(&self) -> usize {
        self.unstored
    }

    pub fn apply(&mut self, event: &Event<'a>) {
        self.mode = self.mode.next(event);
        match *event {
            Event::Begin { github, .. } => *self = Self { github, ..Self::new() },
            Event::Settled { id, outcome: Outcome::Failed, .. } => self.record(id),
            _ => {}
        }
    }

    fn record(&mut self, id: Identity<'a>) {
        if self.stored < FAILURES {
            self.failures[self.stored] = id;
            self.stored += 1;
        } else {
            self.unstored += 1;
        }
    }

    /// Appends the lines `event` warrants, given the state after `apply`. Err carries the bytes the frame could not hold.
    ///
    /// # Errors
    ///
    /// Returns the number of bytes cut when the frame was too small; the frame then holds a truncated projection.
    pub fn project<const FRAME: usize>(&self, event: &Event<'_>, frame: &mut Line<FRAME>) -> Result<(), usize> {
        match *event {
            Event::Begin { version, shard, jobs, .. } => header(frame, version, shard, jobs),
            Event::Listed { binaries, tests } => listed(frame, binaries, tests),
            Event::Started { id, solo } => event_line(frame, b"START", id, None, solo),
            Event::Settled { id, outcome, millis, solo } => self.settled(frame, id, outcome, millis, solo),
            Event::Captured { id, stdout, stderr } => self.captured(frame, id, stdout, stderr),
            Event::Warned { id, what } => warned(frame, id, what),
            Event::BuildFailed { diagnostic } => text_line(frame, b"BUILD", b"failed  ", diagnostic),
            Event::Interrupted { signal } => interrupted(frame, signal),
            Event::Finished { passed, failed, ignored, not_run, wall_ms, manifest } => {
                self.closing(frame, [passed, failed, ignored, not_run], wall_ms, manifest);
            }
        }
        match frame.lost() {
            0 => Ok(()),
            lost => Err(lost),
        }
    }

    fn settled<const FRAME: usize>(
        &self,
        frame: &mut Line<FRAME>,
        id: Identity<'_>,
        outcome: Outcome,
        millis: Option<u64>,
        solo: bool,
    ) {
        let label: &[u8] = match outcome {
            Outcome::Passed => b"PASS",
            Outcome::Failed => b"FAIL",
            Outcome::Ignored => b"IGNORED",
        };
        event_line(frame, label, id, millis, solo);
        if outcome == Outcome::Failed && self.github {
            frame.text(b"::error title=cargo val::");
            frame.text(id.0);
            frame.text(b"::");
            frame.text(id.1);
            frame.text(b" failed\n");
        }
    }

    fn captured<const FRAME: usize>(
        &self,
        frame: &mut Line<FRAME>,
        id: Identity<'_>,
        stdout: (&[u8], bool),
        stderr: (&[u8], bool),
    ) {
        if self.github {
            frame.text(b"::group::");
            frame.text(id.0);
            frame.text(b" ");
            frame.text(id.1);
            frame.text(b"\n");
        }
        stream(frame, id, STDOUT_STREAM, stdout);
        stream(frame, id, STDERR_STREAM, stderr);
        if self.github {
            frame.text(b"::endgroup::\n");
        }
    }

    fn closing<const FRAME: usize>(
        &self,
        frame: &mut Line<FRAME>,
        [passed, failed, ignored, not_run]: [usize; 4],
        wall_ms: u64,
        manifest: Option<&[u8]>,
    ) {
        label(frame, b"SUMMARY");
        for (key, value) in [(&b"passed="[..], passed), (b" failed=", failed), (b" ignored=", ignored), (b" not_run=", not_run)] {
            frame.text(key);
            frame.number(value);
        }
        frame.text(b"  wall=");
        seconds(frame, wall_ms);
        frame.text(b"\n");
        for (target, name) in self.failures() {
            label(frame, b"FAILED");
            frame.text(target);
            frame.text(b"  ");
            frame.text(name);
            frame.text(b"\n");
        }
        if self.unstored != 0 {
            label(frame, b"FAILED");
            frame.text(b"+");
            frame.number(self.unstored);
            frame.text(b" failures not listed\n");
        }
        self.rerun(frame, manifest);
    }

    fn rerun<const FRAME: usize>(&self, frame: &mut Line<FRAME>, manifest: Option<&[u8]>) {
        let included = self.clauses_within(RERUN);
        if included == 0 {
            return;
        }
        label(frame, b"RERUN");
        frame.text(b"cargo val");
        if let Some(path) = manifest {
            frame.text(b" --manifest-path '");
            frame.text(path);
            frame.text(b"'");
        }
        frame.text(b" --no-fail-fast -E '");
        for (position, (target, name)) in self.failures()[..included].iter().enumerate() {
            if position != 0 {
                frame.text(FILTER_JOIN);
            }
            frame.text(b"(binary(=");
            frame.text(target);
            frame.text(b") & test(=");
            frame.text(name);
            frame.text(b"))");
        }
        frame.text(b"'");
        let omitted = self.stored - included + self.unstored;
        if omitted != 0 {
            frame.text(b"  (");
            frame.number(omitted);
            frame.text(b" more failures are not in this command)");
        }
        frame.text(b"\n");
    }

    fn clauses_within(&self, budget: usize) -> usize {
        let mut used = 0;
        let mut included = 0;
        for (target, name) in self.failures() {
            let join = if included == 0 { 0 } else { FILTER_JOIN.len() };
            let clause = FILTER_OVERHEAD + target.len() + name.len();
            if used + join + clause > budget {
                break;
            }
            used += join + clause;
            included += 1;
        }
        included
    }
}

impl<const FAILURES: usize, const RERUN: usize> Default for State<'_, FAILURES, RERUN> {
    fn default() -> Self {
        Self::new()
    }
}

fn label<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8]) {
    frame.text(name);
    for _ in name.len()..LABEL_WIDTH.max(name.len() + 1) {
        frame.text(b" ");
    }
}

fn seconds<const FRAME: usize>(frame: &mut Line<FRAME>, millis: u64) {
    frame.number(usize::try_from(millis / 1000).unwrap_or(usize::MAX));
    frame.text(b".");
    let fraction = usize::try_from(millis % 1000).unwrap_or(0);
    if fraction < 100 {
        frame.text(b"0");
    }
    if fraction < 10 {
        frame.text(b"0");
    }
    frame.number(fraction);
    frame.text(b"s");
}

fn event_line<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], id: Identity<'_>, millis: Option<u64>, solo: bool) {
    label(frame, name);
    frame.text(id.0);
    frame.text(b"  ");
    frame.text(id.1);
    if let Some(millis) = millis {
        frame.text(b"  ");
        seconds(frame, millis);
    }
    if solo {
        frame.text(b"  (solo rerun)");
    }
    frame.text(b"\n");
}

fn header<const FRAME: usize>(frame: &mut Line<FRAME>, version: &[u8], shard: bool, jobs: usize) {
    frame.text(b"cargo-val ");
    frame.text(version);
    frame.text(b"  process=");
    frame.text(if shard { b"shard" } else { b"test" });
    frame.text(b"  jobs=");
    frame.number(jobs);
    frame.text(b"\n");
}

fn listed<const FRAME: usize>(frame: &mut Line<FRAME>, binaries: usize, tests: usize) {
    label(frame, b"LISTED");
    frame.text(b"binaries=");
    frame.number(binaries);
    frame.text(b"  tests=");
    frame.number(tests);
    frame.text(b"\n");
}

fn warned<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, what: &[u8]) {
    label(frame, b"WARN");
    frame.text(id.0);
    frame.text(b"  ");
    frame.text(id.1);
    frame.text(b"  ");
    frame.text(what);
    frame.text(b"\n");
}

fn text_line<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], prefix: &[u8], text: &[u8]) {
    label(frame, name);
    frame.text(prefix);
    frame.text(text);
    frame.text(b"\n");
}

fn interrupted<const FRAME: usize>(frame: &mut Line<FRAME>, signal: u8) {
    label(frame, b"INTERRUPTED");
    frame.text(b"signal=");
    frame.number(usize::from(signal));
    frame.text(b"\n");
}

fn stream<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, name: &[u8], (bytes, truncated): (&[u8], bool)) {
    if bytes.is_empty() {
        return;
    }
    frame.text(b"------ ");
    frame.text(name);
    frame.text(b": ");
    frame.text(id.0);
    frame.text(b" ");
    frame.text(id.1);
    frame.text(b" ------\n");
    frame.text(bytes);
    if bytes.last() != Some(&b'\n') {
        frame.text(b"\n");
    }
    if truncated {
        frame.text(b"[output truncated]\n");
    }
    frame.text(b"------ end ");
    frame.text(name);
    frame.text(b" ------\n");
}

#[cfg(test)]
mod tests {
    use std::string::String;

    use super::{Event, Identity, Mode, Outcome, State};
    use crate::line::Line;
    use crate::test_alloc::measure_allocations;

    const FAILING: Identity<'static> = (b"cli_run", b"fixture::fails_when_requested");
    const PASSING: Identity<'static> = (b"cli_run", b"fixture::passes");

    type Small = State<'static, 2, 200>;

    fn projected<const FAILURES: usize, const RERUN: usize>(state: &State<'static, FAILURES, RERUN>, event: &Event<'static>) -> String {
        let mut frame = Line::<4096>::new();
        state.project(event, &mut frame).expect("the frame holds one event");
        String::from_utf8(frame.bytes().to_vec()).expect("plain output is utf-8")
    }

    fn settle(state: &mut Small, id: Identity<'static>, outcome: Outcome) -> String {
        let event = Event::Settled { id, outcome, millis: Some(3123), solo: false };
        state.apply(&event);
        projected(state, &event)
    }

    #[test]
    fn a_settled_failure_projects_its_fail_line_and_joins_the_ledger() {
        let mut state = Small::new();

        let line = settle(&mut state, FAILING, Outcome::Failed);

        assert_eq!(line, "FAIL     cli_run  fixture::fails_when_requested  3.123s\n");
        assert_eq!(state.failures(), &[FAILING]);
    }

    #[test]
    fn github_adds_the_error_annotation_after_the_fail_line_and_wraps_captured_output_in_a_group() {
        let mut state = Small::new();
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: true });

        let fail = settle(&mut state, FAILING, Outcome::Failed);
        let captured = Event::Captured { id: FAILING, stdout: (b"requested fixture failure", false), stderr: (b"", false) };
        let block = projected(&state, &captured);

        assert!(fail.ends_with("::error title=cargo val::cli_run::fixture::fails_when_requested failed\n"), "{fail}");
        assert_eq!(
            block,
            "::group::cli_run fixture::fails_when_requested\n------ stdout: cli_run fixture::fails_when_requested ------\nrequested fixture failure\n------ end stdout ------\n::endgroup::\n"
        );
    }

    #[test]
    fn the_closing_block_lists_every_failure_and_one_command_that_reruns_them() {
        let mut state = Small::new();
        settle(&mut state, PASSING, Outcome::Passed);
        settle(&mut state, FAILING, Outcome::Failed);
        let finished = Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: 8400, manifest: Some(b"/w/Cargo.toml") };
        state.apply(&finished);

        let block = projected(&state, &finished);

        assert_eq!(
            block,
            "SUMMARY  passed=1 failed=1 ignored=0 not_run=0  wall=8.400s\n\
             FAILED   cli_run  fixture::fails_when_requested\n\
             RERUN    cargo val --manifest-path '/w/Cargo.toml' --no-fail-fast -E '(binary(=cli_run) & test(=fixture::fails_when_requested))'\n"
        );
        assert_eq!(state.mode(), Mode::Running, "failures keep the run on screen");
    }

    #[test]
    fn failures_past_the_ledger_are_counted_and_named_in_the_closing_block() {
        let mut state = Small::new();
        let names: [Identity<'static>; 4] = [(b"a", b"one"), (b"a", b"two"), (b"a", b"three"), (b"a", b"four")];
        for id in names {
            settle(&mut state, id, Outcome::Failed);
        }
        let finished = Event::Finished { passed: 0, failed: 4, ignored: 0, not_run: 0, wall_ms: 10, manifest: None };

        let block = projected(&state, &finished);

        assert_eq!(state.unstored(), 2);
        assert!(block.contains("FAILED   +2 failures not listed\n"), "{block}");
        assert!(block.contains("  (2 more failures are not in this command)\n"), "{block}");
    }

    #[test]
    fn a_rerun_command_that_outgrows_its_budget_says_how_many_failures_it_left_out() {
        let mut state = State::<'static, 4, 40>::new();
        for id in [(&b"a"[..], &b"one"[..]), (b"a", b"two"), (b"a", b"three")] {
            state.apply(&Event::Settled { id, outcome: Outcome::Failed, millis: None, solo: false });
        }
        let finished = Event::Finished { passed: 0, failed: 3, ignored: 0, not_run: 0, wall_ms: 10, manifest: None };

        let block = projected(&state, &finished);

        let rerun = block.lines().find(|line| line.starts_with("RERUN")).expect("a rerun line");
        assert_eq!(rerun, "RERUN    cargo val --no-fail-fast -E '(binary(=a) & test(=one))'  (2 more failures are not in this command)");
    }

    #[test]
    fn a_frame_too_small_for_the_event_reports_the_bytes_it_cut() {
        let state = Small::new();
        let mut frame = Line::<16>::new();

        let result = state.project(&Event::Started { id: FAILING, solo: false }, &mut frame);

        assert_eq!(result, Err(b"START    cli_run  fixture::fails_when_requested\n".len() - 16));
    }

    #[test]
    fn every_mode_event_pair_lands_where_the_transition_table_says() {
        let finished = |failed| Event::Finished { passed: 1, failed, ignored: 0, not_run: 0, wall_ms: 5, manifest: None };
        let rows = [
            (Mode::Running, Event::BuildFailed { diagnostic: b"E0308" }, Mode::BuildError { diagnostic: b"E0308" }),
            (Mode::Running, Event::Interrupted { signal: 2 }, Mode::Interrupted { signal: 2 }),
            (Mode::Running, finished(0), Mode::Done { wall_ms: 5 }),
            (Mode::Running, finished(1), Mode::Running),
            (Mode::Running, Event::Started { id: PASSING, solo: false }, Mode::Running),
            (Mode::Done { wall_ms: 5 }, Event::Interrupted { signal: 2 }, Mode::Done { wall_ms: 5 }),
            (Mode::BuildError { diagnostic: b"E0308" }, finished(0), Mode::BuildError { diagnostic: b"E0308" }),
            (Mode::Interrupted { signal: 15 }, Event::Started { id: PASSING, solo: false }, Mode::Interrupted { signal: 15 }),
        ];

        for (from, event, to) in rows {
            assert_eq!(from.next(&event), to, "{from:?} on {event:?}");
        }
    }

    #[test]
    fn no_projection_contains_an_escape_byte() {
        let mut state = Small::new();
        let events = [
            Event::Begin { version: b"0.1.0", shard: true, jobs: 8, github: true },
            Event::Listed { binaries: 19, tests: 383 },
            Event::Started { id: FAILING, solo: true },
            Event::Warned { id: FAILING, what: b"rerun failed to start" },
            Event::Interrupted { signal: 2 },
        ];

        for event in events {
            state.apply(&event);
            assert!(!projected(&state, &event).as_bytes().contains(&0x1b));
        }
    }

    #[test]
    fn a_hundred_thousand_events_update_and_project_with_zero_allocations() {
        let mut state = State::<'static, 64, 8192>::new();
        let mut frame = Line::<32_768>::new();
        let mut events_applied = 0_usize;

        let ((), allocations) = measure_allocations(|| {
            for round in 0..25_000_u64 {
                let outcome = if round % 7 == 0 { Outcome::Failed } else { Outcome::Passed };
                for event in [
                    Event::Started { id: PASSING, solo: false },
                    Event::Settled { id: FAILING, outcome, millis: Some(round), solo: false },
                    Event::Captured { id: FAILING, stdout: (b"assertion failed: left == right", false), stderr: (b"", false) },
                    Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: round, manifest: Some(b"/w/Cargo.toml") },
                ] {
                    frame.clear();
                    state.apply(&event);
                    state.project(&event, &mut frame).expect("the frame holds one event");
                    events_applied += 1;
                }
            }
        });

        assert_eq!(events_applied, 100_000, "the loop ran every event, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
        assert!(state.unstored() > 0, "the ledger overflowed during the loop, so the overflow path was measured too");
    }
}
