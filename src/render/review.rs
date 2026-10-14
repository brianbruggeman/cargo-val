//! The review screen: decoded keys in, a mode and an action out; the state's failure ledger in, a frame out.
//!
//! Sans-IO, like [`crate::render`]: no terminal, no clock, no allocation. Keys are data ([`Key`]); each is a transition of
//! [`Mode`] (`impl Mode` below, one exhaustive match per mode) and, separately, an [`Action`] the driver executes later
//! (rerun these, open the editor, write the receipt, interrupt). The list is not a second copy of what failed: it is the
//! [`State`] ledger filtered to the rows still listed, so a rerun that passed is an `Event::Reran` and the list, its length
//! and the cursor bounds follow from the ledger. The class, location and output of a failure are retained by the [`State`]
//! (`State::retain`, read back by `State::detail`); [`State::screen`] and [`State::action`] take a lookup from an id to
//! them, so nothing here copies an output byte except into the frame.
//!
//! The quit receipt is not drawn here: after `q q` the mode is [`Mode::Receipt`] and the driver projects
//! `Event::Finished` through `State::project`, so the bytes are the plain closing block by construction.

use super::{Identity, Mode, State, excerpt, seconds};
use crate::failure_class::{Class, Enforcer, Location};
use memchr::memmem;
use crate::hyperlink::write_link;
use crate::line::Line;

const MIN_WIDTH: usize = 30;
const LOCATION_BYTES: usize = 512;
const LIST_ROWS: usize = 3;
const OUTPUT_LINES: usize = 6;
const CLASS_FIELD: usize = 8;
const SECONDS_FIELD: usize = 10;
const MINI_BAR: usize = 6;
const INTERRUPT_SIGNAL: u8 = 2;
const ESCAPE: u8 = 0x1b;
const RESET: &[u8] = b"\x1b[0m";
const BOLD: &[u8] = b"1";
const NAME_INDENT: usize = 2;
const FAINT: &[u8] = b"2";
const RED_BOLD: &[u8] = b"1;31";
const GREEN_BOLD: &[u8] = b"1;32";
const YELLOW_BOLD: &[u8] = b"1;33";
const YELLOW: &[u8] = b"33";
const GREEN: &[u8] = b"32";
const CYAN: &[u8] = b"36";
const CYAN_BOLD: &[u8] = b"1;36";
const KEYS: [(&[u8], &[u8]); 9] = [
    (b"j/k", b"move"),
    (b"r", b"rerun"),
    (b"R", b"all"),
    (b"e", b"edit"),
    (b"/", b"find"),
    (b"i", b"skip"),
    (b"w", b"warn"),
    (b"s", b"timeline"),
    (b"q", b"quit"),
];
const EMPTY_KEYS: [(&[u8], &[u8]); 5] = [(b"/", b"find"), (b"s", b"timeline"), (b"i", b"skip"), (b"w", b"warn"), (b"q", b"quit")];
const SCROLL_KEYS: [(&[u8], &[u8]); 5] = [(b"j/k", b"scroll"), (b"enter", b"list"), (b"r", b"rerun"), (b"e", b"edit"), (b"q", b"quit")];
const STUB_KEYS: [(&[u8], &[u8]); 2] = [(b"esc", b"back"), (b"q", b"quit")];

/// Width in columns, and whether the frame may carry colour.
type Look = (usize, bool);

/// One decoded key: the edge turns bytes into this, the transition table reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(u8),
    Enter,
    Esc,
    Up,
    Down,
    CtrlC,
}


/// What the driver should do for a key. The renderer never executes any of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action<'a> {
    None,
    RerunOne { id: Identity<'a> },
    RerunFailed,
    OpenEditor { file: &'a [u8], line: u32 },
    /// Write the closing block to scrollback; cancel the run first if it is still live.
    Quit,
    Interrupt,
}

/// What retrying a failure alone concluded, or why it was not tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Real,
    Flaky,
    OrderDependent,
    NotRetried { budget_secs: u32 },
}

/// The retry-alone state of one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rerun {
    Idle,
    Running { elapsed_ms: u64, expected_ms: Option<u64> },
    Passed { millis: u64 },
    /// The rerun ended failing again; the row keeps its class and shows how long this attempt took.
    Failed { millis: u64 },
}

/// What the state holds for one failure: its class, its source location, the classifier's reason, the source-excerpt rows
/// (empty when there are none) and the child's bytes, shown verbatim.
pub type Detail<'a> = (Class, Option<Location<'a>>, Reason<'a>, &'a [u8], &'a [u8]);

/// The classifier's message and, for an assertion, its left and right values; both borrow from the retained capture.
pub type Reason<'a> = (&'a [u8], Option<(&'a [u8], &'a [u8])>);

type Shown<'a> = (Identity<'a>, u64, Option<Verdict>, Rerun, Detail<'a>, u8);

pub(super) fn clamp(selected: u16, open: usize) -> u16 {
    let last = u16::try_from(open.saturating_sub(1)).unwrap_or(u16::MAX);
    selected.min(last)
}

/// The lines of one captured stream as the review detail counts them: none for an empty stream.
#[must_use]
pub fn output_lines(bytes: &[u8]) -> usize {
    let body = bytes.trim_ascii_end();
    if body.is_empty() { 0 } else { memchr::memchr_iter(b'\n', body).count() + 1 }
}

const fn interrupted() -> Mode {
    Mode::Interrupted { signal: INTERRUPT_SIGNAL }
}

const fn opening(byte: u8, back: Option<u16>) -> Option<Mode> {
    match byte {
        b's' => Some(Mode::Timeline { back }),
        b'i' => Some(Mode::Skipped { back }),
        b'w' => Some(Mode::Warnings { back }),
        b'/' => Some(Mode::Find { back }),
        _ => None,
    }
}

impl Key {
    /// Turns the bytes one terminal read returned into keys, in order, handing each to `sink`; the first error it
    /// returns stops the decode and is returned.
    ///
    /// `carry` is the decoder's only memory: `0` between keys, `0x1b` after an escape, `[` or `O` after `0x1b [` or
    /// `0x1b O`. The caller keeps it in the [`State`] across reads, so `0x1b` in one read and `[A` in the next is one
    /// `Up`. A lone `0x1b` is `Esc` once the next byte is not `[` or `O`, or when [`Key::flush`] runs on a tick; no timer
    /// thread is involved. `Enter` is `\r` or `\n`, `Ctrl-C` is `0x03`, printable ASCII is itself, the arrows end `A` or
    /// `B`, and every other escape sequence, control byte or UTF-8 byte is dropped, whichever read it arrives in.
    ///
    /// # Errors
    ///
    /// Returns the first error `sink` returns.
    pub fn decode<E>(carry: &mut u8, bytes: &[u8], mut sink: impl FnMut(Self) -> Result<(), E>) -> Result<(), E> {
        bytes.iter().try_for_each(|byte| Self::step(carry, *byte, &mut sink))
    }

    /// Resolves a held lone `0x1b` as `Esc`; a held `0x1b [` or `0x1b O` is an unfinished sequence and is dropped.
    ///
    /// # Errors
    ///
    /// Returns the error `sink` returns.
    pub fn flush<E>(carry: &mut u8, mut sink: impl FnMut(Self) -> Result<(), E>) -> Result<(), E> {
        let held = std::mem::take(carry);
        if held == ESCAPE { sink(Self::Esc) } else { Ok(()) }
    }

    fn step<E>(carry: &mut u8, byte: u8, sink: &mut impl FnMut(Self) -> Result<(), E>) -> Result<(), E> {
        match (*carry, byte) {
            (ESCAPE, b'[' | b'O') => *carry = byte,
            (ESCAPE, _) => {
                *carry = 0;
                sink(Self::Esc)?;
                return Self::step(carry, byte, sink);
            }
            (b'[' | b'O', b'A') => {
                *carry = 0;
                return sink(Self::Up);
            }
            (b'[' | b'O', b'B') => {
                *carry = 0;
                return sink(Self::Down);
            }
            (b'[', 0x20..=0x3f) => {}
            (b'[' | b'O', ESCAPE) => *carry = ESCAPE,
            (b'[' | b'O', _) => *carry = 0,
            (_, ESCAPE) => *carry = ESCAPE,
            (_, b'\r' | b'\n') => return sink(Self::Enter),
            (_, 0x03) => return sink(Self::CtrlC),
            (_, 0x20..=0x7e) => return sink(Self::Char(byte)),
            _ => {}
        }
        Ok(())
    }
}

const fn restore(back: Option<u16>) -> Mode {
    match back {
        None => Mode::Running,
        Some(selected) => Mode::Review { selected, armed: false, top: None },
    }
}

impl Mode {
    /// The key rows of the transition table; `open` is the review list length, `lines` the selected row's output lines.
    pub(super) fn keyed(self, key: Key, open: usize, lines: usize) -> Self {
        match self {
            Self::Running => running_key(key, open),
            Self::QuitArmed => armed_key(key),
            Self::Review { selected, armed, top } => review_key(selected, armed, top, key, (open, lines)),
            Self::Timeline { back } | Self::Find { back } | Self::Skipped { back } | Self::Warnings { back } => {
                screen_key(self, back, key)
            }
            Self::BuildError | Self::Interrupted { .. } | Self::Done { .. } | Self::Receipt => self,
        }
    }

    const fn own_key(self) -> u8 {
        match self {
            Self::Timeline { .. } => b's',
            Self::Find { .. } => b'/',
            Self::Skipped { .. } => b'i',
            Self::Warnings { .. } => b'w',
            Self::Running
            | Self::QuitArmed
            | Self::Review { .. }
            | Self::BuildError
            | Self::Interrupted { .. }
            | Self::Done { .. }
            | Self::Receipt => 0,
        }
    }
}

fn running_key(key: Key, open: usize) -> Mode {
    match key {
        Key::Char(b'f') if open > 0 => Mode::Review { selected: 0, armed: false, top: None },
        Key::Char(b'q') => Mode::QuitArmed,
        Key::Char(b'c') | Key::CtrlC => interrupted(),
        Key::Char(byte) => opening(byte, None).unwrap_or(Mode::Running),
        Key::Enter | Key::Esc | Key::Up | Key::Down => Mode::Running,
    }
}

const fn armed_key(key: Key) -> Mode {
    match key {
        Key::Char(b'q') => Mode::Receipt,
        Key::Char(b'c') | Key::CtrlC => interrupted(),
        Key::Char(_) | Key::Enter | Key::Esc | Key::Up | Key::Down => Mode::Running,
    }
}

fn review_key(selected: u16, armed: bool, top: Option<u16>, key: Key, (open, lines): (usize, usize)) -> Mode {
    let stay = Mode::Review { selected, armed: false, top };
    match (key, top) {
        (Key::Enter, None) if open > 0 => Mode::Review { selected, armed: false, top: Some(0) },
        (Key::Enter, Some(_)) => Mode::Review { selected, armed: false, top: None },
        (Key::Char(b'j') | Key::Down, Some(line)) => Mode::Review { selected, armed: false, top: Some(clamp(line.saturating_add(1), lines)) },
        (Key::Char(b'k') | Key::Up, Some(line)) => Mode::Review { selected, armed: false, top: Some(line.saturating_sub(1)) },
        (Key::Char(b'j') | Key::Down, None) => Mode::Review { selected: clamp(selected.saturating_add(1), open), armed: false, top },
        (Key::Char(b'k') | Key::Up, None) => Mode::Review { selected: selected.saturating_sub(1), armed: false, top },
        (Key::Char(b'q'), _) if armed => Mode::Receipt,
        (Key::Char(b'q'), _) => Mode::Review { selected, armed: true, top },
        (Key::Char(b'c') | Key::CtrlC, _) => interrupted(),
        (Key::Char(byte), _) => opening(byte, Some(selected)).unwrap_or(stay),
        (Key::Enter | Key::Esc, _) => stay,
    }
}

fn screen_key(own: Mode, back: Option<u16>, key: Key) -> Mode {
    match key {
        Key::Esc => restore(back),
        Key::Char(b'c') | Key::CtrlC => interrupted(),
        Key::Char(byte) if byte == own.own_key() => restore(back),
        Key::Char(byte) => opening(byte, back).unwrap_or(own),
        Key::Enter | Key::Up | Key::Down => own,
    }
}

impl<const RERUN: usize> State<'_, RERUN> {
    /// What `key` asks the driver to do, given the mode it arrives in. Call it with the state before `apply`.
    #[must_use]
    pub fn action<'s, 'a: 's>(&'s self, key: Key, detail: impl Fn(Identity<'s>) -> Detail<'a>) -> Action<'s> {
        match self.mode() {
            Mode::Review { selected, armed, .. } => self.review_action(selected, armed, key, detail),
            Mode::QuitArmed => match key {
                Key::Char(b'q') => Action::Quit,
                Key::Char(b'c') | Key::CtrlC => Action::Interrupt,
                Key::Char(_) | Key::Enter | Key::Esc | Key::Up | Key::Down => Action::None,
            },
            Mode::Running | Mode::Timeline { .. } | Mode::Find { .. } | Mode::Skipped { .. } | Mode::Warnings { .. } => match key {
                Key::Char(b'c') | Key::CtrlC => Action::Interrupt,
                Key::Char(_) | Key::Enter | Key::Esc | Key::Up | Key::Down => Action::None,
            },
            Mode::BuildError | Mode::Interrupted { .. } | Mode::Done { .. } | Mode::Receipt => Action::None,
        }
    }

    fn review_action<'s, 'a: 's>(&'s self, selected: u16, armed: bool, key: Key, detail: impl Fn(Identity<'s>) -> Detail<'a>) -> Action<'s> {
        let row = self.listed(selected).map(|index| self.ledger_entry(index));
        let any_idle = (0..self.stored).any(|index| self.is_listed(index) && matches!(self.ledger_entry(index).3, Rerun::Idle | Rerun::Failed { .. }));
        match key {
            Key::Char(b'r') => row.filter(|(_, _, _, rerun)| matches!(rerun, Rerun::Idle | Rerun::Failed { .. })).map_or(Action::None, |(id, ..)| Action::RerunOne { id }),
            Key::Char(b'R') if any_idle => Action::RerunFailed,
            Key::Char(b'e') => row
                .and_then(|(id, ..)| detail(id).1)
                .map_or(Action::None, |location| Action::OpenEditor { file: location.file, line: location.line }),
            Key::Char(b'q') if armed => Action::Quit,
            Key::Char(b'c') | Key::CtrlC => Action::Interrupt,
            Key::Char(_) | Key::Enter | Key::Esc | Key::Up | Key::Down => Action::None,
        }
    }

    /// Draws the current mode into `frame` at `width` columns, reading the ledger and `detail` for each listed failure.
    /// Modes other than the review screens draw nothing.
    ///
    /// # Errors
    ///
    /// Returns the number of bytes cut when `frame` was too small.
    pub fn screen<'a, const FRAME: usize>(
        &self,
        detail: impl Fn(Identity<'_>) -> Detail<'a>,
        width: usize,
        color: bool,
        frame: &mut Line<FRAME>,
    ) -> Result<(), usize> {
        let look = (width.max(MIN_WIDTH), color);
        match self.mode() {
            Mode::Review { selected, armed, top } => self.page(frame, look, (selected, armed, top), &detail),
            Mode::Timeline { .. } => self.stub(frame, look, b"timeline"),
            Mode::Find { .. } => self.stub(frame, look, b"find"),
            Mode::Skipped { .. } => self.stub(frame, look, b"skipped"),
            Mode::Warnings { .. } => self.stub(frame, look, b"warnings"),
            Mode::Running
            | Mode::QuitArmed
            | Mode::BuildError
            | Mode::Interrupted { .. }
            | Mode::Done { .. }
            | Mode::Receipt => {}
        }
        match frame.lost() {
            0 => Ok(()),
            lost => Err(lost),
        }
    }

    fn stub<const FRAME: usize>(&self, frame: &mut Line<FRAME>, look: Look, title: &[u8]) {
        self.header(frame, look, title);
        frame.text(b"\n\n ");
        paint(frame, look.1, FAINT, title);
        paint(frame, look.1, FAINT, b" view is drawn by a later slice\n\n");
        keys_line(frame, look, &STUB_KEYS);
    }

    fn shown<'s, 'a: 's>(&'s self, index: usize, detail: &impl Fn(Identity<'_>) -> Detail<'a>) -> Shown<'s> {
        let (id, millis, verdict, rerun) = self.ledger_entry(index);
        (id, millis, verdict, rerun, detail(id), self.attempts(index))
    }

    fn page<'a, const FRAME: usize>(
        &self,
        frame: &mut Line<FRAME>,
        look: Look,
        (selected, armed, top): (u16, bool, Option<u16>),
        detail: &impl Fn(Identity<'_>) -> Detail<'a>,
    ) {
        self.header(frame, look, b"review");
        frame.text(b"\n\n");
        self.counts(frame, look);
        let open = self.open();
        if open == 0 {
            self.empty(frame, look, armed);
            return;
        }
        frame.text(b"\n");
        if open > LIST_ROWS {
            more_failures(frame, look, open);
        }
        frame.text(b"\n");
        let selected = clamp(selected, open);
        let shown = open.min(LIST_ROWS);
        let first = usize::from(selected).saturating_sub(1).min(open - shown);
        let window = || (0..self.stored).filter(|index| self.is_listed(*index)).skip(first).take(shown);
        let verdict_width = window().map(|index| self.ledger_entry(index).2.map_or(0, |verdict| verdict_word(verdict).0.len() + 1)).max().unwrap_or(0);
        for (position, index) in window().enumerate() {
            list_row(frame, look, &self.shown(index, detail), first + position == usize::from(selected), verdict_width);
        }
        rule(frame, look.0);
        if let Some(index) = self.listed(selected) {
            detail_view(frame, look, &self.shown(index, detail), (top, self.links()));
        }
        footer(frame, look, if top.is_some() { &SCROLL_KEYS } else { &KEYS }, (armed, self.notice()));
    }

    fn empty<const FRAME: usize>(&self, frame: &mut Line<FRAME>, look: Look, armed: bool) {
        frame.text(b"\n\n ");
        paint(frame, look.1, GREEN_BOLD, b"no failures left");
        frame.text(b"\n ");
        let mut text = Line::<64>::new();
        text.number(self.passed + self.cleared());
        text.text(b" passed. q then q to leave.");
        paint(frame, look.1, FAINT, text.bytes());
        frame.text(b"\n\n");
        footer(frame, look, &EMPTY_KEYS, (armed, self.notice()));
    }

    fn header<const FRAME: usize>(&self, frame: &mut Line<FRAME>, look: Look, title: &[u8]) {
        frame.text(b" ");
        paint(frame, look.1, BOLD, b"cargo val");
        frame.text(b"  ");
        paint(frame, look.1, CYAN_BOLD, title);
        let used = 1 + 9 + 2 + title.len();
        let clock_len = digits(usize::try_from(self.now_ms / 60_000).unwrap_or(usize::MAX)) + 3;
        spaces(frame, look.0.saturating_sub(1 + used + clock_len).max(1));
        clock(frame, look.1, self.now_ms);
    }

    fn cleared(&self) -> usize {
        (0..self.stored).filter(|index| matches!(self.ledger_entry(*index).3, Rerun::Passed { .. })).count()
    }

    fn counts<const FRAME: usize>(&self, frame: &mut Line<FRAME>, look: Look) {
        let flaky = (0..self.stored).filter(|index| self.ledger_entry(*index).2 == Some(Verdict::Flaky)).count();
        let cleared = self.cleared();
        let failed = (0..self.stored)
            .filter(|index| {
                let (_, _, verdict, rerun) = self.ledger_entry(*index);
                !matches!(rerun, Rerun::Passed { .. }) && verdict != Some(Verdict::Flaky)
            })
            .count();
        let tone = |count: usize, loud: &'static [u8]| if count == 0 { FAINT } else { loud };
        let items: [(usize, &[u8], &[u8]); 5] = [
            (failed, b"failed", tone(failed, RED_BOLD)),
            (flaky, b"flaky", YELLOW_BOLD),
            (self.passed + cleared, b"passed", GREEN),
            (self.skipped, b"skipped", FAINT),
            (self.warnings, b"warnings", tone(self.warnings, YELLOW)),
        ];
        let limit = look.0 - 1;
        let mut column = 0;
        frame.text(b" ");
        column += 1;
        for (count, word, sgr) in items.into_iter().filter(|(count, word, _)| *count != 0 || *word != b"flaky") {
            let len = digits(count) + 1 + word.len();
            column = separate(frame, column, 1, limit, len, 2);
            let mut text = Line::<32>::new();
            text.number(count);
            text.text(b" ");
            text.text(word);
            paint(frame, look.1, sgr, text.bytes());
            column += len;
        }
    }
}

fn separate<const FRAME: usize>(frame: &mut Line<FRAME>, column: usize, indent: usize, limit: usize, len: usize, gap: usize) -> usize {
    if column == indent {
        return column;
    }
    if column + gap + len > limit {
        frame.text(b"\n");
        spaces(frame, indent);
        return indent;
    }
    spaces(frame, gap);
    column + gap
}

fn more_failures<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, open: usize) {
    let len = digits(open) + 9;
    spaces(frame, look.0.saturating_sub(1 + len + 1));
    let mut text = Line::<32>::new();
    text.number(open);
    text.text(b" failures");
    paint(frame, look.1, FAINT, text.bytes());
}

fn paint<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, sgr: &[u8], text: &[u8]) {
    if color {
        frame.text(b"\x1b[");
        frame.text(sgr);
        frame.text(b"m");
    }
    frame.text(text);
    if color {
        frame.text(RESET);
    }
}

fn spaces<const FRAME: usize>(frame: &mut Line<FRAME>, count: usize) {
    for _ in 0..count {
        frame.text(b" ");
    }
}

fn digits(value: usize) -> usize {
    value.checked_ilog10().map_or(1, |log| usize::try_from(log).unwrap_or(0) + 1)
}

const fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

fn clock<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, millis: u64) {
    let mut text = Line::<24>::new();
    text.number(usize::try_from(millis / 60_000).unwrap_or(usize::MAX));
    text.text(b":");
    let remainder = usize::try_from(millis / 1000 % 60).unwrap_or(0);
    if remainder < 10 {
        text.text(b"0");
    }
    text.number(remainder);
    paint(frame, color, FAINT, text.bytes());
}

fn centiseconds<const FRAME: usize>(line: &mut Line<FRAME>, millis: u64) {
    line.number(usize::try_from(millis / 1000).unwrap_or(usize::MAX));
    line.text(b".");
    let hundredths = usize::try_from(millis % 1000 / 10).unwrap_or(0);
    if hundredths < 10 {
        line.text(b"0");
    }
    line.number(hundredths);
    line.text(b"s");
}

fn tenths<const FRAME: usize>(line: &mut Line<FRAME>, millis: u64) {
    line.number(usize::try_from(millis / 1000).unwrap_or(usize::MAX));
    line.text(b".");
    line.number(usize::try_from(millis % 1000 / 100).unwrap_or(0));
    line.text(b"s");
}

const fn class_word(class: Class) -> &'static [u8] {
    match class {
        Class::Assert => b"ASSERT",
        Class::Panic => b"PANIC",
        Class::Timeout { .. } => b"TIMEOUT",
        Class::Hang => b"HANG",
        Class::Crash { .. } => b"CRASH",
    }
}

fn rule<const FRAME: usize>(frame: &mut Line<FRAME>, width: usize) {
    frame.text(b" ");
    for _ in 0..width.saturating_sub(2) {
        frame.text("\u{2500}".as_bytes());
    }
    frame.text(b"\n");
}

fn list_row<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, row: &Shown<'_>, selected: bool, verdict_width: usize) {
    let (id, millis, verdict, rerun, (class, ..), _) = *row;
    if selected {
        paint(frame, look.1, CYAN_BOLD, "\u{203a}".as_bytes());
    } else {
        frame.text(b" ");
    }
    let prefix = match rerun {
        Rerun::Idle | Rerun::Failed { .. } => {
            let mut text = Line::<24>::new();
            centiseconds(&mut text, if let Rerun::Failed { millis: again } = rerun { again } else { millis });
            field(frame, look.1, RED_BOLD, class_word(class), text.bytes(), FAINT);
            frame.text(b"   ");
            CLASS_FIELD + SECONDS_FIELD + 3
        }
        Rerun::Running { elapsed_ms, expected_ms } => {
            let mut text = Line::<24>::new();
            tenths(&mut text, elapsed_ms);
            field(frame, look.1, CYAN_BOLD, b"RERUN", text.bytes(), CYAN);
            frame.text(b" ");
            mini_bar(frame, look.1, elapsed_ms, expected_ms);
            frame.text(b" ");
            CLASS_FIELD + SECONDS_FIELD + MINI_BAR + 2
        }
        Rerun::Passed { millis } => {
            let mut text = Line::<24>::new();
            centiseconds(&mut text, millis);
            field(frame, look.1, GREEN_BOLD, b"PASS", text.bytes(), FAINT);
            frame.text(b"   ");
            CLASS_FIELD + SECONDS_FIELD + 3
        }
    };
    verdict_column(frame, look.1, verdict, verdict_width);
    middle(frame, id, look.0.saturating_sub(prefix + verdict_width + 1).max(1));
    frame.text(b"\n");
}

fn verdict_column<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, verdict: Option<Verdict>, width: usize) {
    let used = verdict.map_or(0, |verdict| {
        let (word, sgr) = verdict_word(verdict);
        paint(frame, color, sgr, word);
        word.len()
    });
    spaces(frame, width - used);
}

const fn verdict_word(verdict: Verdict) -> (&'static [u8], &'static [u8]) {
    match verdict {
        Verdict::Real => (b"REAL", RED_BOLD),
        Verdict::Flaky => (b"FLAKY", YELLOW_BOLD),
        Verdict::OrderDependent => (b"ORDER-DEPENDENT", YELLOW_BOLD),
        Verdict::NotRetried { .. } => (b"NO-RETRY", YELLOW_BOLD),
    }
}

fn field<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, sgr: &[u8], word: &[u8], time: &[u8], time_sgr: &[u8]) {
    paint(frame, color, sgr, word);
    spaces(frame, CLASS_FIELD - 1 - word.len());
    spaces(frame, SECONDS_FIELD - time.len());
    paint(frame, color, time_sgr, time);
}

fn mini_bar<const FRAME: usize>(frame: &mut Line<FRAME>, color: bool, elapsed_ms: u64, expected_ms: Option<u64>) {
    let leading = match expected_ms {
        Some(expected) if expected > 0 => usize::try_from(elapsed_ms.saturating_mul(MINI_BAR as u64).div_ceil(expected)).unwrap_or(MINI_BAR).min(MINI_BAR),
        Some(_) | None => 0,
    };
    let sweep = usize::try_from(elapsed_ms / 250 % (MINI_BAR as u64 - 1)).unwrap_or(0);
    for cell in 0..MINI_BAR {
        let filled = if expected_ms.is_some() { cell < leading } else { cell == sweep || cell == sweep + 1 };
        match (filled, color) {
            (true, true) => paint(frame, true, CYAN, "\u{2588}".as_bytes()),
            (false, true) => paint(frame, true, FAINT, "\u{2588}".as_bytes()),
            (true, false) => frame.text("\u{2592}".as_bytes()),
            (false, false) => frame.text("\u{2591}".as_bytes()),
        }
    }
}

fn byte_at(id: Identity<'_>, index: usize) -> u8 {
    let (binary, name) = id;
    if index < binary.len() {
        binary[index]
    } else if index < binary.len() + 2 {
        b':'
    } else {
        name[index - binary.len() - 2]
    }
}

fn joined_len(id: Identity<'_>) -> usize {
    id.0.len() + 2 + id.1.len()
}

fn joined_range<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, from: usize, to: usize) {
    let (binary, name) = id;
    for (bytes, base) in [(binary, 0), (&b"::"[..], binary.len()), (name, binary.len() + 2)] {
        let start = from.saturating_sub(base).min(bytes.len());
        let end = to.saturating_sub(base).min(bytes.len());
        if end > start {
            frame.text(&bytes[start..end]);
        }
    }
}

fn middle<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, room: usize) {
    let total = joined_len(id);
    if total <= room {
        joined_range(frame, id, 0, total);
        return;
    }
    let keep = room.saturating_sub(1);
    let mut head = keep - keep / 2;
    let mut tail_start = total - keep / 2;
    while head > 0 && head < total && is_continuation(byte_at(id, head)) {
        head -= 1;
    }
    while tail_start < total && is_continuation(byte_at(id, tail_start)) {
        tail_start += 1;
    }
    joined_range(frame, id, 0, head);
    frame.text("\u{2026}".as_bytes());
    joined_range(frame, id, tail_start, total);
}

fn detail_view<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, row: &Shown<'_>, (top, links): (Option<u16>, (&[u8], &[u8]))) {
    let (id, millis, verdict, _, (class, location, reason, rows, output), attempts) = *row;
    wrapped_name(frame, look, (id, attempts));
    class_line(frame, look, (class, millis, location), links);
    if let Some(verdict) = verdict {
        verdict_line(frame, look, verdict, millis);
    }
    reason_block(frame, look, reason);
    excerpt_block(frame, look, rows);
    output_block(frame, look, output, top);
    frame.text(b"\n");
    hint_line(frame, look, class, millis);
    if matches!(verdict, Some(Verdict::Real | Verdict::NotRetried { .. })) {
        bisect_line(frame, look, id);
    }
}

fn wrapped_name<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, (id, attempts): (Identity<'_>, u8)) {
    let total = joined_len(id);
    let label = if attempts == 0 { 0 } else { 8 + digits(usize::from(attempts)) };
    if total + label <= look.0.saturating_sub(2).max(1) {
        frame.text(b" ");
        if look.1 {
            frame.text(b"\x1b[1m");
        }
        joined_range(frame, id, 0, total);
        if look.1 {
            frame.text(RESET);
        }
        attempt_label(frame, look, attempts);
        frame.text(b"\n");
        return;
    }
    hard_wrapped(frame, look, id.0, 0);
    hard_wrapped(frame, look, id.1, NAME_INDENT);
    if attempts != 0 {
        attempt_label(frame, look, attempts);
        frame.text(b"\n");
    }
}

fn attempt_label<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, attempts: u8) {
    if attempts == 0 {
        return;
    }
    let mut text = Line::<16>::new();
    text.text(b"rerun ");
    text.number(usize::from(attempts));
    frame.text(b"  ");
    paint(frame, look.1, FAINT, text.bytes());
}

fn hard_wrapped<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, text: &[u8], indent: usize) {
    let room = look.0.saturating_sub(2 + indent).max(1);
    let mut position = 0;
    while position < text.len() {
        let mut end = (position + room).min(text.len());
        while end < text.len() && is_continuation(text[end]) {
            end += 1;
        }
        frame.text(b" ");
        spaces(frame, indent);
        paint(frame, look.1, BOLD, &text[position..end]);
        frame.text(b"\n");
        position = end;
    }
}

fn class_line<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, (class, millis, location): (Class, u64, Option<Location<'_>>), (template, directory): (&[u8], &[u8])) {
    let word = class_word(class);
    let mut after = Line::<32>::new();
    after.text(b" after ");
    centiseconds(&mut after, millis);
    frame.text(b" ");
    paint(frame, look.1, RED_BOLD, word);
    paint(frame, look.1, FAINT, after.bytes());
    let Some(location) = location else {
        frame.text(b"\n");
        return;
    };
    let right = location.file.len() + 1 + digits(usize::try_from(location.line).unwrap_or(0)) + b"  e opens".len();
    let left = 1 + word.len() + after.bytes().len();
    if left + 2 + right < look.0 {
        spaces(frame, look.0 - 1 - left - right);
    } else {
        frame.text(b"\n ");
    }
    let line = usize::try_from(location.line).unwrap_or(0);
    let mut shown = Line::<LOCATION_BYTES>::new();
    shown.text(location.file);
    shown.text(b":");
    shown.number(line);
    let position = (line, usize::try_from(location.col).unwrap_or(0));
    write_link(frame, !template.is_empty(), template, directory, location.file, position, shown.bytes());
    paint(frame, look.1, FAINT, b"  e opens");
    frame.text(b"\n");
}

fn verdict_line<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, verdict: Verdict, millis: u64) {
    let (word, sgr) = verdict_word(verdict);
    let mut why = Line::<192>::new();
    match verdict {
        Verdict::Real => why.text(b"failed again alone: load and test order are ruled out"),
        Verdict::Flaky => why.text(b"passed alone: it failed under load with other tests running"),
        Verdict::OrderDependent => why.text(b"passed alone, fails in its shard: another test in this binary leaves state behind"),
        Verdict::NotRetried { budget_secs } => {
            why.text(b"not retried alone: it failed after ");
            seconds(&mut why, millis);
            why.text(b", over the ");
            why.number(usize::try_from(budget_secs).unwrap_or(usize::MAX));
            why.text(b"s retry budget. r retries it now");
        }
    }
    paint(frame, look.1, sgr, word);
    let mut column = word.len();
    flow(frame, look.0 - 1, &mut column, 2, why.bytes());
    frame.text(b"\n");
}

fn hint_line<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, class: Class, millis: u64) {
    let mut text = Line::<96>::new();
    match class {
        Class::Timeout { enforced_by: Enforcer::Val } => {
            text.text(b"process group killed after ");
            centiseconds(&mut text, millis);
        }
        Class::Timeout { enforced_by: Enforcer::Harness } => text.text(b"the test's own harness enforced this timeout"),
        Class::Hang => text.text(b"killed at its deadline with no output captured"),
        Class::Crash { signal } => {
            text.text(b"the process died from signal ");
            text.number(usize::try_from(signal).unwrap_or(0));
        }
        Class::Assert | Class::Panic => return,
    }
    frame.text(b" ");
    paint(frame, look.1, FAINT, b"cargo val");
    frame.text(b" ");
    let mut column = 11;
    flow(frame, look.0 - 1, &mut column, 12, text.bytes());
    frame.text(b"\n");
}

fn bisect_line<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, id: Identity<'_>) {
    paint(frame, look.1, CYAN_BOLD, b"find the commit");
    frame.text(b" ");
    let mut column = 16;
    for word in [&b"cargo"[..], b"val", b"bisect", id.0, id.1] {
        flow(frame, look.0 - 1, &mut column, 2, word);
    }
    frame.text(b"\n");
}

fn reason_block<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, (message, sides): Reason<'_>) {
    if message.is_empty() && sides.is_none() {
        return;
    }
    frame.text(b"\n");
    let shown = if sides.is_some() { 1 } else { usize::MAX };
    for line in message.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).take(shown) {
        spaces(frame, 3);
        let mut column = 3;
        flow(frame, look.0 - 1, &mut column, 3, line);
        frame.text(b"\n");
    }
    if let Some((left, right)) = sides {
        side(frame, look, b"left   ", left);
        side(frame, look, b"right  ", right);
    }
}

fn excerpt_block<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, rows: &[u8]) {
    if rows.is_empty() {
        return;
    }
    frame.text(b"\n");
    excerpt::draw(frame, rows, (3, look.0 - 1, look.1));
}

fn indented<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, text: &[u8]) {
    spaces(frame, 3);
    let mut column = 3;
    put(frame, look.0 - 1, &mut column, 3, text);
    frame.text(b"\n");
}

fn side<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, word: &[u8], value: &[u8]) {
    spaces(frame, 3);
    paint(frame, look.1, FAINT, word);
    let mut column = 3 + word.len();
    put(frame, look.0 - 1, &mut column, 3 + word.len(), value);
    frame.text(b"\n");
}

fn window_start(body: &[u8], total: usize, top: Option<u16>) -> usize {
    let last = total.saturating_sub(OUTPUT_LINES);
    if let Some(line) = top {
        return usize::from(line).min(total - 1);
    }
    let panic = body.split(|byte| *byte == b'\n').position(|line| memmem::find(line, b"panicked at").is_some());
    panic.map_or(0, |index| index.saturating_sub(OUTPUT_LINES / 2)).min(last)
}

fn elided<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, count: usize, side: &[u8]) {
    if count == 0 {
        return;
    }
    let mut text = Line::<40>::new();
    text.text(b"   +");
    text.number(count);
    text.text(b" lines ");
    text.text(side);
    paint(frame, look.1, FAINT, text.bytes());
    frame.text(b"\n");
}

fn output_block<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, output: &[u8], top: Option<u16>) {
    let body = output.trim_ascii_end();
    if body.is_empty() {
        return;
    }
    let total = output_lines(body);
    let start = window_start(body, total, top);
    frame.text(b"\n ");
    paint(frame, look.1, FAINT, b"test output  verbatim");
    frame.text(b"\n");
    elided(frame, look, start, b"above");
    for line in body.split(|byte| *byte == b'\n').skip(start).take(OUTPUT_LINES) {
        indented(frame, look, line);
    }
    elided(frame, look, total.saturating_sub(start + OUTPUT_LINES), b"below");
}

fn footer<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, keys: &[(&[u8], &[u8])], (armed, notice): (bool, &[u8])) {
    if armed {
        frame.text(b" ");
        paint(frame, look.1, CYAN_BOLD, b"q again to confirm, any key cancels");
        frame.text(b"\n");
        return;
    }
    if !notice.is_empty() {
        frame.text(b" ");
        paint(frame, look.1, YELLOW, &notice[..notice.len().min(look.0.saturating_sub(2))]);
        frame.text(b"\n");
    }
    keys_line(frame, look, keys);
}

fn keys_line<const FRAME: usize>(frame: &mut Line<FRAME>, look: Look, keys: &[(&[u8], &[u8])]) {
    let limit = look.0 - 1;
    frame.text(b" ");
    let mut column = 1;
    for (key, label) in keys {
        let len = key.len() + 1 + label.len();
        column = separate(frame, column, 1, limit, len, 2);
        paint(frame, look.1, CYAN, key);
        frame.text(b" ");
        frame.text(label);
        column += len;
    }
    frame.text(b"\n");
}

fn flow<const FRAME: usize>(frame: &mut Line<FRAME>, limit: usize, column: &mut usize, indent: usize, text: &[u8]) {
    for word in text.split(|byte| *byte == b' ').filter(|word| !word.is_empty()) {
        let width = word.iter().filter(|byte| !is_continuation(**byte)).count();
        if *column != indent {
            if *column + 1 + width > limit {
                frame.text(b"\n");
                spaces(frame, indent);
                *column = indent;
            } else {
                frame.text(b" ");
                *column += 1;
            }
        }
        put(frame, limit, column, indent, word);
    }
}

fn put<const FRAME: usize>(frame: &mut Line<FRAME>, limit: usize, column: &mut usize, indent: usize, text: &[u8]) {
    for byte in text {
        let byte = if *byte == b'\t' { b' ' } else { *byte };
        if (byte < 0x20 && byte != b'\t') || byte == 0x7f {
            continue;
        }
        if !is_continuation(byte) {
            if *column >= limit {
                frame.text(b"\n");
                spaces(frame, indent);
                *column = indent;
            }
            *column += 1;
        }
        frame.text(&[byte]);
    }
}

#[cfg(test)]
mod tests;
