//! Shard children: many tests in one libtest process, each counted as its result line streams in.

use super::{
    CargoDriver, ExactTestIntent, ExecutablePath, LaunchCause, LaunchInput, LaunchOwner, Ledger,
    Line, ListingRun, NAME_CAPACITY, ProcessMode, RunError, RunningChild, Tally, TerminalOutcome, TestWatch,
    Watches, executable_text, override_entries, report_launch_failure, report_nothing_ran, slot_range, spawn_reserved, start_session, write_fd,
    monotonic_millis, projected, rendered, emit, emit_kept, Render,
};
use crate::failure_class::Exit;
use crate::render::Event;
use core::ops::Range;

use crate::build_defaults::SHARD_COMMAND_UNITS;
use crate::discover_listing::{DocEvent, DocOutcome, DocParser};
use crate::edge_signal::release_child;
use crate::run::{JOB_BASE, Lanes};
use crate::runner::LibtestVerdict;
use crate::sched_file::{Loaded, load};
use crate::shard_arena::{Located, PushError, Rerun, Reported, Shard, ShardArena};
use crate::shard_sched::{Class, group_key};
use crate::status_queue::{self, EVENT_RESERVE};

pub(super) const FAILURE_CAPACITY: usize = 16384;
const FIXED_UNITS: usize = 3 + "--exact ".len();
const GROUP_CAPACITY: usize = crate::sched_binary::GROUP_CAPACITY;

/// Per-child state for a shard: which of its names reported, the libtest report parser, and the failure blocks it has streamed.
pub(super) struct ShardWatch {
    shard: Shard,
    parser: DocParser,
    reported: Reported,
    ambiguous: Reported,
    failed: Reported,
    blocked: Reported,
    expected: (usize, usize),
    failures: [u8; FAILURE_CAPACITY],
    failure_length: usize,
    keeping: bool,
    streamed: (usize, usize, usize),
    deferred: (usize, usize, usize),
    group: [u8; GROUP_CAPACITY],
    group_length: usize,
    /// The lane window and the offset in it where framing paused for lack of status room; the lane stays gated until it is fed.
    backlog: Option<(Range<u32>, usize)>,
    /// How many times framing paused on a full status queue.
    pauses: usize,
    /// The ordinals of this shard's names whose START has not been queued yet; non-empty keeps the lane gated.
    announcing: Range<usize>,
}

impl ShardWatch {
    // a shard child starts all its names at once, so every name that has not reported is in flight inside the one group
    pub(super) fn unreported<'arena>(&self, arena: &'arena ShardArena) -> impl Iterator<Item = &'arena [u8]> {
        let (shard, reported) = (self.shard, self.reported);
        (0..shard.count)
            .filter(move |ordinal| !reported.has(*ordinal))
            .filter_map(move |ordinal| arena.nth(shard, ordinal))
    }

    fn new(shard: Shard, group: &[u8]) -> Self {
        let mut watch = Self {
            shard,
            parser: DocParser::new(),
            reported: Reported::new(),
            ambiguous: Reported::new(),
            failed: Reported::new(),
            blocked: Reported::new(),
            expected: (shard.start, 0),
            failures: [0; FAILURE_CAPACITY],
            failure_length: 0,
            keeping: false,
            streamed: (0, 0, 0),
            deferred: (0, 0, 0),
            backlog: None,
            pauses: 0,
            announcing: 0..0,
            group: [0; GROUP_CAPACITY],
            group_length: group.len().min(GROUP_CAPACITY),
        };
        watch.group[..watch.group_length].copy_from_slice(&group[..watch.group_length]);
        watch
    }

    fn class(&self) -> Class {
        Class::from_rank(self.shard.class)
    }

    fn counts_against(&self, group: &[u8]) -> bool {
        matches!(self.class(), Class::Shared | Class::Grouped) && &self.group[..self.group_length] == group
    }

    fn flush_failures(&mut self, render: &Render<'_>) -> Result<(), RunError> {
        if self.keeping {
            self.keeping = false;
            let closing = projected(render, &Event::BlockClosed);
            if !closing.bytes().is_empty() {
                keep_failure_line(&mut self.failures, &mut self.failure_length, (render, closing.bytes().trim_ascii_end()))?;
            }
        }
        flush(&self.failures, &mut self.failure_length, render)
    }
}

fn flush(failures: &[u8; FAILURE_CAPACITY], length: &mut usize, render: &Render<'_>) -> Result<(), RunError> {
    if *length != 0 {
        emit_kept(render, &failures[..*length])?;
        *length = 0;
    }
    Ok(())
}

// blocks are written whole when they fit, so two shards failing at once do not interleave inside one block
fn keep_failure_line(failures: &mut [u8; FAILURE_CAPACITY], length: &mut usize, (render, line): (&Render<'_>, &[u8])) -> Result<(), RunError> {
    if *length + line.len() + 1 > FAILURE_CAPACITY {
        flush(failures, length, render)?;
    }
    let taken = line.len().min(FAILURE_CAPACITY - 1);
    failures[*length..*length + taken].copy_from_slice(&line[..taken]);
    failures[*length + taken] = b'\n';
    *length += taken + 1;
    Ok(())
}

// plain output closes the block that was open and opens the next kept one with its own delimiter lines
fn keep_block_edge(
    run: &ListingRun<'_>,
    (target, name): (Option<usize>, &[u8]),
    (was_open, keeping): (bool, bool),
    (failures, failure_length): (&mut [u8; FAILURE_CAPACITY], &mut usize),
) -> Result<(), RunError> {
    let render = run.render();
    if was_open {
        let closing = projected(render, &Event::BlockClosed);
        if !closing.bytes().is_empty() {
            keep_failure_line(failures, failure_length, (render, closing.bytes().trim_ascii_end()))?;
        }
    }
    if keeping {
        let targets = run.targets.borrow();
        let id = (target.map_or(&b""[..], |index| targets.id(index)), name);
        let opening = projected(render, &Event::BlockOpened { id });
        if !opening.bytes().is_empty() {
            keep_failure_line(failures, failure_length, (render, opening.bytes().trim_ascii_end()))?;
        }
    }
    Ok(())
}

// libtest prints `---- <name> stdout ----` above the captured output of each failed test
fn failure_block_name(line: &[u8]) -> Option<&[u8]> {
    line.strip_prefix(b"---- ")?.strip_suffix(b" stdout ----")
}

fn tally_streamed(streamed: &mut (usize, usize, usize), outcome: TerminalOutcome) {
    match outcome {
        TerminalOutcome::Passed => streamed.0 += 1,
        TerminalOutcome::Failed => streamed.1 += 1,
        TerminalOutcome::Ignored => streamed.2 += 1,
    }
}

fn terminal(outcome: DocOutcome) -> (TerminalOutcome, Tally) {
    match outcome {
        DocOutcome::Passed => (TerminalOutcome::Passed, Tally::Passed),
        DocOutcome::Failed => (TerminalOutcome::Failed, Tally::Failed),
        DocOutcome::Ignored => (TerminalOutcome::Ignored, Tally::Skipped),
    }
}

fn count_reported(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    target: Option<usize>,
    name: &[u8],
    (outcome, tally): (TerminalOutcome, Tally),
    solo: bool,
) -> Result<(), RunError> {
    ledger.count(outcome).map_err(|_| RunError::Outcome)?;
    if let Some(target) = target {
        run.targets.borrow_mut().tally(target, tally);
    }
    let targets = run.targets.borrow();
    let id = (target.map_or(&b""[..], |index| targets.id(index)), name);
    emit(run.render(), &Event::Settled { id, outcome: rendered(outcome), at_ms: Some(monotonic_millis()), solo })
}

// a shard's failure is only counted when it would trip the stop policy; otherwise a solo run decides it
fn would_trip(run: &ListingRun<'_>, ledger: &Ledger<'_>) -> bool {
    run.budget_spent(ledger.snapshot().failed + run.docs.failed.get() + 1)
}

// a solo run cargo-val terminated is withdrawn: its exit says nothing about the test
pub(super) fn withdraw_solo(run: &ListingRun<'_>, ledger: &mut Ledger<'_>, target: Option<usize>) -> Result<(), RunError> {
    ledger.withdraw(1).map_err(|_| RunError::Outcome)?;
    if let Some(target) = target {
        run.targets.borrow_mut().tally(target, Tally::Withdrawn);
    }
    run.shed(ledger, 1);
    Ok(())
}

/// What one pass over a lane piece did: whether the stop policy tripped, and how many bytes the framer consumed before it paused.
pub(super) struct Observed {
    pub(super) stop: bool,
    pub(super) consumed: usize,
}

/// Whether this shard's framing is paused on a window the status queue could not take whole.
pub(super) fn stalled(watch: &TestWatch) -> bool {
    watch.shard.as_ref().is_some_and(|shard| shard.backlog.is_some() || !shard.announcing.is_empty())
}

/// Records where framing paused; the first pause in a window wins, since later pieces of that window are fed by the resume.
pub(super) fn stall(watch: &mut TestWatch, window: Range<u32>, offset: usize) {
    if let Some(shard) = watch.shard.as_mut() {
        shard.backlog.get_or_insert((window, offset));
    }
}

/// Takes the paused window so the resume can feed its unframed remainder.
pub(super) fn take_backlog(watch: &mut TestWatch) -> Option<(Range<u32>, usize)> {
    watch.shard.as_mut().and_then(|shard| shard.backlog.take())
}

// gate: In = one event, Out = the same event, Err = Cancelled once the trip fired; counted results and the closing tally both refuse to arrive after it
fn trip_gate<'line>(run: &ListingRun<'_>, ledger: &Ledger<'_>, event: DocEvent<'line>) -> Result<DocEvent<'line>, RunError> {
    match event {
        DocEvent::Result { .. } | DocEvent::Finished { .. } if run.tripped(ledger) => Err(RunError::Cancelled),
        admitted => Ok(admitted),
    }
}

// frame: In = unframed lane bytes, Out = one line with its newline (a trailing partial line is the whole remainder), Err = none.
// it consumes exactly the bytes it returns and never captures them; the output capture happened before any parse.
fn frame_line(bytes: &[u8]) -> &[u8] {
    let length = memchr::memchr(b'\n', bytes).map_or(bytes.len(), |newline| newline + 1);
    &bytes[..length]
}

fn debug_pause(run: &ListingRun<'_>, pauses: usize) {
    if !run.settings.debug {
        return;
    }
    let mut line = Line::<96>::new();
    line.text(b"cargo-val: debug: shard output paused pauses=");
    line.number(pauses);
    line.text(b"\n");
    let _ = write_fd(2, line.bytes());
}

/// Feeds a shard child's stdout to its libtest parser as a pipe chain, counting each test as its result line completes.
///
/// The chain per line: `frame_line` (In = unframed bytes, Out = one line, no error) -> `DocParser::feed`
/// (In = one line, Out = [`DocEvent`], Err = the settle error) -> settle (In = [`DocEvent`], Out = a status record
/// written to the queue, Err = [`RunError`]). Before each line's settle step the status queue must have room for one
/// event; without it the chain stops there, defers, and returns how far it got so the caller keeps the remainder on the lane.
/// `stop` reports that the run tripped its stop policy, so the caller ends the other sessions once this watch is released.
pub(super) fn observe(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watch: &mut TestWatch,
    bytes: &[u8],
) -> Result<Observed, RunError> {
    let target = ledger.sessions().get(watch.slot).map(RunningChild::target);
    let whole = Observed { stop: false, consumed: bytes.len() };
    let Some(shard_watch) = watch.shard.as_mut() else {
        return Ok(whole);
    };
    if shard_watch.shard.rerun.is_some() {
        return Ok(whole);
    }
    let ShardWatch {
        shard,
        parser,
        reported,
        ambiguous,
        failed,
        blocked,
        expected,
        failures,
        failure_length,
        keeping,
        streamed,
        deferred,
        backlog: _,
        pauses,
        ..
    } = shard_watch;
    let shard = *shard;
    let mut stop = false;
    let mut apply = |ledger: &mut Ledger<'_>, event: DocEvent<'_>| -> Result<(), RunError> {
        match event {
            DocEvent::Result { name, outcome, stray } => {
                let located = run.shards.borrow().locate(shard, *expected, name);
                let Some(Located { ordinal, next }) = located else {
                    return write_fd(2, b"cargo-val: a shard child reported a test that was not in its shard\n");
                };
                if reported.has(ordinal) || ambiguous.has(ordinal) {
                    return Ok(());
                }
                *expected = (next, ordinal + 1);
                if stray {
                    ambiguous.mark(ordinal);
                    tally_streamed(deferred, terminal(outcome).0);
                    return Ok(());
                }
                if outcome == DocOutcome::Failed && !would_trip(run, ledger) {
                    failed.mark(ordinal);
                    tally_streamed(deferred, TerminalOutcome::Failed);
                    return Ok(());
                }
                reported.mark(ordinal);
                if outcome == DocOutcome::Failed {
                    failed.mark(ordinal);
                }
                let counted = terminal(outcome);
                count_reported(run, ledger, target, name, counted, false)?;
                tally_streamed(streamed, counted.0);
                stop |= counted.0 == TerminalOutcome::Failed && run.tripped(ledger);
            }
            DocEvent::FailureLine(line) => {
                if let Some(name) = failure_block_name(line) {
                    let was_open = std::mem::replace(keeping, false);
                    if let Some(Located { ordinal, .. }) = run.shards.borrow().locate(shard, *expected, name)
                        && reported.has(ordinal)
                        && failed.has(ordinal)
                    {
                        blocked.mark(ordinal);
                        *keeping = true;
                    }
                    return keep_block_edge(run, (target, name), (was_open, *keeping), (failures, failure_length));
                }
                if *keeping {
                    keep_failure_line(failures, failure_length, (run.render(), line))?;
                }
            }
            DocEvent::Finished { passed, failed: failed_count, ignored } => {
                let expected_counts = (streamed.0 + deferred.0, streamed.1 + deferred.1, streamed.2 + deferred.2);
                if (passed, failed_count, ignored) != expected_counts {
                    write_fd(2, b"cargo-val: a shard child's test result line disagrees with the results it streamed\n")?;
                }
            }
            DocEvent::Running { .. } => {}
        }
        Ok(())
    };
    let mut settle = |event: DocEvent<'_>| match trip_gate(run, ledger, event).and_then(|admitted| apply(ledger, admitted)) {
        Err(RunError::Cancelled) => Ok(()),
        settled => settled,
    };
    let mut offset = 0;
    while offset < bytes.len() {
        if !status_queue::room_for(EVENT_RESERVE) {
            *pauses += 1;
            status_queue::defer();
            debug_pause(run, *pauses);
            break;
        }
        let line = frame_line(&bytes[offset..]);
        parser.feed(line, &mut settle)?;
        offset += line.len();
    }
    Ok(Observed { stop, consumed: offset })
}

fn debug_launch(run: &ListingRun<'_>, watches: &Watches, (class, shard): (Class, Shard), in_flight: usize) {
    if !run.settings.debug {
        return;
    }
    let (group, group_length) = group_of(run, shard);
    let same_group = watches
        .iter()
        .flatten()
        .filter(|watch| watch.shard.as_ref().is_some_and(|live| live.counts_against(&group[..group_length])))
        .count();
    let mut line = Line::<192>::new();
    line.text(b"cargo-val: debug: shard launch class=");
    line.text(class.text());
    line.text(b" names=");
    line.number(shard.count);
    line.text(b" in_flight=");
    line.number(in_flight);
    line.text(b" group_in_flight=");
    line.number(same_group);
    line.text(if shard.rerun.is_some() { b" relaunch=true\n" } else { b" relaunch=false\n" });
    let _ = write_fd(2, line.bytes());
}

/// Whether the shard machinery still has names to launch, shards running, or declared tests waiting.
pub(super) fn busy(run: &ListingRun<'_>) -> bool {
    !run.shards.borrow().idle() || run.specials.borrow().has_work()
}

fn announce_refusal(what: &[u8], path: &str, detail: &[u8]) {
    let mut line = Line::<1024>::new();
    line.text(b"cargo-val: ");
    line.text(what);
    line.text(b" in ");
    line.text(path.as_bytes());
    line.text(b": ");
    line.text(detail);
    line.text(b"\n");
    let _ = write_fd(2, line.bytes());
}

// the records are read once per binary, from its file, before any name of it is classified
pub(super) fn load_records(run: &ListingRun<'_>, executable: &str, target: usize) -> Result<(), RunError> {
    if run.sched_target.get() == Some(target) {
        return Ok(());
    }
    run.sched_target.set(Some(target));
    let mut table = run.sched.borrow_mut();
    table.clear();
    match load(executable, table.storage()) {
        Ok(Loaded::Records { count, little_endian }) => table.adopt(count, little_endian),
        Ok(Loaded::Absent(_)) => {}
        Ok(Loaded::Refused(crate::sched_binary::Fault::Capacity)) => return Err(RunError::DeclaredCapacity),
        Ok(Loaded::Refused(fault)) => {
            let detail = match fault {
                crate::sched_binary::Fault::Version(_) => &b"scheduling records of an unknown version; scheduling by default"[..],
                crate::sched_binary::Fault::Partial => b"scheduling section is not whole records; scheduling by default",
                crate::sched_binary::Fault::Length => b"a scheduling record length is out of range; scheduling by default",
                crate::sched_binary::Fault::Capacity => b"more scheduling records than the table holds; scheduling by default",
            };
            announce_refusal(b"scheduling records refused", executable, detail);
        }
        Err(_) => announce_refusal(b"scheduling records unreadable", executable, b"the binary could not be read; scheduling by default"),
    }
    Ok(())
}

/// Whether the next per-test name waits unconsumed: a serial name while anything runs, a grouped name while its group runs.
/// A name with nothing in flight never waits, so the listing always drains.
pub(super) fn holds(run: &ListingRun<'_>, watches: &Watches, in_flight: usize, name: &str) -> bool {
    if run.settings.process != ProcessMode::Test || in_flight == 0 {
        return false;
    }
    let table = run.sched.borrow();
    let Some(entry) = table.find(name.as_bytes()).and_then(|index| table.entry(index)) else {
        return false;
    };
    entry.serial() || (!entry.group.is_empty() && watches.iter().flatten().any(|watch| watch.in_group(entry.group)))
}

/// Records the group a per-test child launched under so a later name of that group waits for it.
pub(super) fn note_group(run: &ListingRun<'_>, watches: &mut Watches, slot: usize, name: &str) {
    let table = run.sched.borrow();
    let Some(entry) = table.find(name.as_bytes()).and_then(|index| table.entry(index)) else {
        return;
    };
    if let Some(watch) = watches[slot].as_mut() {
        let length = entry.group.len().min(GROUP_CAPACITY);
        watch.group[..length].copy_from_slice(&entry.group[..length]);
        watch.group_length = length;
    }
}

/// Seals the arena once the active binary's listing has ended so its names carve into about `jobs` shards.
pub(super) fn seal(run: &ListingRun<'_>) {
    if run.settings.process != ProcessMode::Shard {
        return;
    }
    let mut arena = run.shards.borrow_mut();
    if !arena.sealed() {
        arena.seal(run.settings.jobs);
        run.specials.borrow_mut().seal(&run.sched.borrow());
    }
}

/// Takes one listed name: a declared test joins the scheduling plan, any other joins the arena.
/// Returns whether the listing must pause because the arena is full, after parking the name.
pub(super) fn collect(run: &ListingRun<'_>, (executable, target): (&str, usize), name: &str) -> Result<bool, RunError> {
    load_records(run, executable, target)?;
    if let Some(index) = run.sched.borrow().find(name.as_bytes()) {
        let declared = run.sched.borrow().entry(index).map(|entry| Class::of(&entry));
        if declared.is_some_and(|class| class != Class::Plain) && run.specials.borrow_mut().push(index) {
            return Ok(false);
        }
    }
    let mut arena = run.shards.borrow_mut();
    match arena.push(name) {
        Ok(()) => Ok(false),
        Err(PushError::Full) => {
            arena.seal(run.settings.jobs);
            run.specials.borrow_mut().seal(&run.sched.borrow());
            Ok(true)
        }
        Err(PushError::InteriorNul) => Err(RunError::Arguments),
        Err(PushError::TooLong) => Err(RunError::Launch(LaunchCause::NameTooLong {
            required: name.len(),
            available: NAME_CAPACITY,
        })),
    }
}

// a relaunch comes first, then plain names, then declared tests whose group is free; a serial class waits for nothing else in flight
// once the run has tripped, only queued reruns launch: every name not yet started has been dropped
fn next_shard(run: &ListingRun<'_>, watches: &Watches, fixed: usize, in_flight: usize, stopped: bool) -> Option<(Shard, Class)> {
    if watches.iter().flatten().any(|watch| watch.shard.as_ref().is_some_and(|shard| shard.class().serial())) {
        return None;
    }
    let mut arena = run.shards.borrow_mut();
    if stopped {
        let rerun = arena.carve_rerun()?;
        return Some((rerun, Class::from_rank(rerun.class)));
    }
    if !arena.sealed() {
        return None;
    }
    if let Some(shard) = arena.carve(fixed) {
        return Some((shard, Class::from_rank(shard.class)));
    }
    let table = run.sched.borrow();
    let mut plan = run.specials.borrow_mut();
    let busy = |group: &[u8]| {
        watches
            .iter()
            .flatten()
            .any(|watch| watch.shard.as_ref().is_some_and(|shard| shard.counts_against(group)))
    };
    let alone = in_flight == 0 && !arena.has_work();
    let budget = SHARD_COMMAND_UNITS.saturating_sub(fixed);
    let special = plan.take(&table, busy, alone, budget)?;
    let names = (special.first..special.last)
        .filter_map(|position| plan.at(position))
        .filter_map(|index| table.entry(index))
        .map(|entry| entry.path);
    let mut shard = arena.adopt(names)?;
    plan.commit(&special);
    shard.class = special.class.rank();
    shard.anchor = u32::try_from(special.first).unwrap_or(0);
    Some((shard, special.class))
}

// a name dropped before it launched leaves the discovered count too, or the run never drains
fn withdraw_unlaunched(run: &ListingRun<'_>, ledger: &mut Ledger<'_>, target: usize, dropped: usize) -> Result<(), RunError> {
    ledger.withdraw(dropped).map_err(|_| RunError::Outcome)?;
    (0..dropped).for_each(|_| run.targets.borrow_mut().tally(target, Tally::Withdrawn));
    Ok(())
}

fn group_of(run: &ListingRun<'_>, shard: Shard) -> ([u8; GROUP_CAPACITY], usize) {
    let mut group = [0; GROUP_CAPACITY];
    let class = Class::from_rank(shard.class);
    let table = run.sched.borrow();
    let entry = run
        .specials
        .borrow()
        .at(shard.anchor as usize)
        .and_then(|index| table.entry(index));
    let key = entry.map_or(&[][..], |entry| group_key(class, entry.group));
    let length = key.len().min(GROUP_CAPACITY);
    group[..length].copy_from_slice(&key[..length]);
    (group, length)
}

impl CargoDriver<'_> {
    /// Launches the next shards while a job slot is free; returns whether anything moved.
    pub(super) fn dispatch_shards(
        &mut self,
        lanes: &Lanes,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<bool, RunError> {
        if run.settings.process != ProcessMode::Shard {
            return Ok(false);
        }
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        {
            let mut arena = run.shards.borrow_mut();
            if arena.sealed() && arena.idle() && !run.specials.borrow().has_work() {
                arena.reset();
                run.specials.borrow_mut().reset();
                return Ok(true);
            }
        }
        if run.interrupted() {
            run.root.fire();
            let dropped = run.shards.borrow_mut().discard_everything() + run.specials.borrow_mut().discard();
            run.shed(ledger, dropped);
            withdraw_unlaunched(run, ledger, active.target, dropped)?;
            return Ok(dropped > 0);
        }
        let stopped = run.admit(ledger).is_err();
        let mut moved = false;
        if stopped {
            let queued = core::iter::from_fn(|| run.shards.borrow_mut().pop_queued());
            for names in queued.chain(Some(run.specials.borrow_mut().discard())) {
                run.shed(ledger, names);
                withdraw_unlaunched(run, ledger, active.target, names)?;
                moved |= names > 0;
            }
        }
        let executable = core::str::from_utf8(&active.path[..active.path_length]).map_err(|_| RunError::Arguments)?;
        let environment = &self.environments[slot_range(active.slot)];
        let (overrides, override_count) = override_entries(&active.layout, environment)?;
        let fixed = executable.len() + FIXED_UNITS + run.harness_arguments.iter().map(|argument| argument.len() + 1).sum::<usize>();
        while ledger.sessions().len() < run.settings.jobs {
            let in_flight = ledger.sessions().len();
            let Some((shard, class)) = next_shard(run, watches, fixed, in_flight, stopped) else {
                break;
            };
            debug_launch(run, watches, (class, shard), in_flight);
            let arena = run.shards.borrow();
            let names = core::str::from_utf8(arena.names(shard)).map_err(|_| RunError::Arguments)?;
            let first = names.split('\0').next().unwrap_or("");
            let mut storage = run.launch_storage.borrow_mut();
            let intent = ExactTestIntent::new(ExecutablePath::Borrowed(executable), names);
            let slot = spawn_reserved(|| {
                LaunchInput {
                    target: active.target,
                    owner: LaunchOwner::Ledger(&mut *ledger),
                    intent,
                    inherited_environment: run.environment,
                    child_overrides: &overrides[..override_count],
                    storage: &mut storage,
                    harness_arguments: run.harness_arguments,
                    current_directory: active.layout.current_directory(environment).as_bytes(),
                }
                .launch_shard(if shard.rerun.is_some() { 0 } else { shard.count })
                .map_err(|error| {
                    report_launch_failure(LaunchCause::from(&error), first, executable_text(intent.executable()))
                })
            })?;
            drop(storage);
            drop(arena);
            if shard.rerun.is_none() {
                (0..shard.count).for_each(|_| run.targets.borrow_mut().tally(active.target, Tally::Launched));
            }
            let (group, group_length) = group_of(run, shard);
            let mut watch = ShardWatch::new(shard, &group[..group_length]);
            watch.announcing = 0..announcement_total(shard);
            announce_starts(run, active.target, &mut watch)?;
            let paused = !watch.announcing.is_empty();
            let scope = run.session_scope(shard.rerun);
            start_session(ledger, watches, (lanes, &run.store), slot, (Some(watch), scope))?;
            lanes[JOB_BASE + slot.index()].3.set(!paused);
            moved = true;
        }
        Ok(moved)
    }
}

// a shard child starts all its names at once; a rerun starts only the one name its exit will decide
const fn announcement_total(shard: Shard) -> usize {
    match shard.rerun {
        Some(Rerun::OutputOnly) => 0,
        Some(Rerun::Decides { .. }) => 1,
        None => shard.count,
    }
}

// each START takes room first; a full queue defers the rest and the watch stays stalled until the pump drains and resumes it
pub(super) fn announce_starts(run: &ListingRun<'_>, target: usize, watch: &mut ShardWatch) -> Result<(), RunError> {
    let solo = watch.shard.rerun.is_some();
    let arena = run.shards.borrow();
    let targets = run.targets.borrow();
    while !watch.announcing.is_empty() {
        if !status_queue::room_for(EVENT_RESERVE) {
            status_queue::defer();
            return Ok(());
        }
        let ordinal = watch.announcing.start;
        watch.announcing.start += 1;
        if let Some(name) = arena.nth(watch.shard, ordinal) {
            let started = Event::Started { id: (targets.id(target), name), solo, at_ms: monotonic_millis(), limit_ms: run.settings.test_limit_ms };
            emit(run.render(), &started)?;
        }
    }
    Ok(())
}

/// Continues a paused START announcement once the queue has room; true when none remain.
pub(super) fn resume_announcing(run: &ListingRun<'_>, ledger: &Ledger<'_>, watch: &mut TestWatch) -> Result<bool, RunError> {
    let target = ledger.sessions().get(watch.slot).map(RunningChild::target);
    let Some((shard_watch, target)) = watch.shard.as_mut().zip(target) else {
        return Ok(true);
    };
    announce_starts(run, target, shard_watch)?;
    Ok(shard_watch.announcing.is_empty())
}

/// Settles a shard child that exited: names that never reported are failed, a crash is pinned on the test that was running
/// and the names behind it run again, and a stopped run drops them. A rerun child settles only its own single name.
pub(super) fn finish(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
    status: i32,
) -> Result<(), RunError> {
    let Some(mut watch) = watches[index].take() else {
        return Ok(());
    };
    release_child(watch.process_id);
    let target = ledger.sessions().get(watch.slot).map(RunningChild::target);
    let Some(mut shard_watch) = watch.shard.take() else {
        return Ok(());
    };
    shard_watch.flush_failures(run.render())?;
    if shard_watch.shard.rerun.is_some() {
        finish_rerun(run, ledger, target, (&shard_watch, &watch), (watch.exit(status), status))?;
        ledger.retire(watch.slot);
        run.shards.borrow_mut().finished(shard_watch.shard);
        return Ok(());
    }
    let interrupted = run.interrupted();
    let stopped = run.tripped(ledger) || interrupted;
    if stopped {
        if !interrupted {
            queue_output_reruns(&mut run.shards.borrow_mut(), &shard_watch)?;
        }
        settle_stopped(run, ledger, target, &shard_watch)?;
    } else {
        queue_solo_reruns(run, ledger, &mut run.shards.borrow_mut(), &shard_watch)?;
    }
    ledger.retire(watch.slot);
    run.shards.borrow_mut().finished(shard_watch.shard);
    Ok(())
}

// a stopped run withdraws every name that did not report; no name is blamed for a crash
fn settle_stopped(run: &ListingRun<'_>, ledger: &mut Ledger<'_>, target: Option<usize>, shard_watch: &ShardWatch) -> Result<(), RunError> {
    let withdrawn = shard_watch.shard.count - shard_watch.reported.total();
    if withdrawn != 0 {
        ledger.withdraw(withdrawn).map_err(|_| RunError::Outcome)?;
        if let Some(target) = target {
            (0..withdrawn).for_each(|_| run.targets.borrow_mut().tally(target, Tally::Withdrawn));
        }
        run.shed(ledger, withdrawn);
    }
    Ok(())
}

// a deciding solo that cargo-val itself ended, by interrupt or by a fail-fast stop, reports nothing about its test
fn withdrawn_solo(interrupted: bool, exit: Result<i32, RunError>, rerun: Option<Rerun>) -> bool {
    matches!(rerun, Some(Rerun::Decides { .. })) && (interrupted || exit == Err(RunError::Cancelled))
}

// a rerun child's exit is its verdict when the rerun decides; otherwise it only shows the output of a failure
fn finish_rerun(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    target: Option<usize>,
    (shard_watch, watch): (&ShardWatch, &TestWatch),
    (exit, status): (Result<i32, RunError>, i32),
) -> Result<(), RunError> {
    let shard = shard_watch.shard;
    let arena = run.shards.borrow();
    let name = arena.nth(shard, 0).unwrap_or(b"");
    if withdrawn_solo(run.interrupted(), exit, shard.rerun) {
        return withdraw_solo(run, ledger, target);
    }
    let solo = true;
    match (shard.rerun, exit == Ok(0)) {
        (Some(Rerun::Decides { after_shard_failure }), true) => {
            if after_shard_failure {
                warn_rerun(run, target, name, b"failed in a shared process, passed alone")?;
            }
            let counted = match watch.output.verdict() {
                LibtestVerdict::NothingRan => return Err(report_nothing_ran(&run.targets, target, name)),
                LibtestVerdict::Skipped => (TerminalOutcome::Ignored, Tally::Skipped),
                _ => (TerminalOutcome::Passed, Tally::Passed),
            };
            count_reported(run, ledger, target, name, counted, solo)
        }
        (Some(Rerun::Decides { .. }), false) => {
            count_reported(run, ledger, target, name, (TerminalOutcome::Failed, Tally::Failed), solo)?;
            failure_output(run, target, name, (watch, status))?;
            run.tripped(ledger);
            Ok(())
        }
        (_, true) => warn_rerun(run, target, name, b"passed when re-run alone"),
        (_, false) => block_output(run, target, name, (watch, status)),
    }
}

// the streamed path prints libtest's failure block in cargo-val's frame and says nothing about reproduction, so the rerun does the same
fn block_output(run: &ListingRun<'_>, target: Option<usize>, name: &[u8], (watch, status): (&TestWatch, i32)) -> Result<(), RunError> {
    let targets = run.targets.borrow();
    let id = (target.map_or(&b""[..], |index| targets.id(index)), name);
    run.emit_shaped(id, watch.streams.as_ref(), Exit::from_status(status), |stdout, stderr| match failure_body(stdout.0) {
        Some(body) => ((body, stdout.1), (b"", 0)),
        None => (stdout, stderr),
    })
}

// the lines between libtest's `---- <name> stdout ----` header and the `failures:` that closes the section, found by the parser that streams them
fn failure_body(stdout: &[u8]) -> Option<&[u8]> {
    let mut parser = DocParser::new();
    let mut offset = 0;
    let (mut start, mut end) = (None, 0);
    while offset < stdout.len() {
        let line = frame_line(&stdout[offset..]);
        let next = offset + line.len();
        let fed = parser.feed(line, |event| {
            if let DocEvent::FailureLine(kept) = event {
                match failure_block_name(kept) {
                    Some(_) => _ = start.get_or_insert(next),
                    None => end = next,
                }
            }
            Ok::<(), RunError>(())
        });
        fed.ok()?;
        offset = next;
    }
    start.map(|begin| &stdout[begin..end.max(begin)])
}

fn warn_rerun(run: &ListingRun<'_>, target: Option<usize>, name: &[u8], what: &[u8]) -> Result<(), RunError> {
    let targets = run.targets.borrow();
    let id = (target.map_or(&b""[..], |index| targets.id(index)), name);
    emit(run.render(), &Event::Warned { id, what })
}

fn failure_output(run: &ListingRun<'_>, target: Option<usize>, name: &[u8], (watch, status): (&TestWatch, i32)) -> Result<(), RunError> {
    let targets = run.targets.borrow();
    let id = (target.map_or(&b""[..], |index| targets.id(index)), name);
    run.emit_failure(id, watch.streams.as_ref(), Exit::from_status(status))
}

// every name that did not settle on its clean shard line owes a solo run, and that solo run's exit decides it
fn queue_solo_reruns(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    arena: &mut ShardArena<'_>,
    shard_watch: &ShardWatch,
) -> Result<(), RunError> {
    let shard = shard_watch.shard;
    let mut owed = Reported::new();
    (0..shard.count)
        .filter(|ordinal| !shard_watch.reported.has(*ordinal) || shard_watch.failed.has(*ordinal))
        .for_each(|ordinal| _ = owed.mark(ordinal));
    arena.owe_reruns(shard, (owed, shard_watch.failed), true).map_err(|_| RunError::Outcome)?;
    run.tripped(ledger);
    Ok(())
}

// a stopped run keeps only the output of a failure it counted, and that rerun never counts
fn queue_output_reruns(arena: &mut ShardArena<'_>, shard_watch: &ShardWatch) -> Result<(), RunError> {
    let shard = shard_watch.shard;
    let mut owed = Reported::new();
    (0..shard.count)
        .filter(|ordinal| shard_watch.reported.has(*ordinal) && shard_watch.failed.has(*ordinal) && !shard_watch.blocked.has(*ordinal))
        .for_each(|ordinal| _ = owed.mark(ordinal));
    arena.owe_reruns(shard, (owed, shard_watch.failed), false).map_err(|_| RunError::Outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::scope_for;
    use proxima_core::signal::Signal;

    const DECIDING: Option<Rerun> = Some(Rerun::Decides { after_shard_failure: false });
    const OUTPUT: Option<Rerun> = Some(Rerun::OutputOnly);

    fn scopes() -> (Signal, Signal) {
        let root = Signal::new();
        let trip = root.child();
        (root, trip)
    }

    mod cases {
        use super::*;
        use crate::driver::ChildOutput;

        #[proxima::test]
        async fn fail_fast_spares_output_reruns() {
            let (root, trip) = scopes();
            let scope = scope_for(OUTPUT, (&root, &trip));
            trip.fire();
            assert!(!scope.is_fired());
        }

        #[proxima::test]
        async fn fail_fast_kills_deciding_and_plain_sessions() {
            let (root, trip) = scopes();
            let scoped = [scope_for(DECIDING, (&root, &trip)), scope_for(None, (&root, &trip))];
            trip.fire();
            assert!(scoped.iter().all(Signal::is_fired));
        }

        #[proxima::test]
        async fn interrupt_kills_everything() {
            let (root, trip) = scopes();
            let scoped = [scope_for(OUTPUT, (&root, &trip)), scope_for(DECIDING, (&root, &trip)), scope_for(None, (&root, &trip))];
            root.fire();
            assert!(scoped.iter().all(Signal::is_fired));
        }

        #[proxima::test]
        async fn a_frozen_session_exits_as_cancelled_once_an_interrupt_fires_its_scope() {
            let (root, trip) = scopes();
            let mut slots = crate::schedule::SessionSlots::<u8, 1>::new();
            let frozen = TestWatch {
                slot: slots.insert(0).expect("scratch table has a slot"),
                process_id: 0,
                scope: scope_for(None, (&root, &trip)),
                output: ChildOutput::new(),
                streams: None,
                shard: None,
                group: [0; GROUP_CAPACITY],
                group_length: 0,
                started_ms: 0,
                status: None,
                rerun: None,
            };
            assert_eq!(frozen.exit(0), Ok(0));
            root.fire();
            assert_eq!(frozen.exit(0), Err(RunError::Cancelled));
        }

        #[proxima::test]
        async fn deciding_solos_are_killed_on_fail_fast() {
            assert!(!withdrawn_solo(false, Ok(0), DECIDING));
        }

        #[proxima::test]
        async fn interrupted_deciding_solo_is_withdrawn_for_any_status() {
            assert!(withdrawn_solo(true, Ok(0), DECIDING));
            assert!(withdrawn_solo(true, Ok(1), DECIDING));
            assert!(withdrawn_solo(true, Err(RunError::Cancelled), DECIDING));
        }

        #[proxima::test]
        async fn cancelled_deciding_solo_is_withdrawn() {
            assert!(withdrawn_solo(false, Err(RunError::Cancelled), DECIDING));
        }

        #[proxima::test]
        async fn natural_exit_of_deciding_solo_is_not_withdrawn() {
            assert!(!withdrawn_solo(false, Ok(0), DECIDING));
            assert!(!withdrawn_solo(false, Ok(1), DECIDING));
        }

        #[proxima::test]
        async fn output_solo_is_never_withdrawn_by_the_decision() {
            assert!(!withdrawn_solo(true, Err(RunError::Cancelled), OUTPUT));
            assert!(!withdrawn_solo(false, Err(RunError::Cancelled), OUTPUT));
        }
    }
}

#[cfg(all(test, unix))]
mod pause_tests {
    use core::cell::{Cell, RefCell};
    use core::future::poll_fn;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_runtime::CoreId;

    use proxima_core::signal::Signal;

    use crate::driver::ChildOutput;
    use super::{GROUP_CAPACITY, Ledger, ListingRun, ShardArena, ShardWatch, TestWatch, status_queue};
    use crate::config::{IgnoreMode, NoTestsPolicy, OutputMode, ProcessMode};
    use crate::driver::{DocObservation, Render, RenderState, Settings, TargetTable};
    use crate::line::Line;
    use std::path::PathBuf;
    use crate::edge_exit::fixture::in_single_threaded_process;
    use crate::edge_signal::SignalPipe;
    use crate::edge_spawn::held_launch_storage;
    use crate::outcomes::OutcomeLedger;
    use crate::schedule::SessionSlots;
    use crate::shard_sched::{SchedTable, SpecialPlan};
    use crate::status_queue::{Capture, EVENT_RESERVE, Sinks};
    use crate::stop::StopPolicy;

    const NAMES: [&str; 3] = ["parser::tests::reads_a_header", "parser::tests::rejects_a_bad_tag", "parser::tests::keeps_order"];
    const PIECE: &[u8] = b"test parser::tests::reads_a_header ... ok\ntest parser::tests::rejects_a_bad_tag ... ok\ntest parser::tests::keeps_order ... ok\n";
    const FILLER: [u8; 512] = [b'y'; 512];

    fn open_pipe() -> (OwnedFd, OwnedFd) {
        let mut ends = [-1; 2];
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "pipe is created");
        let pair = unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
        for end in [&pair.0, &pair.1] {
            let flags = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }, 0, "end turns nonblocking");
        }
        pair
    }

    fn redirect_standard_streams() -> std::fs::File {
        let (reader, writer) = open_pipe();
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null").expect("null device opens");
        assert!(unsafe { libc::dup2(writer.as_raw_fd(), 1) } >= 0, "stdout points at the pipe");
        assert!(unsafe { libc::dup2(null.as_raw_fd(), 2) } >= 0, "stderr points at the null device");
        std::fs::File::from(reader)
    }

    fn drained_text(reader: &mut std::fs::File) -> std::string::String {
        let mut bytes = std::vec::Vec::new();
        let mut buffer = [0_u8; 4096];
        while let Ok(count @ 1..) = reader.read(&mut buffer) {
            bytes.extend_from_slice(&buffer[..count]);
        }
        std::string::String::from_utf8(bytes).expect("report lines are UTF-8")
    }

    fn fill_status_queue() {
        while status_queue::room_for(EVENT_RESERVE) {
            status_queue::write(2, &FILLER).expect("filler queues");
        }
    }

    fn settings() -> Settings {
        Settings {
            bin_gate: false,
            debug: false,
            ignore: IgnoreMode::Include,
            filterset: None,
            link_template: None,
            editor: std::vec::Vec::new(),
            probe_ms: 300,
            jobs: 1,
            max_fail: None,
            test_limit_ms: None,
            heartbeat_ms: 10_000,
            source_root: PathBuf::new(),
            no_tests: NoTestsPolicy::Pass,
            process: ProcessMode::Shard,
            output: crate::config::OutputMode::Plain,
            output_why: "flag",
            color: false,
            mouse: None,
            exit_with_parent: crate::config::ExitWithParent::Off,
        }
    }

    fn run_over<'run>(signals: &'run SignalPipe, settings: &'run Settings, (render, names): (&'run Render<'run>, &'run mut [u8])) -> ListingRun<'run> {
        let mut arena = ShardArena::new(names);
        for name in NAMES {
            arena.push(name).expect("name fits the arena");
        }
        arena.seal(1);
        let root = Signal::new();
        ListingRun {
            signals,
            store: std::rc::Rc::new(crate::edge_store::unavailable()),
            reads: RefCell::new(&mut []),
            cursor: Cell::new([0; 5]), head: Cell::new(0),
            filter: "",
            exact: false,
            targets: RefCell::new(TargetTable::new()),
            started_millis: 0,
            harness_arguments: &[],
            environment: &[],
            library_directories: &[],
            inherited_dylib_path: None,
            home: None,
            settings,
            prepared: None,
            docs: DocObservation::new(render),
            policy: RefCell::new(StopPolicy::new(None)),
            doc: RefCell::new(None),
            doc_pending: Cell::new(false),
            launch_storage: RefCell::new(held_launch_storage()),
            ready: Cell::new(None),
                shards: RefCell::new(arena),
            sched: RefCell::new(SchedTable::new(&mut [])),
            specials: RefCell::new(SpecialPlan::new(&mut [])),
            sched_target: Cell::new(None),
            interrupt_reported: Cell::new(0),
            interrupt_not_run: Cell::new(0),
            editing: RefCell::new(None),
            editor_program: RefCell::new(Vec::new()), request: RefCell::new(None), outside: RefCell::new(Vec::new()), detached_running: Cell::new(false), probe: RefCell::new(None), probe_site: RefCell::new((Vec::new(), 0, 0)), probe_output: RefCell::new((Vec::new(), None, false)),
            trip: root.child(),
            root,
        }
    }

    fn shard_watch(run: &ListingRun<'_>) -> TestWatch {
        let shard = run.shards.borrow_mut().carve(0).expect("one shard carves from the sealed names");
        let mut scratch = SessionSlots::<u8, 1>::new();
        TestWatch {
            slot: scratch.insert(0).expect("scratch table has a slot"),
            process_id: 0,
            scope: Signal::new(),
            output: ChildOutput::new(),
            streams: None,
            shard: Some(ShardWatch::new(shard, b"")),
            group: [0; GROUP_CAPACITY],
            group_length: 0,
            started_ms: 0,
            status: None,
            rerun: None,
        }
    }

    fn pauses(watch: &TestWatch) -> usize {
        watch.shard.as_ref().expect("watch is a shard").pauses
    }

    fn seen_once(output: &str) -> bool {
        NAMES.iter().all(|name| output.lines().filter(|line| line.starts_with("PASS") && line.ends_with(*name)).count() == 1)
    }

    #[proxima::test]
    async fn a_full_status_queue_pauses_framing_and_the_drained_remainder_settles_each_test_once() {
        in_single_threaded_process(|| {
            let mut reader = redirect_standard_streams();
            let outcome = run_on_this_thread(CoreId(0), async move {
                let signals = SignalPipe::install().expect("signal pipe installs in the isolated process");
                let settings = settings();
                let mut names = [0_u8; 512];
                let mut arena = [0_u8; 64];
                let mut ledger = [0_u32; 4];
                let mut frame = Line::new();
                let sink: Render<'_> = (OutputMode::Plain, RefCell::new(RenderState::new(&mut arena, &mut ledger)), RefCell::new(&mut frame), RefCell::new(None));
                let run = run_over(&signals, &settings, (&sink, &mut names));
                let mut sessions = SessionSlots::new();
                let mut ledger: Ledger<'_> = OutcomeLedger::new(&mut sessions).expect("table starts empty");
                let mut watch = shard_watch(&run);
                let _capture = Capture::enter();
                fill_status_queue();

                let paused = super::observe(&run, &mut ledger, &mut watch, PIECE).expect("observe pauses without failing");
                let after_pause = (paused.consumed, paused.stop, pauses(&watch), ledger.snapshot().passed);

                let mut sinks = Sinks::new();
                poll_fn(|context| sinks.poll_drain(context)).await.expect("drain completes into the pipe and null device");
                let resumed = super::observe(&run, &mut ledger, &mut watch, &PIECE[paused.consumed..]).expect("observe resumes");
                poll_fn(|context| sinks.poll_drain(context)).await.expect("settled lines drain");
                (after_pause, resumed.consumed, ledger.snapshot().passed, drained_text(&mut reader))
            });
            let ((consumed, stop, pause_count, passed_while_paused), resumed, passed, text) = outcome.expect("root completes");

            assert_eq!(consumed, 0, "a full queue stops framing before the first line");
            assert!(!stop, "a pause is not a stop-policy trip");
            assert_eq!(pause_count, 1, "the pause is counted once");
            assert_eq!(passed_while_paused, 0, "nothing settles while paused");
            assert_eq!(resumed, PIECE.len(), "the drained queue takes the whole piece");
            assert_eq!(passed, 3, "each of the three tests counts once");
            assert!(seen_once(&text), "each result line is reported exactly once: {text}");
        });
    }

    #[proxima::test]
    async fn an_unfilled_status_queue_settles_the_whole_piece_without_pausing() {
        in_single_threaded_process(|| {
            let mut reader = redirect_standard_streams();
            let outcome = run_on_this_thread(CoreId(0), async move {
                let signals = SignalPipe::install().expect("signal pipe installs in the isolated process");
                let settings = settings();
                let mut names = [0_u8; 512];
                let mut arena = [0_u8; 64];
                let mut ledger = [0_u32; 4];
                let mut frame = Line::new();
                let sink: Render<'_> = (OutputMode::Plain, RefCell::new(RenderState::new(&mut arena, &mut ledger)), RefCell::new(&mut frame), RefCell::new(None));
                let run = run_over(&signals, &settings, (&sink, &mut names));
                let mut sessions = SessionSlots::new();
                let mut ledger: Ledger<'_> = OutcomeLedger::new(&mut sessions).expect("table starts empty");
                let mut watch = shard_watch(&run);
                let _capture = Capture::enter();

                let observed = super::observe(&run, &mut ledger, &mut watch, PIECE).expect("observe settles");
                let mut sinks = Sinks::new();
                poll_fn(|context| sinks.poll_drain(context)).await.expect("settled lines drain");
                (observed.consumed, pauses(&watch), ledger.snapshot().passed, drained_text(&mut reader))
            });
            let (consumed, pause_count, passed, text) = outcome.expect("root completes");

            assert_eq!(consumed, PIECE.len(), "the whole piece is consumed");
            assert_eq!(pause_count, 0, "an unfilled queue never pauses");
            assert_eq!(passed, 3, "all three tests count");
            assert!(seen_once(&text), "each result line is reported exactly once: {text}");
        });
    }
}
