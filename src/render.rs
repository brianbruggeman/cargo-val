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

mod agent;
mod entry;
mod excerpt;
mod json;
pub mod review;

use memchr::memchr;

use crate::line::Line;
use crate::failure_class::{Absent, Class, Enforcer, Exit, Location, Record, Reject, classify};
use crate::hyperlink::write_linked;
use crate::view_defaults::{HEARTBEAT_SECS, REASON_BYTES};
use review::{Key, Rerun, Verdict};

mod rich;
mod timeline;

/// A test id as two borrowed fields: the binary, then the test name.
pub type Identity<'a> = (&'a [u8], &'a [u8]);

/// The driver's rendered source excerpt for a failure: the rows, or the reason there are none. An empty reason means the
/// driver did not look, and no block is printed.
pub type ExcerptRows<'a> = Result<&'a [u8], &'a [u8]>;

/// Marks an event whose producer did not read a source file.
pub const UNREAD: ExcerptRows<'static> = Err(b"");

/// Classifies a failure's two captures the way every projection and the driver must agree on: stdout when it has bytes, else stderr, then the other stream.
///
/// # Errors
///
/// `Reject` exactly as `classify` returns it.
pub fn located<'a>(stdout: Stream<'a>, stderr: Stream<'a>, exit: Exit) -> Result<Record<'a>, Reject> {
    let (capture, dropped) = if stdout.0.is_empty() { stderr } else { stdout };
    classify(capture, exit, dropped).or_else(|_| classify(stderr.0, exit, stderr.1))
}

/// One captured stream: its bytes, and how many bytes the driver's buffer discarded after it filled.
pub type Stream<'a> = (&'a [u8], u64);

const LABEL_WIDTH: usize = 9;
const HEARTBEAT_MS: u64 = HEARTBEAT_SECS as u64 * 1000;
const INFLIGHT_HEAD: usize = 13;
const FAILURE_HEAD: usize = 13;
const LOCK_BYTES: usize = 32;
const NOTICE_BYTES: usize = 120;
const TIMED_HEAD: usize = 9;
const FILTER_OVERHEAD: usize = b"(binary(=) & test(=))".len();
const FILTER_JOIN: &[u8] = b" | ";
pub(crate) const STDOUT_STREAM: &[u8] = b"stdout";
pub(crate) const STDERR_STREAM: &[u8] = b"stderr";
const SOURCE_STREAM: &[u8] = b"source";

// the stream that holds the panic site is the one the kept record is read from, so `detail` and `e` find the location the excerpt was cut from;
// with no site in either stream the stdout-first rule of `located` stands
fn sited<'a>((stdout, stderr): (Stream<'a>, Stream<'a>), exit: Exit) -> Result<Record<'a>, Reject> {
    [stdout, stderr]
        .into_iter()
        .find_map(|(capture, dropped)| classify(capture, exit, dropped).ok().filter(|record| record.location.is_ok()))
        .map_or_else(|| located(stdout, stderr, exit), Ok)
}

const EXIT_HEAD: usize = 5;
const VIEWS_AT: usize = 8;
const VIEW_BYTES: usize = 28;
const BLOCK_AT: usize = VIEWS_AT + 2 * VIEW_BYTES;
const RECORD_AT: usize = BLOCK_AT + 8;
const PAYLOAD_HEAD: usize = RECORD_AT + 23;
const CLASS_NONE: u8 = 255;
const WHICH_STDERR: u8 = 1;
const REASON_FLAG: u8 = 2;
const BLOCK_PENDING: u8 = 4;
const FILE_KEEP: usize = 512;
const MESSAGE_KEEP: usize = 2048;
const SIDE_KEEP: usize = 1024;
const QUIT_WINDOW_MS: u64 = 2_000;
const PASS_SHOWN_MS: u64 = 1_500;

/// How one test ended, as the settle event reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    Ignored,
}

/// The screen the run is in. Plain uses exactly these four; review, timeline and the rest join with the rich projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Running,
    QuitArmed,
    /// `top` is `None` while the list has focus and the first visible output line while the detail is being scrolled.
    Review { selected: u16, armed: bool, top: Option<u16> },
    /// `back` is where a list screen returns to: `None` is the running screen, `Some(selected)` review with its selection.
    Timeline { back: Option<u16> },
    Receipt,
    BuildError,
    Interrupted { signal: u8 },
    Done { wall_ms: u64 },
}

/// Everything the driver reports to the renderer. `B` payloads borrow from buffers the driver owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    Begin { version: &'a [u8], shard: bool, jobs: usize, github: bool, why: Option<&'a [u8]>, json: bool, at_ms: u64 },
    Waiting { lock: &'a [u8] },
    Previous { wall_ms: u64, tests: u32 },
    Listed { binaries: usize, of: Option<usize>, tests: usize },
    Built { done: u16, total: Option<u16> },
    Resize { cols: u16 },
    Started { id: Identity<'a>, solo: bool, at_ms: u64, limit_ms: Option<u32> },
    Settled { id: Identity<'a>, outcome: Outcome, at_ms: Option<u64>, solo: bool },
    Tick { now_ms: u64 },
    Captured { id: Identity<'a>, stdout: Stream<'a>, stderr: Stream<'a>, exit: Exit, excerpt: ExcerptRows<'a> },
    Warned { id: Identity<'a>, what: &'a [u8] },
    BlockOpened { id: Identity<'a> },
    BlockClosed,
    BuildFailed { diagnostic: &'a [u8], exit: u8 },
    Interrupted { signal: u8, groups: usize },
    Running { id: Identity<'a>, group: u32, age_ms: u64 },
    Exited { code: u8 },
    Key(Key),
    /// A one-line message the review footer shows until the next key; the driver emits it when an action it ran needs to tell the user.
    Notice { what: &'a [u8] },
    /// What retrying a failure alone concluded, or why it was not tried.
    Retried { id: Identity<'a>, verdict: Verdict },
    /// How a failure's rerun stands: running, passed, or idle again after it failed.
    Reran { id: Identity<'a>, rerun: Rerun },
    Finished { passed: usize, failed: usize, ignored: usize, not_run: usize, wall_ms: u64, manifest: Option<&'a [u8]>, discovered: usize, filtered: usize, exit: u8 },
}

impl Mode {
    /// The transition table: every (mode, event) pair is named, so a new mode or event is a compile error here.
    /// `open` is the number of failures the review list holds and `lines` the output lines of the selected one; keys read
    /// them for `f`, for clamping the selection and for bounding the detail scroll.
    #[must_use]
    pub fn next(self, event: &Event<'_>, open: usize, lines: usize) -> Self {
        match *event {
            Event::Key(key) => self.keyed(key, open, lines),
            Event::Interrupted { signal, .. } if self.is_live() => Self::Interrupted { signal },
            Event::BuildFailed { .. } if self == Self::Running => Self::BuildError,
            Event::Finished { wall_ms, .. } if self == Self::Running => Self::Done { wall_ms },
            Event::Begin { .. }
            | Event::Waiting { .. }
            | Event::Previous { .. }
            | Event::Listed { .. }
            | Event::Built { .. }
            | Event::Resize { .. }
            | Event::Started { .. }
            | Event::Settled { .. }
            | Event::Captured { .. }
            | Event::Warned { .. }
            | Event::BlockOpened { .. }
            | Event::BlockClosed
            | Event::BuildFailed { .. }
            | Event::Interrupted { .. }
            | Event::Running { .. }
            | Event::Exited { .. }
            | Event::Tick { .. }
            | Event::Retried { .. }
            | Event::Reran { .. }
            | Event::Notice { .. }
            | Event::Finished { .. } => self.clamped(open, lines),
        }
    }

    /// A `q` is waiting for its second press.
    #[must_use]
    pub const fn is_armed(self) -> bool {
        matches!(self, Self::QuitArmed | Self::Review { armed: true, .. })
    }

    const fn disarmed(self) -> Self {
        match self {
            Self::QuitArmed => Self::Running,
            Self::Review { selected, top, .. } => Self::Review { selected, armed: false, top },
            Self::Running
            | Self::Timeline { .. }
            | Self::Receipt
            | Self::BuildError
            | Self::Interrupted { .. }
            | Self::Done { .. } => self,
        }
    }

    /// Screens that a run, a signal or a quit can still leave; the other four are final.
    #[must_use]
    pub const fn is_live(self) -> bool {
        match self {
            Self::Running
            | Self::QuitArmed
            | Self::Review { .. }
            | Self::Timeline { .. } => true,
            Self::BuildError | Self::Interrupted { .. } | Self::Done { .. } | Self::Receipt => false,
        }
    }

    fn clamped(self, open: usize, lines: usize) -> Self {
        match self {
            Self::Review { selected, armed, top } => Self::Review {
                selected: review::clamp(selected, open),
                armed,
                top: top.filter(|_| open != 0).map(|line| review::clamp(line, lines)),
            },
            Self::Running
            | Self::QuitArmed
            | Self::Timeline { .. }
            | Self::BuildError
            | Self::Interrupted { .. }
            | Self::Done { .. }
            | Self::Receipt => self,
        }
    }
}

/// The render state: the screen, the projection inputs, and the failure ledger as offsets into a caller-owned name arena.
///
/// A failure's binary and name are copied once, on its settle event, into `arena` as `binary NUL name NUL`; the ledger
/// (a caller-owned `[u32]`, its length the capacity) holds the record's start offset. The arena is the caller's storage for the whole run, so nothing in the state borrows
/// from an event, and a full arena counts the failure instead of storing it.
#[derive(Debug)]
pub struct State<'storage, const RERUN: usize> {
    mode: Mode,
    github: bool,
    agent_json: Option<bool>,
    arena: &'storage mut [u8],
    used: usize,
    failures: &'storage mut [u32],
    stored: usize,
    unstored: usize,
    inflight: &'storage mut [u8],
    inflight_used: usize,
    inflight_count: usize,
    untracked: usize,
    timed: &'storage mut [u8],
    timed_used: usize,
    timed_count: usize,
    untimed: usize,
    peak: usize,
    peak_ms: u64,
    busy_ms: u64,
    wall_ms: Option<u64>,
    previous: Option<(u64, u32)>,
    heartbeat_ms: u64,
    last_beat_ms: u64,
    now_ms: u64,
    beat_at: Option<u64>,
    settled_ms: Option<u64>,
    jobs: usize,
    done: usize,
    built: u16,
    units: Option<u16>,
    listed_binaries: usize,
    binaries_total: Option<usize>,
    listed_tests: usize,
    cols: u16,
    origin_ms: Option<u64>,
    drawn_rows: u16,
    reason: [u8; REASON_BYTES],
    reason_len: usize,
    color: bool,
    lock: [u8; LOCK_BYTES],
    lock_len: usize,
    notice: [u8; NOTICE_BYTES],
    notice_len: usize,
    armed_until_ms: u64,
    passed: usize,
    skipped: usize,
    warnings: usize,
    key_carry: [u8; review::CARRY],
    mouse: Option<bool>,
    hover: bool,
    /// `None` while no key lane reads the terminal, `Some(false)` while one does and the run goes on, `Some(true)` once review is the last screen of a finished run.
    keys: Option<bool>,
    origin: u16,
    link_template: &'storage [u8],
    link_directory: &'storage [u8],
    window: &'storage mut [u8],
    window_len: usize,
    window_of: usize,
}

impl<'storage, const RERUN: usize> State<'storage, RERUN> {
    #[must_use]
    pub const fn new(arena: &'storage mut [u8], failures: &'storage mut [u32]) -> Self {
        Self {
            mode: Mode::Running,
            github: false,
            agent_json: None,
            arena,
            used: 0,
            failures,
            stored: 0,
            unstored: 0,
            inflight: &mut [],
            inflight_used: 0,
            inflight_count: 0,
            untracked: 0,
            timed: &mut [],
            timed_used: 0,
            timed_count: 0,
            untimed: 0,
            peak: 0,
            peak_ms: 0,
            busy_ms: 0,
            wall_ms: None,
            previous: None,
            heartbeat_ms: HEARTBEAT_MS,
            last_beat_ms: 0,
            now_ms: 0,
            beat_at: None,
            settled_ms: None,
            jobs: 0,
            done: 0,
            built: 0,
            units: None,
            listed_binaries: 0,
            binaries_total: None,
            listed_tests: 0,
            cols: 80,
            origin_ms: None,
            drawn_rows: 0,
            reason: [0; REASON_BYTES],
            reason_len: 0,
            color: false,
            lock: [0; LOCK_BYTES],
            lock_len: 0,
            notice: [0; NOTICE_BYTES],
            notice_len: 0,
            armed_until_ms: 0,
            passed: 0,
            skipped: 0,
            warnings: 0,
            key_carry: [0; review::CARRY],
            mouse: None,
            hover: false,
            keys: None,
            origin: 0,
            link_template: &[],
            link_directory: &[],
            window: &mut [],
            window_len: 0,
            window_of: usize::MAX,
        }
    }

    /// Gives the state the one buffer a review loads the selected failure's output window into; without one the review shows no verbatim output.
    #[must_use]
    pub const fn with_window(mut self, window: &'storage mut [u8]) -> Self {
        self.window = window;
        self
    }

    /// Chooses whether the live frame is drawn in the terminal's own colours.
    #[must_use]
    pub const fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Turns every rich `file:line` into an OSC 8 link built from `template` (see [`crate::hyperlink::editor_template`]); an empty template leaves them as text.
    ///
    /// `directory` is the absolute directory relative panic paths resolve against. The driver resolves both at its edge, so the render reads no environment.
    #[must_use]
    pub const fn with_links(mut self, template: &'storage [u8], directory: &'storage [u8]) -> Self {
        self.link_template = template;
        self.link_directory = directory;
        self
    }

    pub(crate) const fn links(&self) -> (&[u8], &[u8]) {
        (self.link_template, self.link_directory)
    }

    /// Gives the state the byte arena its in-flight tests live in; without one every start is counted as untracked.
    #[must_use]
    pub const fn with_inflight(mut self, inflight: &'storage mut [u8]) -> Self {
        self.inflight = inflight;
        self
    }

    /// Gives the state the byte arena every settled test's start and settle times live in; without one (or with a zero-size one) nothing is timed and nothing is counted as untimed.
    #[must_use]
    pub const fn with_timeline(mut self, timed: &'storage mut [u8]) -> Self {
        self.timed = timed;
        self
    }

    #[must_use]
    pub const fn with_heartbeat_ms(mut self, heartbeat_ms: u64) -> Self {
        self.heartbeat_ms = heartbeat_ms;
        self
    }

    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// The key decoder's memory between terminal reads; see [`Key::decode`].
    #[must_use]
    pub const fn key_carry(&self) -> [u8; review::CARRY] {
        self.key_carry
    }

    pub const fn set_key_carry(&mut self, carry: [u8; review::CARRY]) {
        self.key_carry = carry;
    }

    /// Whether the review screens ask the terminal to report the mouse; the driver passes false for the setting or a terminal that cannot be read. Held as `None` (never), `Some(false)` (wanted, off now) or `Some(true)` (on now).
    pub const fn set_mouse(&mut self, mouse: bool) {
        self.mouse = if mouse { Some(false) } else { None };
    }

    /// Whether a key lane reads the terminal during the run; the live frame draws its key hints only then, since keys that cannot act are not offered.
    pub const fn set_keys_live(&mut self, live: bool) {
        self.keys = if live { Some(false) } else { None };
    }

    /// Marks review as the last screen of a finished run: `Esc` there has no run to go back to.
    pub const fn hold_review(&mut self) {
        self.keys = Some(true);
    }

    /// Whether the last frame turned mouse reporting on and nothing has turned it off since.
    #[must_use]
    pub const fn mouse_on(&self) -> bool {
        matches!(self.mouse, Some(true))
    }

    /// The ledger as borrowed views into the arena, in settle order.
    pub fn failures(&self) -> impl Iterator<Item = Identity<'_>> {
        self.failure_records().map(|(id, _)| id)
    }

    /// The ledger with each failure's recorded duration; `None` is a duration the run never observed.
    pub fn failure_records(&self) -> impl Iterator<Item = (Identity<'_>, Option<u64>)> {
        self.failures[..self.stored].iter().map(|offset| self.resolve(*offset))
    }

    /// The previous run's wall time in milliseconds and discovered test count; `None` is no previous run.
    #[must_use]
    pub const fn previous(&self) -> Option<(u64, u32)> {
        self.previous
    }

    /// Tests in flight right now.
    #[must_use]
    pub const fn inflight_len(&self) -> usize {
        self.inflight_count
    }

    /// Started tests the in-flight arena had no room for; they still settle, without a duration.
    #[must_use]
    pub const fn untracked(&self) -> usize {
        self.untracked
    }

    /// The longest-running test: its id, start timestamp, and declared deadline (`None` is unknown, never zero).
    #[must_use]
    pub fn oldest(&self) -> Option<(Identity<'_>, u64, Option<u32>)> {
        (self.inflight_count != 0).then(|| self.inflight_at(0)).map(|(start, limit, id, _)| (id, start, limit))
    }

    /// Failures that arrived after the ledger or the name arena was full; the closing block says how many.
    #[must_use]
    pub const fn unstored(&self) -> usize {
        self.unstored
    }

    pub fn apply(&mut self, event: &Event<'_>) {
        // the frame zeroes the origin when it writes the query, so zero with reporting on is exactly an outstanding query
        if matches!(event, Event::Key(Key::Cursor { .. })) && !(self.mouse_on() && self.origin == 0) {
            return;
        }
        let armed_before = self.mode.is_armed();
        if let Event::Tick { now_ms } = *event {
            self.now_ms = now_ms;
        }
        let resolved;
        let event = if let Event::Key(key) = *event {
            resolved = Event::Key(self.pointer(key));
            self.touched(key);
            &resolved
        } else {
            event
        };
        let (open, lines) = self.extent();
        let leaves_review = self.keys != Some(true) && matches!((self.mode, event), (Mode::Review { armed: false, .. }, Event::Key(Key::Esc)));
        self.mode = if leaves_review { Mode::Running } else { self.mode.next(event, open, lines) };
        if self.mode.is_armed() && !armed_before {
            self.armed_until_ms = self.now_ms.saturating_add(QUIT_WINDOW_MS);
        }
        if matches!(event, Event::Tick { .. }) && self.mode.is_armed() && self.now_ms >= self.armed_until_ms {
            self.mode = self.mode.disarmed();
        }
        self.beat_at = None;
        self.settled_ms = None;
        match *event {
            Event::Begin { github, why, json, jobs, at_ms, .. } => self.begin((github, why.is_some(), json), jobs, at_ms),
            Event::Waiting { lock } => self.wait_on(lock),
            Event::Previous { wall_ms, tests } => self.previous = Some((wall_ms, tests)),
            Event::Listed { binaries, of, tests } => (self.listed_binaries, self.binaries_total, self.listed_tests) = (binaries, of, tests),
            Event::Built { done, total } => {
                (self.built, self.units) = (done, total);
                self.lock_len = 0;
            }
            Event::Resize { cols } => self.cols = cols,
            Event::Captured { id, stdout, stderr, exit, excerpt } => {
                self.remember_reason(id, stdout.0, stderr.0);
                self.begin_attempt(id);
                self.retain(id, exit, (stdout, stderr), excerpt);
                _ = self.edit(id, |tail| entry::set_lines(tail, review::output_lines(stdout.0) + review::output_lines(stderr.0)));
            }
            Event::Started { id, at_ms, limit_ms, .. } => {
                self.watch(at_ms);
                self.track(id, at_ms, limit_ms);
                self.note_peak(at_ms);
            }
            Event::Settled { id, outcome, at_ms, .. } => {
                self.done += 1;
                if let Some(at_ms) = at_ms {
                    self.watch(at_ms);
                }
                self.settled_ms = self.untrack(id, at_ms);
                self.time(id, outcome, at_ms, self.settled_ms);
                if outcome == Outcome::Failed {
                    self.record(id, self.settled_ms);
                } else if outcome == Outcome::Passed {
                    self.passed += 1;
                } else {
                    self.skipped += 1;
                }
            }
            Event::Notice { what } => self.keep_notice(what),
            Event::Tick { now_ms } => self.beat(now_ms),
            Event::Warned { what, .. } => {
                self.warnings += 1;
                self.keep_notice(what);
            }
            Event::Retried { id, verdict } => _ = self.edit(id, |tail| entry::set_verdict(tail, verdict)),
            Event::Reran { id, rerun } => {
                let now_ms = self.now_ms;
                _ = self.edit(id, |tail| entry::set_rerun(tail, rerun, now_ms));
                if matches!(rerun, Rerun::Failed { .. }) {
                    _ = self.edit(id, entry::note_attempt);
                }
            }
            Event::Key(Key::Pointer { .. } | Key::Cursor { .. }) => {}
            Event::Key(key) => {
                self.notice_len = 0;
                self.start_reruns(key);
            }
            Event::Finished { wall_ms, .. } => {
                self.wall_ms = Some(wall_ms);
                self.clear_inflight();
            }
            _ => {}
        }
    }

    fn begin(&mut self, (github, agent, json): (bool, bool, bool), jobs: usize, at_ms: u64) {
        self.mode = Mode::Running;
        self.github = github;
        self.previous = None;
        self.agent_json = agent.then_some(json);
        self.jobs = jobs;
        self.done = 0;
        self.built = 0;
        self.units = None;
        self.listed_binaries = 0;
        self.binaries_total = None;
        self.listed_tests = 0;
        (self.passed, self.skipped, self.warnings) = (0, 0, 0);
        self.reason_len = 0;
        self.notice_len = 0;
        self.origin_ms = Some(at_ms);
        self.now_ms = at_ms;
        self.lock_len = 0;
        self.drawn_rows = 0;
        self.keys = self.keys.map(|_| false);
        self.used = 0;
        self.window_of = usize::MAX;
        self.stored = 0;
        self.unstored = 0;
        self.untracked = 0;
        self.timed_used = 0;
        self.timed_count = 0;
        self.untimed = 0;
        self.peak = 0;
        self.peak_ms = 0;
        self.busy_ms = 0;
        self.wall_ms = None;
        self.clear_inflight();
    }

    const fn clear_inflight(&mut self) {
        self.inflight_used = 0;
        self.inflight_count = 0;
    }

    fn beat(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        (0..self.stored).for_each(|index| {
            let start = self.failure_tail_start(index);
            entry::tick_running(&mut self.arena[start..start + entry::TAIL], now_ms);
        });
        self.origin_ms.get_or_insert(now_ms);
        let due = self.inflight_count != 0 && now_ms.saturating_sub(self.last_beat_ms) >= self.heartbeat_ms;
        self.beat_at = due.then_some(now_ms);
        if due {
            self.last_beat_ms = now_ms;
        }
    }

    fn inflight_at(&self, offset: usize) -> (u64, Option<u32>, Identity<'_>, usize) {
        let mut start = [0; 8];
        start.copy_from_slice(&self.inflight[offset..offset + 8]);
        let mut limit = [0; 4];
        limit.copy_from_slice(&self.inflight[offset + 8..offset + 12]);
        let declared = self.inflight[offset + 12] != 0;
        let (id, consumed) = names(&self.inflight[offset + INFLIGHT_HEAD..self.inflight_used]);
        (u64::from_le_bytes(start), declared.then(|| u32::from_le_bytes(limit)), id, offset + INFLIGHT_HEAD + consumed)
    }

    fn find(&self, id: Identity<'_>) -> Option<(usize, usize)> {
        let mut offset = 0;
        while offset < self.inflight_used {
            let (_, _, found, end) = self.inflight_at(offset);
            if found == id {
                return Some((offset, end));
            }
            offset = end;
        }
        None
    }

    fn track(&mut self, id: Identity<'_>, at_ms: u64, limit_ms: Option<u32>) {
        if let Some((start, end)) = self.find(id) {
            self.drop_span(start, end);
        }
        if self.inflight_count == 0 {
            self.last_beat_ms = at_ms;
        }
        let at = self.inflight_used;
        let end = at + INFLIGHT_HEAD + id.0.len() + id.1.len() + 2;
        if end > self.inflight.len() {
            self.untracked += 1;
            return;
        }
        self.inflight[at..at + 8].copy_from_slice(&at_ms.to_le_bytes());
        self.inflight[at + 8..at + 12].copy_from_slice(&limit_ms.unwrap_or(0).to_le_bytes());
        self.inflight[at + 12] = u8::from(limit_ms.is_some());
        write_names(&mut self.inflight[at + INFLIGHT_HEAD..end], id);
        self.inflight_used = end;
        self.inflight_count += 1;
    }

    fn untrack(&mut self, id: Identity<'_>, at_ms: Option<u64>) -> Option<u64> {
        let (start, end) = self.find(id)?;
        let started = self.inflight_at(start).0;
        self.drop_span(start, end);
        at_ms.map(|finished| finished.saturating_sub(started))
    }

    fn drop_span(&mut self, start: usize, end: usize) {
        self.inflight.copy_within(end..self.inflight_used, start);
        self.inflight_used -= end - start;
        self.inflight_count -= 1;
    }

    // a capture that arrives while the newest entry of its test is still a pending block opens a new entry, so the block keeps its own attempt
    fn begin_attempt(&mut self, id: Identity<'_>) {
        let Some(index) = self.find_failure(id).filter(|index| self.is_pending(*index)) else {
            return;
        };
        let duration = self.resolve(self.failures[index]).1;
        self.record(id, duration);
    }

    fn record(&mut self, id: Identity<'_>, duration_ms: Option<u64>) {
        let (binary, name) = id;
        let end = self.used + FAILURE_HEAD + binary.len() + name.len() + 2 + entry::TAIL;
        let offset = u32::try_from(self.used);
        match offset {
            Ok(offset) if self.stored < self.failures.len() && end <= self.arena.len() => {
                let head = self.used + FAILURE_HEAD;
                self.arena[self.used] = u8::from(duration_ms.is_some());
                self.arena[self.used + 1..head - 4].copy_from_slice(&duration_ms.unwrap_or(0).to_le_bytes());
                self.arena[head - 4..head].copy_from_slice(&u32::try_from(self.done).unwrap_or(u32::MAX).to_le_bytes());
                let names_end = end - entry::TAIL;
                write_names(&mut self.arena[head..names_end], id);
                self.arena[names_end..end].fill(0);
                self.used = end;
                self.failures[self.stored] = offset;
                self.stored += 1;
            }
            _ => self.unstored += 1,
        }
    }

    /// The most tests in flight at once the timeline calls a tail: half the jobs, and never fewer than one.
    const fn tail_limit(&self) -> usize {
        let half = self.jobs / 2;
        if half == 0 { 1 } else { half }
    }

    /// Before an event at `at_ms`, the in-flight count is what ran up to that instant: while it exceeds the tail limit, the tail has not begun.
    fn watch(&mut self, at_ms: u64) {
        if self.inflight_count > self.tail_limit() {
            self.busy_ms = at_ms.saturating_sub(self.origin_ms.unwrap_or(0));
        }
    }

    fn note_peak(&mut self, at_ms: u64) {
        if self.inflight_count > self.peak {
            self.peak = self.inflight_count;
            self.peak_ms = at_ms.saturating_sub(self.origin_ms.unwrap_or(0));
        }
    }

    /// Appends `start u32 | end u32 | outcome u8 | binary NUL name NUL`, both times in milliseconds since `Begin`; a full arena counts instead, and a zero-size arena means the view is not wanted so nothing is counted.
    fn time(&mut self, id: Identity<'_>, outcome: Outcome, finished: Option<u64>, duration: Option<u64>) {
        if outcome == Outcome::Ignored || self.timed.is_empty() {
            return;
        }
        let (Some(finished), Some(duration)) = (finished, duration) else {
            self.untimed += 1;
            return;
        };
        let origin = self.origin_ms.unwrap_or(0);
        let end = finished.saturating_sub(origin);
        let start = end.saturating_sub(duration);
        let at = self.timed_used;
        let stop = at + TIMED_HEAD + id.0.len() + id.1.len() + 2;
        if stop > self.timed.len() {
            self.untimed += 1;
            return;
        }
        self.timed[at..at + 4].copy_from_slice(&u32::try_from(start).unwrap_or(u32::MAX).to_le_bytes());
        self.timed[at + 4..at + 8].copy_from_slice(&u32::try_from(end).unwrap_or(u32::MAX).to_le_bytes());
        self.timed[at + 8] = u8::from(outcome == Outcome::Failed);
        write_names(&mut self.timed[at + TIMED_HEAD..stop], id);
        self.timed_used = stop;
        self.timed_count += 1;
    }

    /// Settled tests the timeline has no interval for, because the arena was full, the settle carried no time or the id was never started; ignored tests never held a job and are not among them.
    #[must_use]
    pub const fn untimed(&self) -> usize {
        self.untimed
    }

    // the newest attempt: a test that settles failed again appends a ledger entry, and the entries before it keep what they captured
    fn find_failure(&self, id: Identity<'_>) -> Option<usize> {
        (0..self.stored).rev().find(|index| self.resolve(self.failures[*index]).0 == id)
    }

    fn edit(&mut self, id: Identity<'_>, change: impl FnOnce(&mut [u8])) -> bool {
        let Some(index) = self.find_failure(id) else {
            return false;
        };
        self.edit_at(index, change);
        true
    }

    fn edit_at(&mut self, index: usize, change: impl FnOnce(&mut [u8])) {
        let start = self.failure_tail_start(index);
        change(&mut self.arena[start..start + entry::TAIL]);
    }

    /// Keeps a failure's exit, source-excerpt rows, store views and the classifier's record after its record in the arena, as much as the arena has room for.
    ///
    /// The first capture can only grow the last record, so one that arrives after a later failure settled is not kept and
    /// the review shows that row without output. A rerun's capture replaces the one already kept, whichever record holds
    /// it: the records after it slide to make room, and the same bound applies. The output bytes are not kept here: they stay
    /// in the run store, and the record the classifier read from them (class, location, message, left and right) is what
    /// [`State::detail`] shows, so a panic header outside the window a review loads still classifies.
    fn retain(&mut self, id: Identity<'_>, exit: Exit, streams: (Stream<'_>, Stream<'_>), excerpt: ExcerptRows<'_>) {
        let Some(index) = self.find_failure(id) else {
            return;
        };
        let start = self.failure_tail_start(index) + entry::TAIL;
        let old = entry::payload(self.failure_tail(index));
        let trailing = self.used.saturating_sub(start + old);
        let room = self.arena.len().saturating_sub(start + trailing);
        if (old == 0 && start != self.used) || room < PAYLOAD_HEAD {
            return;
        }
        let end = self.arena.len();
        self.arena.copy_within(start + old..self.used, end - trailing);
        let kept_rows = match excerpt {
            Ok(rows) => excerpt::keep(rows, &mut self.arena[start + PAYLOAD_HEAD..start + room]),
            Err(reason) => {
                let kept = reason.len().min(room - PAYLOAD_HEAD);
                self.arena[start + PAYLOAD_HEAD..start + PAYLOAD_HEAD + kept].copy_from_slice(&reason[..kept]);
                kept
            }
        };
        let (tag, value) = match exit {
            Exit::Code(code) => (0, code),
            Exit::Signal(signal) => (1, signal),
            Exit::DeadlineKilled => (2, 0),
        };
        self.arena[start] = tag;
        self.arena[start + 1..start + EXIT_HEAD].copy_from_slice(&value.to_le_bytes());
        self.arena[start + EXIT_HEAD..start + EXIT_HEAD + 2].copy_from_slice(&u16::try_from(kept_rows).unwrap_or(0).to_le_bytes());
        self.arena[start + VIEWS_AT..start + RECORD_AT].fill(0);
        self.arena[start + VIEWS_AT - 1] = u8::from(shows_stderr(streams, exit)) | if excerpt.is_err() { REASON_FLAG } else { 0 };
        let kept_record = self.keep_record(start, (start + PAYLOAD_HEAD + kept_rows, start + room), sited(streams, exit));
        self.arena.copy_within(end - trailing..end, start + PAYLOAD_HEAD + kept_rows + kept_record);
        self.used = start + PAYLOAD_HEAD + kept_rows + kept_record + trailing;
        let length = PAYLOAD_HEAD + kept_rows + kept_record;
        let (before, after) = (u32::try_from(old).unwrap_or(0), u32::try_from(length).unwrap_or(0));
        self.failures[index + 1..self.stored].iter_mut().for_each(|offset| *offset = *offset + after - before);
        _ = self.edit(id, |tail| entry::set_payload(tail, length));
        self.window_of = usize::MAX;
    }

    fn keep_record(&mut self, start: usize, (mut at, limit): (usize, usize), record: Result<Record<'_>, Reject>) -> usize {
        let began = at;
        let Ok(found) = record else {
            self.arena[start + RECORD_AT] = CLASS_NONE;
            return 0;
        };
        let (tag, arg) = match found.class {
            Class::Assert => (0, 0),
            Class::Panic => (1, 0),
            Class::Timeout { enforced_by: Enforcer::Val } => (2, 0),
            Class::Timeout { enforced_by: Enforcer::Harness } => (3, 0),
            Class::Hang => (4, 0),
            Class::Crash { signal } => (5, signal),
        };
        let (kind, site) = match found.location {
            Ok(site) => (1, site),
            Err(Absent::External(site)) => (2, site),
            Err(Absent::NoPanicSite) => (0, Location { file: b"", line: 0, col: 0 }),
        };
        let (left, right) = found.sides.unwrap_or((b"", b""));
        let pieces: [(&[u8], usize); 4] = [(site.file, FILE_KEEP), (found.message, MESSAGE_KEEP), (left, SIDE_KEEP), (right, SIDE_KEEP)];
        let mut lengths = [0_u16; 4];
        for (slot, (bytes, cap)) in lengths.iter_mut().zip(pieces) {
            let kept = bytes.len().min(cap).min(limit - at);
            self.arena[at..at + kept].copy_from_slice(&bytes[..kept]);
            *slot = u16::try_from(kept).unwrap_or(0);
            at += kept;
        }
        let head = &mut self.arena[start + RECORD_AT..start + PAYLOAD_HEAD];
        head[0] = tag;
        head[1..5].copy_from_slice(&arg.to_le_bytes());
        head[5] = kind;
        head[6..10].copy_from_slice(&site.line.to_le_bytes());
        head[10..14].copy_from_slice(&site.col.to_le_bytes());
        head[14] = u8::from(found.sides.is_some());
        lengths.iter().enumerate().for_each(|(position, length)| head[15 + position * 2..17 + position * 2].copy_from_slice(&length.to_le_bytes()));
        at - began
    }

    /// Records where the failure's output lives in the run store: its stdout and stderr chains, as the driver sealed them.
    pub fn keep_views(&mut self, id: Identity<'_>, views: [(u32, u64, u64, u64); 2]) {
        let Some(index) = self.find_failure(id) else {
            return;
        };
        let start = self.failure_tail_start(index) + entry::TAIL;
        if entry::payload(self.failure_tail(index)) < PAYLOAD_HEAD {
            return;
        }
        for (position, (key, first, last, total)) in views.into_iter().enumerate() {
            let at = start + VIEWS_AT + position * VIEW_BYTES;
            self.arena[at..at + 4].copy_from_slice(&key.to_le_bytes());
            self.arena[at + 4..at + 12].copy_from_slice(&first.to_le_bytes());
            self.arena[at + 12..at + 20].copy_from_slice(&last.to_le_bytes());
            self.arena[at + 20..at + 28].copy_from_slice(&total.to_le_bytes());
        }
        self.window_of = usize::MAX;
    }

    /// Queues the newest stored attempt of the failure `id` as a block that prints once the scrollback reaches `position`, and returns its ledger index.
    ///
    /// `None` means the arena had no room for the failure; the caller prints the block from the event instead. The ledger entry is
    /// the queue: its views, excerpt and links are the attempt's own, and a later attempt of the same test is a new entry.
    pub fn queue_block(&mut self, id: Identity<'_>, position: u64) -> Option<usize> {
        let index = self.find_failure(id)?;
        if entry::payload(self.failure_tail(index)) < PAYLOAD_HEAD {
            return None;
        }
        let start = self.failure_tail_start(index) + entry::TAIL;
        self.arena[start + VIEWS_AT - 1] |= BLOCK_PENDING;
        self.arena[start + BLOCK_AT..start + BLOCK_AT + 8].copy_from_slice(&position.to_le_bytes());
        Some(index)
    }

    /// The oldest ledger entry at or after `from` whose block still waits, with the scrollback position it prints at.
    #[must_use]
    pub fn pending_block(&self, from: usize) -> Option<(usize, u64)> {
        (from..self.stored).find(|index| self.is_pending(*index)).map(|index| {
            let start = self.failure_tail_start(index) + entry::TAIL;
            let mut position = [0; 8];
            position.copy_from_slice(&self.arena[start + BLOCK_AT..start + BLOCK_AT + 8]);
            (index, u64::from_le_bytes(position))
        })
    }

    /// Marks the block of the ledger entry at `index` printed.
    pub fn finish_block(&mut self, index: usize) {
        let start = self.failure_tail_start(index) + entry::TAIL;
        self.arena[start + VIEWS_AT - 1] &= !BLOCK_PENDING;
    }

    fn is_pending(&self, index: usize) -> bool {
        let start = self.failure_tail_start(index) + entry::TAIL;
        entry::payload(self.failure_tail(index)) >= PAYLOAD_HEAD && self.arena[start + VIEWS_AT - 1] & BLOCK_PENDING != 0
    }

    /// The name of the queued failure at ledger `index`.
    pub(crate) fn block_at(&self, index: usize) -> Identity<'_> {
        self.resolve(self.failures[index]).0
    }

    /// The two store chains of the attempt the ledger entry at `index` kept.
    pub(crate) fn block_views(&self, index: usize) -> [(u32, u64, u64, u64); 2] {
        self.views_at(index).1
    }

    /// The editor template, its directory and the panic site of the attempt at ledger `index`, the links its pages carry.
    pub(crate) fn block_links(&self, index: usize) -> (&[u8], &[u8], Option<Location<'_>>) {
        let site = if self.link_template.is_empty() { None } else { self.detail_at(index).1 };
        (self.link_template, self.link_directory, site)
    }

    /// The source excerpt the queued failure at `index` was captured with.
    pub(crate) fn block_excerpt(&self, index: usize) -> ExcerptRows<'_> {
        let start = self.failure_tail_start(index) + entry::TAIL;
        if self.arena[start + VIEWS_AT - 1] & REASON_FLAG == 0 {
            return Ok(self.excerpt_at(index));
        }
        let length = usize::from(u16::from_le_bytes([self.arena[start + EXIT_HEAD], self.arena[start + EXIT_HEAD + 1]]));
        Err(&self.arena[start + PAYLOAD_HEAD..start + PAYLOAD_HEAD + length])
    }

    fn views_at(&self, index: usize) -> (u8, [(u32, u64, u64, u64); 2]) {
        let start = self.failure_tail_start(index) + entry::TAIL;
        if entry::payload(self.failure_tail(index)) < PAYLOAD_HEAD {
            return (0, [(0, 0, 0, 0); 2]);
        }
        let number = |at: usize, width: usize| {
            let mut bytes = [0; 8];
            bytes[..width].copy_from_slice(&self.arena[start + at..start + at + width]);
            u64::from_le_bytes(bytes)
        };
        let view = |position: usize| {
            let at = VIEWS_AT + position * VIEW_BYTES;
            (u32::try_from(number(at, 4)).unwrap_or(0), number(at + 4, 8), number(at + 12, 8), number(at + 20, 8))
        };
        (self.arena[start + VIEWS_AT - 1] & WHICH_STDERR, [view(0), view(1)])
    }

    /// Loads, through `load`, the stream window the review detail of the selected failure shows, once per selection.
    ///
    /// `load` receives the failure's chain for the stream the review shows and the state's one window buffer, and returns
    /// the bytes it put there. Nothing is loaded outside review, so a plain or agent run never touches the buffer.
    pub fn fill_window(&mut self, load: impl FnOnce((u32, u64, u64, u64), &mut [u8]) -> usize) {
        let Mode::Review { selected, .. } = self.mode else {
            return;
        };
        let Some(index) = self.listed(selected) else {
            return;
        };
        if self.window_of == index {
            return;
        }
        let (which, views) = self.views_at(index);
        self.window_len = load(views[usize::from(which)], &mut *self.window);
        self.window_of = index;
        let lines = review::output_lines(&self.window[..self.window_len]);
        self.edit_at(index, |tail| entry::set_lines(tail, lines));
    }

    /// The source-excerpt rows [`State::retain`] kept for the failure at ledger `index`; empty when there are none.
    pub(super) fn excerpt_at(&self, index: usize) -> &[u8] {
        let length = entry::payload(self.failure_tail(index));
        if length < PAYLOAD_HEAD {
            return b"";
        }
        let start = self.failure_tail_start(index) + entry::TAIL;
        let mut size = [0; 2];
        size.copy_from_slice(&self.arena[start + EXIT_HEAD..start + EXIT_HEAD + 2]);
        if self.arena[start + VIEWS_AT - 1] & REASON_FLAG != 0 {
            return b"";
        }
        &self.arena[start + PAYLOAD_HEAD..start + PAYLOAD_HEAD + usize::from(u16::from_le_bytes(size))]
    }

    fn panic_site<'a>(&self, (stdout, stderr): (Stream<'a>, Stream<'a>), exit: Exit) -> Option<Location<'a>> {
        if self.link_template.is_empty() {
            return None;
        }
        [stdout.0, stderr.0].into_iter().find_map(|capture| match classify(capture, exit, 0).ok()?.location {
            Ok(site) | Err(Absent::External(site)) => Some(site),
            Err(Absent::NoPanicSite) => None,
        })
    }

    /// A failure's class, source location, the classifier's reason and the output window a review loaded for it.
    ///
    /// The class, location and reason are the record [`State::retain`] kept, so they hold when the panic header is not in the
    /// loaded window; the window is empty until [`State::fill_window`] has loaded it for this failure.
    #[must_use]
    pub fn detail(&self, id: Identity<'_>) -> review::Detail<'_> {
        self.find_failure(id).map_or((Class::Panic, None, (b"", None), b"", b""), |index| self.detail_at(index))
    }

    fn detail_at(&self, index: usize) -> review::Detail<'_> {
        let start = self.failure_tail_start(index) + entry::TAIL;
        if entry::payload(self.failure_tail(index)) < PAYLOAD_HEAD {
            return (Class::Panic, None, (b"", None), b"", b"");
        }
        let rows = self.excerpt_at(index);
        let skipped = usize::from(u16::from_le_bytes([self.arena[start + EXIT_HEAD], self.arena[start + EXIT_HEAD + 1]]));
        let capture = if self.window_of == index { &self.window[..self.window_len] } else { &[][..] };
        let head = &self.arena[start + RECORD_AT..start + PAYLOAD_HEAD];
        if head[0] == CLASS_NONE {
            return (Class::Panic, None, (b"", None), rows, capture);
        }
        let number = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let length_of = |position: usize| usize::from(u16::from_le_bytes([head[15 + position * 2], head[16 + position * 2]]));
        let class = match head[0] {
            0 => Class::Assert,
            2 => Class::Timeout { enforced_by: Enforcer::Val },
            3 => Class::Timeout { enforced_by: Enforcer::Harness },
            4 => Class::Hang,
            5 => Class::Crash { signal: number(1).cast_signed() },
            _ => Class::Panic,
        };
        let mut at = start + PAYLOAD_HEAD + skipped;
        let mut next = |position: usize| {
            let piece = &self.arena[at..at + length_of(position)];
            at += piece.len();
            piece
        };
        let (file, message, left, right) = (next(0), next(1), next(2), next(3));
        let site = (head[5] != 0).then_some(Location { file, line: number(6), col: number(10) });
        (class, site, (message, (head[14] != 0).then_some((left, right))), rows, capture)
    }

    fn failure_tail_start(&self, index: usize) -> usize {
        let offset = self.failures[index] as usize;
        let (binary, name) = self.resolve(self.failures[index]).0;
        offset + FAILURE_HEAD + binary.len() + name.len() + 2
    }

    fn failure_tail(&self, index: usize) -> &[u8] {
        let start = self.failure_tail_start(index);
        &self.arena[start..start + entry::TAIL]
    }

    fn extent(&self) -> (usize, usize) {
        let open = self.open();
        let lines = match self.mode {
            Mode::Review { selected, .. } => self.listed(selected).map_or(0, |index| entry::lines(self.failure_tail(index))),
            _ => 0,
        };
        (open, lines)
    }

    // a launch that failed in review is said once, on the footer, until the next key
    fn keep_notice(&mut self, what: &[u8]) {
        if matches!(self.mode, Mode::Review { .. }) {
            let kept = what.len().min(NOTICE_BYTES);
            self.notice[..kept].copy_from_slice(&what[..kept]);
            self.notice_len = kept;
        }
    }

    pub(super) fn attempts(&self, index: usize) -> u8 {
        entry::attempts(self.failure_tail(index))
    }

    pub(super) fn notice(&self) -> &[u8] {
        &self.notice[..self.notice_len]
    }

    // r and R are state before they are processes: the row flips to RERUN here, and the driver launches whichever rows read RERUN with no child behind them
    fn start_reruns(&mut self, key: Key) {
        let Mode::Review { selected, .. } = self.mode else {
            return;
        };
        match key {
            Key::Char(b'r') => self.listed(selected).into_iter().for_each(|index| self.begin_rerun(index)),
            Key::Char(b'R') => {
                for index in 0..self.stored {
                    if self.is_listed(index) {
                        self.begin_rerun(index);
                    }
                }
            }
            _ => {}
        }
    }

    fn begin_rerun(&mut self, index: usize) {
        if !matches!(entry::rerun(self.failure_tail(index)), Rerun::Idle | Rerun::Failed { .. }) {
            return;
        }
        let last = self.resolve(self.failures[index]).1;
        let now_ms = self.now_ms;
        let start = self.failure_tail_start(index);
        let running = Rerun::Running { elapsed_ms: 0, expected_ms: last.filter(|millis| *millis != 0) };
        entry::set_rerun(&mut self.arena[start..start + entry::TAIL], running, now_ms);
    }

    /// The failures whose rerun the review started and no child has settled yet, in ledger order.
    pub fn rerunning(&self) -> impl Iterator<Item = Identity<'_>> {
        (0..self.stored)
            .filter(|index| matches!(entry::rerun(self.failure_tail(*index)), Rerun::Running { .. }))
            .map(|index| self.resolve(self.failures[index]).0)
    }

    pub(super) fn is_listed(&self, index: usize) -> bool {
        match entry::rerun(self.failure_tail(index)) {
            Rerun::Passed { .. } => self.now_ms < entry::cleared_at(self.failure_tail(index)).saturating_add(PASS_SHOWN_MS),
            Rerun::Idle | Rerun::Running { .. } | Rerun::Failed { .. } => true,
        }
    }

    pub(super) fn open(&self) -> usize {
        (0..self.stored).filter(|index| self.is_listed(*index)).count()
    }

    pub(super) fn listed(&self, position: u16) -> Option<usize> {
        let last = self.open().checked_sub(1)?;
        (0..self.stored).filter(|index| self.is_listed(*index)).nth(usize::from(position).min(last))
    }

    pub(super) fn ledger_entry(&self, index: usize) -> (Identity<'_>, u64, Option<Verdict>, Rerun) {
        let tail = self.failure_tail(index);
        let (id, millis) = self.resolve(self.failures[index]);
        (id, millis.unwrap_or(0), entry::verdict(tail), entry::rerun(tail))
    }

    fn wait_on(&mut self, lock: &[u8]) {
        let kept = lock.len().min(LOCK_BYTES);
        self.lock[..kept].copy_from_slice(&lock[..kept]);
        self.lock_len = kept;
    }

    fn remember_reason(&mut self, id: Identity<'_>, stdout: &[u8], stderr: &[u8]) {
        let first = self.failures().next();
        if self.reason_len != 0 || first != Some(id) {
            return;
        }
        let Ok(record) = classify(stderr, Exit::Code(101), 0).or_else(|_| classify(stdout, Exit::Code(101), 0)) else {
            return;
        };
        let line = &record.message[..memchr(b'\n', record.message).unwrap_or(record.message.len())];
        let kept = line.len().min(REASON_BYTES);
        self.reason[..kept].copy_from_slice(&line[..kept]);
        self.reason_len = kept;
    }

    /// The settled-test count at which each stored failure landed, for the tick row.
    pub fn failure_positions(&self) -> impl Iterator<Item = usize> + '_ {
        self.failures[..self.stored].iter().map(|offset| {
            let start = *offset as usize;
            let mut done = [0; 4];
            done.copy_from_slice(&self.arena[start + 9..start + FAILURE_HEAD]);
            u32::from_le_bytes(done) as usize
        })
    }

    fn resolve(&self, offset: u32) -> (Identity<'_>, Option<u64>) {
        let start = offset as usize;
        let mut duration = [0; 8];
        duration.copy_from_slice(&self.arena[start + 1..start + 9]);
        let measured = self.arena[start] != 0;
        let (id, _) = names(&self.arena[start + FAILURE_HEAD..self.used]);
        (id, measured.then(|| u64::from_le_bytes(duration)))
    }

    /// Appends the lines `event` warrants, given the state after `apply`. Err carries the bytes the frame could not hold.
    ///
    /// # Errors
    ///
    /// Returns the number of bytes cut when the frame was too small; the frame then holds a truncated projection.
    pub fn project<const FRAME: usize>(&self, event: &Event<'_>, frame: &mut Line<FRAME>) -> Result<(), usize> {
        if let Some(json) = self.agent_json {
            if json {
                self.project_json(event, frame);
            } else {
                self.project_agent(event, frame);
            }
            return Self::checked(frame);
        }
        match *event {
            Event::Begin { version, shard, jobs, .. } => header(frame, version, shard, jobs),
            Event::Listed { binaries, tests, .. } => listed(frame, binaries, tests),
            Event::Built { .. } | Event::Resize { .. } | Event::Waiting { .. } | Event::Previous { .. } | Event::Exited { .. } | Event::Key(_) | Event::Retried { .. } | Event::Reran { .. } | Event::Notice { .. } => {}
            Event::Started { id, solo, .. } => event_line(frame, b"START", id, None, solo),
            Event::Settled { id, outcome, solo, .. } => self.settled(frame, id, outcome, self.settled_ms, solo),
            Event::Tick { .. } => self.heartbeat(frame),
            Event::Captured { id, stdout, stderr, exit, excerpt } => self.captured(frame, id, (stdout, stderr, exit), excerpt),
            Event::Warned { id, what } => warned(frame, id, what),
            Event::BlockOpened { id } => block_open(frame, id, STDOUT_STREAM),
            Event::BlockClosed => block_close(frame, STDOUT_STREAM),
            Event::BuildFailed { diagnostic, .. } => text_line(frame, b"BUILD", b"failed  ", diagnostic),
            Event::Interrupted { signal, groups } => interrupted(frame, signal, groups),
            Event::Running { id, group, age_ms } => killed(frame, id, group, age_ms),
            Event::Finished { passed, failed, ignored, not_run, wall_ms, manifest, discovered, filtered, .. } => {
                let extras = matches!(self.mode, Mode::Interrupted { .. }).then_some([discovered, filtered]);
                self.closing(frame, [passed, failed, ignored, not_run], wall_ms, manifest, extras);
            }
        }
        Self::checked(frame)
    }

    fn checked<const FRAME: usize>(frame: &Line<FRAME>) -> Result<(), usize> {
        match frame.lost() {
            0 => Ok(()),
            lost => Err(lost),
        }
    }

    fn heartbeat<const FRAME: usize>(&self, frame: &mut Line<FRAME>) {
        let Some((id, start, limit)) = self.oldest().filter(|_| self.beat_at.is_some()) else {
            return;
        };
        label(frame, b"RUNNING");
        frame.text(id.0);
        frame.text(b"  ");
        frame.text(id.1);
        frame.text(b"  ");
        seconds(frame, self.now_ms.saturating_sub(start));
        if let Some(limit) = limit {
            frame.text(b"  limit=");
            seconds(frame, u64::from(limit));
        }
        frame.text(b"  (oldest of ");
        frame.number(self.inflight_count);
        frame.text(b" running)\n");
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
        (stdout, stderr, exit): (Stream<'_>, Stream<'_>, Exit),
        excerpt: ExcerptRows<'_>,
    ) {
        self.captured_open(frame, id);
        let links = self.stream_links((stdout, stderr), exit);
        stream(frame, id, STDOUT_STREAM, stdout, links);
        stream(frame, id, STDERR_STREAM, stderr, links);
        self.captured_close(frame, id, excerpt);
    }

    /// Opens a failure block: the GitHub group line when the run asked for annotations.
    pub(crate) fn captured_open<const FRAME: usize>(&self, frame: &mut Line<FRAME>, id: Identity<'_>) {
        if self.github {
            frame.text(b"::group::");
            frame.text(id.0);
            frame.text(b" ");
            frame.text(id.1);
            frame.text(b"\n");
        }
    }

    /// Ends a failure block with its source excerpt and the GitHub group end.
    pub(crate) fn captured_close<const FRAME: usize>(&self, frame: &mut Line<FRAME>, id: Identity<'_>, excerpt: ExcerptRows<'_>) {
        source_block(frame, id, excerpt);
        if self.github {
            frame.text(b"::endgroup::\n");
        }
    }

    /// The editor template, its directory and the panic site the stream pages are linked with.
    pub(crate) fn stream_links<'a>(&'a self, streams: (Stream<'a>, Stream<'a>), exit: Exit) -> (&'a [u8], &'a [u8], Option<Location<'a>>) {
        (self.link_template, self.link_directory, self.panic_site(streams, exit))
    }

    fn passed_alone(&self, index: usize) -> bool {
        matches!(entry::rerun(self.failure_tail(index)), Rerun::Passed { .. })
    }

    fn unresolved(&self) -> impl Iterator<Item = Identity<'_>> {
        (0..self.stored).filter(|index| !self.passed_alone(*index)).map(|index| self.resolve(self.failures[index]).0)
    }

    fn closing<const FRAME: usize>(
        &self,
        frame: &mut Line<FRAME>,
        [passed, failed, ignored, not_run]: [usize; 4],
        wall_ms: u64,
        manifest: Option<&[u8]>,
        extras: Option<[usize; 2]>,
    ) {
        let flaky = (0..self.stored).filter(|index| self.passed_alone(*index)).count();
        let failed = failed.saturating_sub(flaky);
        label(frame, b"SUMMARY");
        for (key, value) in [(&b"passed="[..], passed), (b" failed=", failed)] {
            frame.text(key);
            frame.number(value);
        }
        if flaky != 0 {
            frame.text(b" flaky=");
            frame.number(flaky);
        }
        for (key, value) in [(&b" ignored="[..], ignored), (b" not_run=", not_run)] {
            frame.text(key);
            frame.number(value);
        }
        frame.text(b"  wall=");
        seconds(frame, wall_ms);
        if let Some([discovered, filtered]) = extras {
            frame.text(b" discovered=");
            frame.number(discovered);
            frame.text(b" run=");
            frame.number(passed + failed);
            frame.text(b" filtered=");
            frame.number(filtered);
        }
        frame.text(b"\n");
        for (target, name) in self.unresolved() {
            label(frame, b"FAILED");
            frame.text(target);
            frame.text(b" ");
            frame.text(name);
            frame.text(b"\n");
        }
        for (target, name) in (0..self.stored).filter(|index| self.passed_alone(*index)).map(|index| self.resolve(self.failures[index]).0) {
            label(frame, b"FLAKY");
            frame.text(target);
            frame.text(b" ");
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
        for (position, (target, name)) in self.unresolved().take(included).enumerate() {
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
        let omitted = self.unresolved().count() - included + self.unstored;
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
        for (target, name) in self.unresolved() {
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

fn names(bytes: &[u8]) -> (Identity<'_>, usize) {
    let binary_end = memchr(0, bytes).unwrap_or(bytes.len());
    let rest = &bytes[(binary_end + 1).min(bytes.len())..];
    let name_end = memchr(0, rest).unwrap_or(rest.len());
    ((&bytes[..binary_end], &rest[..name_end]), binary_end + name_end + 2)
}

fn write_names(target: &mut [u8], (binary, name): Identity<'_>) {
    let binary_end = binary.len();
    target[..binary_end].copy_from_slice(binary);
    target[binary_end] = 0;
    target[binary_end + 1..binary_end + 1 + name.len()].copy_from_slice(name);
    target[binary_end + 1 + name.len()] = 0;
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

fn text_pair<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], id: Identity<'_>) {
    label(frame, name);
    frame.text(id.0);
    frame.text(b" ");
    frame.text(id.1);
    frame.text(b"\n");
}

fn killed<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, group: u32, age_ms: u64) {
    label(frame, b"RUNNING");
    frame.text(id.0);
    frame.text(b"  ");
    frame.text(id.1);
    frame.text(b"  ");
    seconds(frame, age_ms);
    frame.text(b"  killed group=");
    frame.number(usize::try_from(group).unwrap_or(usize::MAX));
    frame.text(b"\n");
}

fn interrupted<const FRAME: usize>(frame: &mut Line<FRAME>, signal: u8, groups: usize) {
    label(frame, b"INTERRUPTED");
    frame.text(b" signal=");
    frame.number(usize::from(signal));
    frame.text(b"  forwarded_groups=");
    frame.number(groups);
    frame.text(b"\n");
}

// the review shows the stream whose classifier record has a site, falling back to stderr when it has any bytes
fn shows_stderr((stdout, stderr): (Stream<'_>, Stream<'_>), exit: Exit) -> bool {
    let sited = |(bytes, dropped): Stream<'_>| classify(bytes, exit, dropped).is_ok_and(|record| record.location.is_ok() || matches!(record.location, Err(Absent::External(_))));
    if sited(stdout) {
        false
    } else {
        sited(stderr) || !stderr.0.is_empty()
    }
}

fn block_open<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, name: &[u8]) {
    frame.text(b"------ ");
    frame.text(name);
    frame.text(b": ");
    frame.text(id.0);
    frame.text(b" ");
    frame.text(id.1);
    frame.text(b" ------\n");
}

fn block_close<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8]) {
    frame.text(b"------ end ");
    frame.text(name);
    frame.text(b" ------\n");
}

fn source_block<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, excerpt: ExcerptRows<'_>) {
    if excerpt == UNREAD {
        return;
    }
    block_open(frame, id, SOURCE_STREAM);
    match excerpt {
        Ok(rows) => frame.text(rows),
        Err(reason) => {
            frame.text(b"source absent: ");
            frame.text(reason);
            frame.text(b"\n");
        }
    }
    block_close(frame, SOURCE_STREAM);
}

fn stream<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, name: &[u8], (bytes, dropped): Stream<'_>, links: (&[u8], &[u8], Option<Location<'_>>)) {
    if bytes.is_empty() {
        return;
    }
    stream_open(frame, id, name);
    stream_page(frame, bytes, links);
    stream_close(frame, name, (bytes.last() == Some(&b'\n'), dropped));
}

/// Opens the fenced block of one stream; the pages follow through [`stream_page`] and [`stream_close`] ends it.
pub(crate) fn stream_open<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, name: &[u8]) {
    block_open(frame, id, name);
}

/// Writes one page of a stream's bytes, linking the panic site when the run has an editor template.
pub(crate) fn stream_page<const FRAME: usize>(frame: &mut Line<FRAME>, bytes: &[u8], (template, directory, site): (&[u8], &[u8], Option<Location<'_>>)) {
    match site {
        Some(site) => write_linked(frame, (template, directory), bytes, (site.file, site.line, site.col)),
        None => frame.text(bytes),
    }
}

/// Ends the fenced block: a newline when the last byte was not one, and the count of bytes the run could not keep.
pub(crate) fn stream_close<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], (newline_ended, dropped): (bool, u64)) {
    if !newline_ended {
        frame.text(b"\n");
    }
    if dropped != 0 {
        frame.text(b"[output truncated: ");
        frame.number(usize::try_from(dropped).unwrap_or(usize::MAX));
        frame.text(b" bytes dropped]\n");
    }
    block_close(frame, name);
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use super::{Event, Identity, Mode, Outcome, State, UNREAD};
    use crate::failure_class::Exit;
    use crate::line::Line;
    use crate::test_alloc::measure_allocations;

    const FAILING: Identity<'static> = (b"cli_run", b"fixture::fails_when_requested");
    const PASSING: Identity<'static> = (b"cli_run", b"fixture::passes");

    type Small<'storage> = State<'storage, 200>;

    fn projected<const RERUN: usize>(
        state: &State<'_, RERUN>,
        event: &Event<'_>,
    ) -> String {
        let mut frame = Line::<4096>::new();
        state.project(event, &mut frame).expect("the frame holds one event");
        String::from_utf8(frame.bytes().to_vec()).expect("plain output is utf-8")
    }

    fn settle(state: &mut Small<'_>, id: Identity<'static>, outcome: Outcome) -> String {
        state.apply(&Event::Started { id, solo: false, at_ms: 0, limit_ms: None });
        let event = Event::Settled { id, outcome, at_ms: Some(3123), solo: false };
        state.apply(&event);
        projected(state, &event)
    }

    #[test]
    fn a_settled_failure_projects_its_fail_line_and_joins_the_ledger() {
        let mut inflight = [0_u8; 256];
        let mut arena = [0_u8; 512];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger).with_inflight(&mut inflight);

        let line = settle(&mut state, FAILING, Outcome::Failed);

        assert_eq!(line, "FAIL     cli_run  fixture::fails_when_requested  3.123s\n");
        assert_eq!(state.failures().collect::<Vec<_>>(), [FAILING]);
    }

    #[test]
    fn the_previous_run_reaches_the_state_without_changing_a_byte_of_any_output_mode() {
        let begins = [(None, false), (Some(b"flag".as_slice()), false), (Some(b"flag".as_slice()), true)];
        for (why, json) in begins {
            let mut arena = [0_u8; 64];
            let mut ledger = [0_u32; 2];
            let mut state = Small::new(&mut arena, &mut ledger);
            state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why, json, at_ms: 0 });
            assert_eq!(state.previous(), None);
            let previous = Event::Previous { wall_ms: 258_230, tests: 383 };
            let mut frame = Line::<256>::new();

            let ((), allocations) = measure_allocations(|| {
                for _ in 0..10_000 {
                    state.apply(&previous);
                }
            });
            state.project(&previous, &mut frame).expect("the frame holds the projection");

            assert_eq!((state.previous(), frame.bytes(), allocations), (Some((258_230, 383)), b"".as_slice(), 0), "agent_like={why:?} json={json}");
        }
    }

    #[test]
    fn the_live_view_keeps_searching_for_tests_after_a_previous_run_until_a_binary_is_known() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        let mut frame = Line::<4096>::new();
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });
        let previous = Event::Previous { wall_ms: 1, tests: 2 };
        state.apply(&previous);

        state.project_live(&previous, &mut frame).expect("the live view accepts the event");

        assert!(String::from_utf8_lossy(frame.bytes()).contains(" searching for tests"), "{}", String::from_utf8_lossy(frame.bytes()));
        assert_eq!(state.previous(), Some((1, 2)));
    }

    #[test]
    fn github_adds_the_error_annotation_after_the_fail_line_and_wraps_captured_output_in_a_group() {
        let mut arena = [0_u8; 512];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 8, github: true, why: None, json: false, at_ms: 0 });

        let fail = settle(&mut state, FAILING, Outcome::Failed);
        let captured = Event::Captured { id: FAILING, stdout: (b"requested fixture failure", 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD };
        let block = projected(&state, &captured);

        assert!(fail.ends_with("::error title=cargo val::cli_run::fixture::fails_when_requested failed\n"), "{fail}");
        assert_eq!(
            block,
            "::group::cli_run fixture::fails_when_requested\n------ stdout: cli_run fixture::fails_when_requested ------\nrequested fixture failure\n------ end stdout ------\n::endgroup::\n"
        );
    }

    #[test]
    fn the_closing_block_lists_every_failure_and_one_command_that_reruns_them() {
        let mut arena = [0_u8; 512];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        settle(&mut state, PASSING, Outcome::Passed);
        settle(&mut state, FAILING, Outcome::Failed);
        let finished = Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: 8400, manifest: Some(b"/w/Cargo.toml"), discovered: 0, filtered: 0, exit: 0 };
        state.apply(&finished);

        let block = projected(&state, &finished);

        assert_eq!(
            block,
            "SUMMARY  passed=1 failed=1 ignored=0 not_run=0  wall=8.400s\n\
             FAILED   cli_run fixture::fails_when_requested\n\
             RERUN    cargo val --manifest-path '/w/Cargo.toml' --no-fail-fast -E '(binary(=cli_run) & test(=fixture::fails_when_requested))'\n"
        );
        assert_eq!(state.mode(), Mode::Done { wall_ms: 8400 }, "a finish with failures closes the screen too, so no tick redraws a frame under the closing block");
    }

    #[test]
    fn failures_past_the_ledger_are_counted_and_named_in_the_closing_block() {
        let mut arena = [0_u8; 512];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        let names: [Identity<'static>; 4] = [(b"a", b"one"), (b"a", b"two"), (b"a", b"three"), (b"a", b"four")];
        for id in names {
            settle(&mut state, id, Outcome::Failed);
        }
        let finished = Event::Finished { passed: 0, failed: 4, ignored: 0, not_run: 0, wall_ms: 10, manifest: None, discovered: 0, filtered: 0, exit: 0 };

        let block = projected(&state, &finished);

        assert_eq!(state.unstored(), 2);
        assert!(block.contains("FAILED   +2 failures not listed\n"), "{block}");
        assert!(block.contains("  (2 more failures are not in this command)\n"), "{block}");
    }

    #[test]
    fn a_failure_whose_names_do_not_fit_the_arena_is_counted_not_stored() {
        let mut arena = [0_u8; 112];
        let mut ledger = [0_u32; 8];
        let mut state = State::<200>::new(&mut arena, &mut ledger);
        let long: Identity<'static> = (b"cli_run", b"fixture::a_test_name_longer_than_the_arena_has_room_for");

        settle_any(&mut state, FAILING);
        settle_any(&mut state, long);

        assert_eq!(state.failures().collect::<Vec<_>>(), [FAILING]);
        assert_eq!(state.unstored(), 1);
    }

    fn settle_any(state: &mut State<'_, 200>, id: Identity<'_>) {
        state.apply(&Event::Settled { id, outcome: Outcome::Failed, at_ms: None, solo: false });
    }

    #[test]
    fn a_rerun_command_that_outgrows_its_budget_says_how_many_failures_it_left_out() {
        let mut arena = [0_u8; 256];
        let mut ledger = [0_u32; 4];
        let mut state = State::<40>::new(&mut arena, &mut ledger);
        for id in [(&b"a"[..], &b"one"[..]), (b"a", b"two"), (b"a", b"three")] {
            state.apply(&Event::Settled { id, outcome: Outcome::Failed, at_ms: None, solo: false });
        }
        let finished = Event::Finished { passed: 0, failed: 3, ignored: 0, not_run: 0, wall_ms: 10, manifest: None, discovered: 0, filtered: 0, exit: 0 };

        let block = projected(&state, &finished);

        let rerun = block.lines().find(|line| line.starts_with("RERUN")).expect("a rerun line");
        assert_eq!(rerun, "RERUN    cargo val --no-fail-fast -E '(binary(=a) & test(=one))'  (2 more failures are not in this command)");
    }

    #[test]
    fn a_frame_too_small_for_the_event_reports_the_bytes_it_cut() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let state = Small::new(&mut arena, &mut ledger);
        let mut frame = Line::<16>::new();

        let result = state.project(&Event::Started { id: FAILING, solo: false, at_ms: 0, limit_ms: None }, &mut frame);

        assert_eq!(result, Err(b"START    cli_run  fixture::fails_when_requested\n".len() - 16));
    }

    #[test]
    fn every_mode_event_pair_lands_where_the_transition_table_says() {
        let finished = |failed| Event::Finished { passed: 1, failed, ignored: 0, not_run: 0, wall_ms: 5, manifest: None, discovered: 0, filtered: 0, exit: 0 };
        let rows = [
            (Mode::Running, Event::BuildFailed { diagnostic: b"E0308", exit: 3 }, Mode::BuildError),
            (Mode::Running, Event::Interrupted { signal: 2, groups: 0 }, Mode::Interrupted { signal: 2 }),
            (Mode::Running, finished(0), Mode::Done { wall_ms: 5 }),
            (Mode::Running, finished(1), Mode::Done { wall_ms: 5 }),
            (Mode::Running, Event::Started { id: PASSING, solo: false, at_ms: 0, limit_ms: None }, Mode::Running),
            (Mode::Done { wall_ms: 5 }, Event::Interrupted { signal: 2, groups: 0 }, Mode::Done { wall_ms: 5 }),
            (Mode::BuildError, finished(0), Mode::BuildError),
            (Mode::Interrupted { signal: 15 }, Event::Started { id: PASSING, solo: false, at_ms: 0, limit_ms: None }, Mode::Interrupted { signal: 15 }),
        ];

        for (from, event, to) in rows {
            assert_eq!(from.next(&event, 0, 0), to, "{from:?} on {event:?}");
        }
    }

    #[test]
    fn no_projection_contains_an_escape_byte() {
        let mut arena = [0_u8; 512];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        let events = [
            Event::Begin { version: b"0.1.0", shard: true, jobs: 8, github: true, why: None, json: false, at_ms: 0 },
            Event::Listed { binaries: 19, of: Some(19), tests: 383 },
            Event::Started { id: FAILING, solo: true, at_ms: 0, limit_ms: None },
            Event::Warned { id: FAILING, what: b"rerun failed to start" },
            Event::BlockOpened { id: FAILING },
            Event::BlockClosed,
            Event::Interrupted { signal: 2, groups: 0 },
        ];

        for event in events {
            state.apply(&event);
            assert!(!projected(&state, &event).as_bytes().contains(&0x1b));
        }
    }

    #[test]
    fn a_hundred_thousand_events_update_and_project_with_zero_allocations() {
        let mut arena = [0_u8; 4096];
        let mut ledger = [0_u32; 64];
        let mut inflight = [0_u8; 4096];
        let mut state = State::<8192>::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_heartbeat_ms(1);
        let mut frame = Line::<32_768>::new();
        let mut events_applied = 0_usize;
        let mut heartbeats = 0_usize;

        let ((), allocations) = measure_allocations(|| {
            for round in 0..20_000_u64 {
                let outcome = if round % 7 == 0 { Outcome::Failed } else { Outcome::Passed };
                for event in [
                    Event::Started { id: FAILING, solo: false, at_ms: round, limit_ms: Some(120_000) },
                    Event::Tick { now_ms: round + 3 },
                    Event::Settled { id: FAILING, outcome, at_ms: Some(round + 5), solo: false },
                    Event::Captured { id: FAILING, stdout: (b"assertion failed: left == right", 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD },
                    Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: round, manifest: Some(b"/w/Cargo.toml"), discovered: 0, filtered: 0, exit: 0 },
                ] {
                    frame.clear();
                    state.apply(&event);
                    state.project(&event, &mut frame).expect("the frame holds one event");
                    heartbeats += usize::from(matches!(event, Event::Tick { .. }) && frame.bytes().starts_with(b"RUNNING"));
                    events_applied += 1;
                }
            }
        });

        assert_eq!(events_applied, 100_000, "the loop ran every event, so a zero is not an empty loop");
        assert_eq!(heartbeats, 20_000, "every tick found the test in flight, so the heartbeat path was measured too");
        assert_eq!(allocations, 0);
        assert!(state.unstored() > 0, "the ledger overflowed during the loop, so the overflow path was measured too");
    }

    fn failing_run<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32]) -> State<'storage, 8192> {
        let mut state = State::<8192>::new(arena, ledger);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        state.apply(&Event::Captured { id: FAILING, stdout: (REPORT, 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD });
        state
    }

    #[test]
    fn f_opens_review_and_escape_returns_to_the_running_screen_mid_run() {
        let (mut arena, mut ledger) = ([0_u8; 4096], [0_u32; 8]);
        let mut state = failing_run(&mut arena, &mut ledger);

        state.apply(&Event::Key(crate::render::review::Key::Char(b'f')));
        let opened = state.mode();
        state.apply(&Event::Key(crate::render::review::Key::Esc));

        assert_eq!(opened, Mode::Review { selected: 0, armed: false, top: None });
        assert_eq!(state.mode(), Mode::Running);
    }

    #[test]
    fn escape_in_the_review_of_a_finished_run_stays_in_review() {
        let (mut arena, mut ledger) = ([0_u8; 4096], [0_u32; 8]);
        let mut state = failing_run(&mut arena, &mut ledger);
        state.apply(&Event::Key(crate::render::review::Key::Char(b'f')));
        state.hold_review();

        state.apply(&Event::Key(crate::render::review::Key::Esc));

        assert_eq!(state.mode(), Mode::Review { selected: 0, armed: false, top: None });
    }

    #[test]
    fn f_without_a_failure_stays_on_the_running_screen_and_s_opens_the_timeline_mid_run() {
        let (mut arena, mut ledger) = ([0_u8; 4096], [0_u32; 8]);
        let mut state = State::<8192>::new(&mut arena, &mut ledger);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });

        state.apply(&Event::Key(crate::render::review::Key::Char(b'f')));
        let after_f = state.mode();
        state.apply(&Event::Key(crate::render::review::Key::Char(b's')));
        let after_s = state.mode();
        state.apply(&Event::Key(crate::render::review::Key::Char(b's')));

        assert_eq!((after_f, after_s, state.mode()), (Mode::Running, Mode::Timeline { back: None }, Mode::Running));
    }

    const REPORT: &[u8] = b"thread 'fixture::fails_when_requested' panicked at src/lib.rs:12:13:\nrequested fixture failure\n";
    const VIEWS: [(u32, u64, u64, u64); 2] = [(0, 8, 8_192, 1_048_576), (1, 0, 0, 0)];

    fn reviewing<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32], window: &'storage mut [u8]) -> State<'storage, 8192> {
        let mut state = State::<8192>::new(arena, ledger).with_window(window);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        state.apply(&Event::Captured { id: FAILING, stdout: (REPORT, 1_048_576 - REPORT.len() as u64), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD });
        state.keep_views(FAILING, VIEWS);
        state.apply(&Event::Key(crate::render::review::Key::Char(b'f')));
        state.apply(&Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: 9, manifest: None, discovered: 2, filtered: 0, exit: 1 });
        state
    }

    #[test]
    fn the_review_loads_the_window_around_the_panic_and_the_record_outlives_the_window() {
        let (mut arena, mut ledger, mut window) = ([0_u8; 4096], [0_u32; 8], [0_u8; 512]);
        let mut state = reviewing(&mut arena, &mut ledger, &mut window);
        let before = state.detail(FAILING).4.to_vec();

        let mut asked = None;
        state.fill_window(|view, buffer| {
            asked = Some(view);
            let loaded: &[u8] = b"line before the panic\nthread 'fixture::fails_when_requested' panicked at src/lib.rs:12:13:\nrequested fixture failure\nline after the panic\n";
            buffer[..loaded.len()].copy_from_slice(loaded);
            loaded.len()
        });
        let after = state.detail(FAILING);

        assert_eq!(before, b"", "nothing is loaded until the review asks for the window");
        assert_eq!(asked, Some(VIEWS[0]), "the review asks for the chain of the stream whose output it shows");
        assert!(after.4.starts_with(b"line before the panic\n") && after.4.ends_with(b"line after the panic\n"), "{:?}", String::from_utf8_lossy(after.4));
        assert_eq!((after.0, after.1.map(|site| (site.file, site.line, site.col)), after.2.0), (crate::failure_class::Class::Panic, Some((&b"src/lib.rs"[..], 12, 13)), &b"requested fixture failure"[..]), "the kept record, not the window, classifies the failure");
    }

    #[test]
    fn a_site_in_stdout_is_kept_when_stderr_is_a_storm_and_the_review_shows_stdout() {
        let (mut arena, mut ledger, mut window) = ([0_u8; 4096], [0_u32; 8], [0_u8; 512]);
        let storm = [b'e'; 2048];
        let mut state = State::<8192>::new(&mut arena, &mut ledger).with_window(&mut window);
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        state.apply(&Event::Captured { id: FAILING, stdout: (REPORT, 0), stderr: (&storm, 1_048_576 - 2048), exit: Exit::Code(101), excerpt: UNREAD });
        state.keep_views(FAILING, VIEWS);
        state.apply(&Event::Key(crate::render::review::Key::Char(b'f')));

        let mut asked = None;
        state.fill_window(|view, _| {
            asked = Some(view);
            0
        });
        let detail = state.detail(FAILING);

        assert_eq!(asked, Some(VIEWS[0]), "the review loads the stream the site came from, not the storm");
        assert_eq!(detail.1.map(|site| (site.file, site.line)), Some((&b"src/lib.rs"[..], 12)));
        assert_eq!(detail.2.0, b"requested fixture failure");
    }

    #[test]
    fn a_selection_loads_its_window_once_and_a_new_capture_asks_again() {
        let (mut arena, mut ledger, mut window) = ([0_u8; 4096], [0_u32; 8], [0_u8; 64]);
        let mut state = reviewing(&mut arena, &mut ledger, &mut window);
        let mut loads = 0;

        for _ in 0..3 {
            state.fill_window(|_, _| {
                loads += 1;
                0
            });
        }
        state.keep_views(FAILING, VIEWS);
        state.fill_window(|_, _| {
            loads += 1;
            0
        });

        assert_eq!(loads, 2, "three fills of one selection load once; fresh views load again");
    }

    #[test]
    fn ten_thousand_review_windows_and_plain_pages_load_and_project_with_zero_allocations() {
        let (mut arena, mut ledger, mut window) = ([0_u8; 4096], [0_u32; 8], [0_u8; 4096]);
        let mut state = reviewing(&mut arena, &mut ledger, &mut window);
        let mut frame = Line::<32_768>::new();
        let page = [b'x'; 16_384];
        let links: (&[u8], &[u8], Option<crate::failure_class::Location<'_>>) = (b"", b"", None);
        let mut pages = 0_usize;

        let ((), allocations) = measure_allocations(|| {
            for _ in 0..10_000 {
                state.keep_views(FAILING, VIEWS);
                state.fill_window(|_, buffer| {
                    buffer[..REPORT.len()].copy_from_slice(REPORT);
                    REPORT.len()
                });
                frame.clear();
                super::stream_page(&mut frame, &page, links);
                pages += 1;
            }
        });

        assert_eq!(pages, 10_000, "every page was projected, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
        assert_eq!(state.detail(FAILING).4, REPORT, "the last load is the window the review shows");
    }

    #[test]
    fn ten_thousand_agent_failure_blocks_and_closing_frames_project_with_zero_allocations() {
        let mut arena = [0_u8; 4096];
        let mut ledger = [0_u32; 64];
        let mut state = State::<8192>::new(&mut arena, &mut ledger);
        let mut frame = Line::<32_768>::new();
        let capture = b"\nthread 't' panicked at src/lib.rs:12:13:\nrequested \x1b[31mfixture\x1b[0m failure\n";
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: Some(b"flag"), json: false, at_ms: 0 });
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        let block = Event::Captured { id: FAILING, stdout: (capture, 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD };
        let finished = Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: 9, manifest: None, discovered: 2, filtered: 0, exit: 1 };
        let mut projections = 0_usize;

        let ((), allocations) = measure_allocations(|| {
            for _ in 0..10_000 {
                for event in [block, finished] {
                    frame.clear();
                    state.project(&event, &mut frame).expect("the frame holds the agent projection");
                    projections += 1;
                }
            }
        });

        assert_eq!(projections, 20_000, "the loop ran every projection, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
        assert!(!frame.bytes().contains(&0x1b), "the last projection holds no escape byte");
        assert!(frame.bytes().ends_with(b"EXIT     1  failed\n"));
    }

    #[test]
    fn ten_thousand_json_records_project_with_zero_allocations() {
        let mut arena = [0_u8; 4096];
        let mut ledger = [0_u32; 64];
        let mut inflight = [0_u8; 512];
        let mut state = State::<8192>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        let mut frame = Line::<32_768>::new();
        let capture = b"\nthread 't' panicked at src/lib.rs:12:13:\npath \"C:\\tmp\" \x1b[31mis\x1b[0m \xff missing\nsecond line\n";
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: Some(b"flag"), json: true, at_ms: 0 });
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        let events = [
            Event::Started { id: FAILING, solo: false, at_ms: 0, limit_ms: None },
            Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: Some(7), solo: false },
            Event::Captured { id: FAILING, stdout: (capture, 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD },
            Event::Finished { passed: 1, failed: 1, ignored: 0, not_run: 0, wall_ms: 9, manifest: None, discovered: 2, filtered: 0, exit: 1 },
        ];
        let mut records = 0_usize;

        let ((), allocations) = measure_allocations(|| {
            for _ in 0..2_500 {
                for event in events {
                    frame.clear();
                    state.apply(&event);
                    state.project(&event, &mut frame).expect("the frame holds the json projection");
                    records += memchr::memchr_iter(b'\n', frame.bytes()).count();
                }
            }
        });

        assert!(records >= 10_000, "the loop projected {records} records, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
        assert!(!frame.bytes().contains(&0x1b), "the last frame holds no escape byte");
        assert!(frame.bytes().ends_with(b"{\"v\":1,\"t\":\"exit\",\"code\":1,\"outcome\":\"failed\"}\n"));
    }

    #[test]
    fn json_escapes_quotes_backslashes_controls_and_replaces_invalid_utf8() {
        let mut arena = [0_u8; 512];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        let mut frame = Line::<4096>::new();
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 1, github: false, why: Some(b"flag"), json: true, at_ms: 0 });
        let id = (&b"b"[..], &b"say \"hi\"\\\xff\x01\xc3\xa9"[..]);

        state.project(&Event::Started { id, solo: false, at_ms: 40, limit_ms: Some(120_000) }, &mut frame).expect("fits");

        let text = core::str::from_utf8(frame.bytes()).expect("the projection is valid UTF-8");
        assert!(text.contains(r#""test":"say \"hi\"\\"#), "{text}");
        assert!(text.contains("\\\\\u{FFFD}\\u0001\u{e9}\""), "{text}");
    }

    #[test]
    fn ten_thousand_closing_frames_project_with_zero_allocations() {
        let mut arena = [0_u8; 4096];
        let mut ledger = [0_u32; 64];
        let mut state = State::<8192>::new(&mut arena, &mut ledger);
        let mut frame = Line::<32_768>::new();
        let mut frames = 0_usize;
        for ordinal in 0..64_u64 {
            let id = (&b"sched_records"[..], &b"the_host_binary_format_yields_every_declared_record"[..]);
            state.apply(&Event::Settled { id, outcome: Outcome::Failed, at_ms: Some(ordinal), solo: false });
        }
        let finished = Event::Finished { passed: 9, failed: 64, ignored: 0, not_run: 0, wall_ms: 258_235, manifest: Some(b"/w/Cargo.toml"), discovered: 0, filtered: 0, exit: 0 };

        let ((), allocations) = measure_allocations(|| {
            for _ in 0..10_000 {
                frame.clear();
                state.project(&finished, &mut frame).expect("the frame holds the closing block");
                frames += 1;
            }
        });

        assert_eq!(frames, 10_000, "the loop ran every frame, so a zero is not an empty loop");
        assert_eq!(allocations, 0);
        assert!(frame.bytes().starts_with(b"SUMMARY  passed=9 failed=64"), "the last frame is the closing block");
    }
    const SLOW: Identity<'static> = (b"shard_process", b"a_per_test_interrupt_ends_a_blocked_child");
    const HOURS: Identity<'static> = (b"sched_records", b"the_host_binary_format_yields_every_declared_record");

    fn started(id: Identity<'static>, at_ms: u64, limit_ms: Option<u32>) -> Event<'static> {
        Event::Started { id, solo: false, at_ms, limit_ms }
    }

    #[test]
    fn a_heartbeat_names_the_oldest_in_flight_test_with_its_age_and_declared_deadline() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let mut inflight = [0_u8; 512];
        let mut state = Small::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_heartbeat_ms(10_000);
        state.apply(&started(SLOW, 1_000, Some(120_000)));
        state.apply(&started(HOURS, 4_000, None));

        let early = Event::Tick { now_ms: 5_000 };
        state.apply(&early);
        let silent = projected(&state, &early);
        let due = Event::Tick { now_ms: 11_795 };
        state.apply(&due);
        let beat = projected(&state, &due);

        assert_eq!(silent, "", "four seconds after the start is inside the heartbeat interval");
        assert_eq!(
            beat,
            "RUNNING  shard_process  a_per_test_interrupt_ends_a_blocked_child  10.795s  limit=120.000s  (oldest of 2 running)\n"
        );
        assert_eq!(state.inflight_len(), 2);
    }

    #[test]
    fn heartbeats_repeat_each_interval_and_move_to_the_next_oldest_when_the_oldest_settles() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let mut inflight = [0_u8; 512];
        let mut state = Small::new(&mut arena, &mut ledger).with_inflight(&mut inflight).with_heartbeat_ms(10_000);
        state.apply(&started(SLOW, 0, None));
        state.apply(&started(HOURS, 2_000, None));
        state.apply(&Event::Tick { now_ms: 10_000 });
        state.apply(&Event::Settled { id: SLOW, outcome: Outcome::Passed, at_ms: Some(12_000), solo: false });

        let between = Event::Tick { now_ms: 15_000 };
        state.apply(&between);
        let quiet = projected(&state, &between);
        let next = Event::Tick { now_ms: 20_000 };
        state.apply(&next);
        let beat = projected(&state, &next);

        assert_eq!(quiet, "");
        assert_eq!(beat, "RUNNING  sched_records  the_host_binary_format_yields_every_declared_record  18.000s  (oldest of 1 running)\n");
    }

    #[test]
    fn a_settle_projects_the_duration_measured_from_its_recorded_start() {
        let mut arena = [0_u8; 128];
        let mut ledger = [0_u32; 2];
        let mut inflight = [0_u8; 512];
        let mut state = Small::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&started(SLOW, 7_000, Some(30_000)));
        state.apply(&started(HOURS, 7_500, None));

        let pass = Event::Settled { id: HOURS, outcome: Outcome::Passed, at_ms: Some(10_623), solo: false };
        state.apply(&pass);
        let pass_line = projected(&state, &pass);
        let fail = Event::Settled { id: SLOW, outcome: Outcome::Failed, at_ms: Some(37_001), solo: false };
        state.apply(&fail);

        assert_eq!(pass_line, "PASS     sched_records  the_host_binary_format_yields_every_declared_record  3.123s\n");
        assert_eq!(
            state.failure_records().collect::<Vec<_>>(),
            [(SLOW, Some(30_001))],
            "the failure record keeps the duration, so the closing block and the review read one number"
        );
        assert_eq!(state.inflight_len(), 0);
    }

    #[test]
    fn a_settle_with_no_recorded_start_or_no_end_stamp_has_an_unknown_duration_not_zero() {
        let mut arena = [0_u8; 128];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        state.apply(&started(SLOW, 100, None));

        let overflowed = Event::Settled { id: SLOW, outcome: Outcome::Failed, at_ms: Some(900), solo: false };
        state.apply(&overflowed);
        let ignored = Event::Settled { id: HOURS, outcome: Outcome::Ignored, at_ms: None, solo: false };
        state.apply(&ignored);

        assert_eq!(state.untracked(), 1, "no in-flight storage, so the start is counted instead of stored");
        assert_eq!(projected(&state, &overflowed), "FAIL     shard_process  a_per_test_interrupt_ends_a_blocked_child\n");
        assert_eq!(projected(&state, &ignored), "IGNORED  sched_records  the_host_binary_format_yields_every_declared_record\n");
        assert_eq!(state.failure_records().collect::<Vec<_>>(), [(SLOW, None)]);
    }

    #[test]
    fn a_restarted_test_replaces_its_row_and_a_full_arena_counts_the_rest() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let mut inflight = [0_u8; 100];
        let mut state = Small::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&started(SLOW, 1_000, None));
        state.apply(&started(SLOW, 9_000, Some(60_000)));
        state.apply(&started(HOURS, 9_500, None));

        let oldest = state.oldest().expect("the restarted test is in flight");

        assert_eq!(state.inflight_len(), 1, "the second start of one id is one row; the third row does not fit 100 bytes");
        assert_eq!(state.untracked(), 1);
        assert_eq!(oldest, (SLOW, 9_000, Some(60_000)));
    }

    #[test]
    fn finishing_the_run_empties_the_in_flight_set() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let mut inflight = [0_u8; 512];
        let mut state = Small::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
        state.apply(&started(SLOW, 0, None));

        state.apply(&Event::Finished { passed: 0, failed: 0, ignored: 0, not_run: 1, wall_ms: 5, manifest: None, discovered: 0, filtered: 0, exit: 0 });

        assert_eq!(state.inflight_len(), 0);
        assert_eq!(state.oldest(), None);
    }

    #[test]
    fn an_interrupted_run_projects_the_block_the_spec_prints_then_a_summary_that_accounts_for_every_discovered_name() {
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 2];
        let mut state = Small::new(&mut arena, &mut ledger);
        let mut frame = Line::<512>::new();
        let finished = Event::Finished { passed: 381, failed: 0, ignored: 0, not_run: 2, wall_ms: 75_000, manifest: None, discovered: 383, filtered: 0, exit: 130 };
        let events = [
            Event::Interrupted { signal: 2, groups: 2 },
            Event::Running { id: (b"shard_process", b"a_per_test_interrupt"), group: 48213, age_ms: 56_825 },
            Event::Running { id: (b"sched_records", b"the_host_binary_format"), group: 48230, age_ms: 40_904 },
            finished,
        ];

        for event in &events {
            state.apply(event);
            state.project(event, &mut frame).expect("fits");
        }

        assert_eq!(
            core::str::from_utf8(frame.bytes()).expect("utf-8"),
            "INTERRUPTED  signal=2  forwarded_groups=2\n\
             RUNNING  shard_process  a_per_test_interrupt  56.825s  killed group=48213\n\
             RUNNING  sched_records  the_host_binary_format  40.904s  killed group=48230\n\
             SUMMARY  passed=381 failed=0 ignored=0 not_run=2  wall=75.000s discovered=383 run=381 filtered=0\n"
        );
    }

    #[test]
    fn the_panic_site_in_stdout_survives_a_stderr_storm_that_fills_the_capture_so_e_has_a_location() {
        let (mut arena, mut ledger) = ([0_u8; 16384], [0_u32; 8]);
        let mut state = Small::new(&mut arena, &mut ledger);
        let storm: Vec<u8> = (0..100).flat_map(|line| format!("storm_01 diagnostic line {line:03}: connection to 10.0.0.{line} reset by peer after 3 retries\n").into_bytes()).collect();
        let panic = b"\nthread 'tests::storm_01' panicked at src/lib.rs:31:9:\nstorm_01 failed after its diagnostics\n";
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });

        state.apply(&Event::Captured { id: FAILING, stdout: (panic, 0), stderr: (&storm[..4096], 5000), exit: Exit::Code(101), excerpt: UNREAD });

        let located = state.detail(FAILING).1.expect("the site is kept with the stream that holds it");
        assert_eq!((located.file, located.line, located.col), (&b"src/lib.rs"[..], 31, 9));
    }

    #[test]
    fn a_rerun_capture_replaces_the_kept_output_of_a_middle_failure_and_leaves_the_failures_around_it_intact() {
        let (mut arena, mut ledger) = ([0_u8; 2048], [0_u32; 8]);
        let mut state = Small::new(&mut arena, &mut ledger);
        let second: Identity<'static> = (b"cli_run", b"fixture::second");
        let third: Identity<'static> = (b"cli_run", b"fixture::third");
        let report = |message: &str| std::format!("thread 't' panicked at src/lib.rs:1:1:\n{message}\n");
        let captures = [(FAILING, report("first message")), (second, report("second message")), (third, report("third message"))];
        state.apply(&Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false, why: None, json: false, at_ms: 0 });
        for (id, bytes) in captures {
            state.apply(&Event::Settled { id, outcome: Outcome::Failed, at_ms: None, solo: false });
            state.apply(&Event::Captured { id, stdout: (bytes.as_bytes(), 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD });
        }
        let longer = report("a much longer second message from the rerun");
        let later = Event::Captured { id: second, stdout: (longer.as_bytes(), 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD };

        state.apply(&later);

        assert_eq!(state.detail(second).2.0, b"a much longer second message from the rerun");
        assert_eq!(state.detail(FAILING).2.0, b"first message");
        assert_eq!(state.detail(third).2.0, b"third message");
        let brief = report("short");
        let shorter = Event::Captured { id: second, stdout: (brief.as_bytes(), 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD };

        state.apply(&shorter);

        assert_eq!(state.detail(second).2.0, b"short");
        assert_eq!(state.detail(third).2.0, b"third message");
        assert_eq!(state.detail(FAILING).2.0, b"first message");
    }

    const FIRST_VIEWS: [(u32, u64, u64, u64); 2] = [(4, 100, 900, 4_194_304), (5, 200, 800, 4_194_304)];
    const RERUN_VIEWS: [(u32, u64, u64, u64); 2] = [(9, 300, 700, 5), (10, 400, 600, 5)];

    fn capture_with_reason(state: &mut State<'_, 8192>, id: Identity<'static>, reason: &'static [u8]) {
        let report = std::format!("thread 't' panicked at src/lib.rs:1:1:\n{}\n", String::from_utf8_lossy(reason));
        state.apply(&Event::Captured { id, stdout: (report.as_bytes(), 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: Err(reason) });
    }

    #[test]
    fn a_rerun_that_changes_the_excerpt_while_the_first_block_is_pending_leaves_the_first_attempt_whole() {
        let (mut arena, mut ledger) = ([0_u8; 4096], [0_u32; 8]);
        let mut state = State::<8192>::new(&mut arena, &mut ledger);
        state.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: None, solo: false });
        capture_with_reason(&mut state, FAILING, b"src/lib.rs is unreadable: first attempt");
        state.keep_views(FAILING, FIRST_VIEWS);
        assert_eq!(state.queue_block(FAILING, 41), Some(0), "the first attempt is ledger entry 0");

        capture_with_reason(&mut state, FAILING, b"src/lib.rs moved: the rerun");
        state.keep_views(FAILING, RERUN_VIEWS);

        assert_eq!(state.pending_block(0), Some((0, 41)), "the pending block is still the first attempt, at its own position");
        assert_eq!(state.block_excerpt(0), Err(b"src/lib.rs is unreadable: first attempt".as_slice()), "its excerpt is the first attempt's");
        assert_eq!(state.block_views(0), FIRST_VIEWS, "its chains are the first attempt's");
        assert_eq!(state.block_excerpt(1), Err(b"src/lib.rs moved: the rerun".as_slice()), "the rerun is a new ledger entry with its own excerpt");
        assert_eq!(state.block_views(1), RERUN_VIEWS);
    }

    #[test]
    fn forty_failures_queue_forty_blocks_in_settle_order_with_no_fixed_cap() {
        let names: Vec<String> = (0..40).map(|index| std::format!("fixture::bulk_{index:02}")).collect();
        let (mut arena, mut ledger) = (std::vec![0_u8; 64 * 1024], [0_u32; 64]);
        let mut state = State::<8192>::new(&mut arena, &mut ledger);
        for (position, name) in names.iter().enumerate() {
            let id: Identity<'_> = (b"views_pair", name.as_bytes());
            state.apply(&Event::Settled { id, outcome: Outcome::Failed, at_ms: None, solo: false });
            state.apply(&Event::Captured { id, stdout: (REPORT, 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD });
            state.keep_views(id, FIRST_VIEWS);
            assert_eq!(state.queue_block(id, position as u64 * 10), Some(position), "failure {position} queues");
        }

        let mut printed = Vec::new();
        let mut head = 0;
        while let Some((index, position)) = state.pending_block(head) {
            printed.push((index, position));
            state.finish_block(index);
            head = index + 1;
        }

        assert_eq!(printed, (0..40).map(|index| (index, index as u64 * 10)).collect::<Vec<_>>(), "every block is pending, in settle order, at its own position");
        assert_eq!(state.pending_block(0), None, "all forty printed");
    }
}
