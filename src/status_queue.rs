use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll, Waker, ready};
use core::time::Duration;
use std::io;
use std::rc::Rc;

use proxima_core::time::timeout;
use proxima_primitives::pipe::{BoundedQueue, EnqueueOutcome, FailMode, UnpinPipe};

use crate::build_defaults::STATUS_QUEUE_BYTES;
use crate::driver::window::SYNC_WINDOW_BYTES;
use crate::driver::{NAME_CAPACITY, RunError};
use crate::edge_output::{self, OutputSink};
use crate::edge_store::{self, HEADER_BYTES, Store, View};
use crate::edge_stream::CAPTURE_BYTES;
use crate::line::Line;
use crate::summary::ID_CAPACITY;
use crate::view_defaults::FRAME_BYTES;

const RECORD_BYTES: usize = 512;
const SLOTS: usize = STATUS_QUEUE_BYTES / RECORD_BYTES;
// worst case one handled item queues: stdout and stderr captures with their headers and truncation markers.
const HANDLER_FRAMING_BYTES: usize = 256 + 2 * PLAIN_BLOCK_FRAMING_BYTES;
// plain mode frames each captured stream with delimiter lines that carry the binary id and the test name.
const PLAIN_BLOCK_FRAMING_BYTES: usize = NAME_CAPACITY + ID_CAPACITY + 128;
// one gated event: a result line is a name, a label and its header; a failure line travels with its headers.
const REPORT_FRAMING_BYTES: usize = 64 + ID_CAPACITY + 16;
pub(crate) const EVENT_RESERVE: usize = NAME_CAPACITY + REPORT_FRAMING_BYTES;
const LANE_CHUNK: usize = CAPTURE_BYTES as usize;
const CAPTURE_PAIR: usize = 2 * SYNC_WINDOW_BYTES + HANDLER_FRAMING_BYTES;
const UNGATED_BYTES: usize = if LANE_CHUNK > CAPTURE_PAIR { LANE_CHUNK } else { CAPTURE_PAIR };
// small writes each take a whole record, so an item also reserves a count of writes beyond its byte volume.
const WRITES_PER_ITEM: usize = 16;
// every unbounded writer is gated per event, so one item queues at most one lane chunk or capture pair ungated, plus the event the gate admitted.
const ITEM_RECORDS: usize = (UNGATED_BYTES + EVENT_RESERVE).div_ceil(RECORD_BYTES) + WRITES_PER_ITEM;

// the longest the run waits on a reader that stopped reading before it abandons the queue: a hung reader must not hang the process.
const FLUSH_BOUND: Duration = Duration::from_secs(2);

const _: () = assert!(SLOTS >= ITEM_RECORDS, "status queue must hold one item");

// a block page frame is written whole into the inline lane; two frames of room keep the drain fed while the next page is built.
const FRAME_RECORDS: usize = FRAME_BYTES.div_ceil(RECORD_BYTES) + 1;
const INLINE_SLOTS: usize = 2 * FRAME_RECORDS;

/// Bytes the queue absorbs from an unread descriptor before `has_room` turns false and the run pauses; the reserve of one worst-case item stays free.
pub const UNPAUSED_QUEUE_BYTES: usize = (SLOTS - ITEM_RECORDS) * RECORD_BYTES;

/// One queued write fragment: the descriptor it belongs to and up to `RECORD_BYTES` of its bytes.
#[derive(Clone, Copy)]
struct Record {
    descriptor: u8,
    length: usize,
    bytes: [u8; RECORD_BYTES],
}

impl Record {
    const fn empty(descriptor: u8) -> Self {
        Self { descriptor, length: 0, bytes: [0; RECORD_BYTES] }
    }

    fn fill(&mut self, fragment: &[u8]) -> usize {
        let taken = fragment.len().min(RECORD_BYTES - self.length);
        self.bytes[self.length..self.length + taken].copy_from_slice(&fragment[..taken]);
        self.length += taken;
        taken
    }

    const fn is_full(&self) -> bool {
        self.length == RECORD_BYTES
    }

    fn line(&self) -> Line<RECORD_BYTES> {
        let mut line = Line::new();
        line.text(&self.bytes[..self.length]);
        line
    }
}

// the tail is the record still being packed by consecutive writes to one descriptor; the ring holds only sealed records, older than it.
//
// a pending failure block is a barrier in the ring: `pushed` and `taken` count the records ever sealed and ever delivered, and the
// ring delivers nothing past the barrier position until the block's pages, written into `inline` as the screen catches up, have
// been delivered ahead of it. Every later write queues in the ring behind the barrier, so scrollback order is the append order
// and nothing that follows a block can print before it, while no producer waits on the screen.
struct Queue {
    records: BoundedQueue<Record>,
    inline: BoundedQueue<Record>,
    tail: RefCell<Option<Record>>,
    pushed: Cell<u64>,
    taken: Cell<u64>,
    barrier: Cell<Option<u64>>,
    inserting: Cell<bool>,
    capturing: Cell<bool>,
    deferred: Cell<bool>,
    abandoned: Cell<usize>,
    // the overflow: scrollback the ring has no room for is sealed into this chain of the run store and read back in order
    spill: RefCell<Option<Rc<Store>>>,
    spilled: Cell<View>,
    spill_read: Cell<(u64, u64)>,
    unread: Cell<u64>,
    spill_ok: Cell<bool>,
    paused_noticed: Cell<bool>,
    skipped: Cell<u64>,
}

std::thread_local! {
    // allocated once, on first use by the pump thread; FailClosed so a status line is never silently dropped.
    static QUEUE: Queue = Queue {
        records: BoundedQueue::new(SLOTS, FailMode::FailClosed),
        inline: BoundedQueue::new(INLINE_SLOTS, FailMode::FailClosed),
        tail: RefCell::new(None),
        pushed: Cell::new(0),
        taken: Cell::new(0),
        barrier: Cell::new(None),
        inserting: Cell::new(false),
        capturing: Cell::new(false),
        deferred: Cell::new(false),
        abandoned: Cell::new(0),
        spill: RefCell::new(None),
        spilled: Cell::new((SCROLLBACK_STREAM, 0, 0, 0)),
        spill_read: Cell::new((0, 0)),
        unread: Cell::new(0),
        spill_ok: Cell::new(false),
        paused_noticed: Cell::new(false),
        skipped: Cell::new(0),
    };
}

/// The store stream key the scrollback overflow is sealed under; session streams count up from zero, so this never meets one.
pub(crate) const SCROLLBACK_STREAM: u32 = u32::MAX;

// one overflow page is the descriptor byte then up to this many bytes of one record
const PAGE_TEXT_BYTES: usize = RECORD_BYTES - 1;
const PAUSE_NOTICE: &[u8] = b"cargo-val: warn: the screen is behind and the run store is unavailable, so tests pause until it catches up\n";

/// Makes `store` the overflow of the scrollback: from now on a write the ring has no room for is sealed into it instead of pausing the
/// lanes. A memory-only store keeps the ring's pause, which says so once.
pub(crate) fn spill_into(store: Rc<Store>) {
    QUEUE.with(|queue| {
        queue.spill_ok.set(store.0.is_some());
        *queue.spill.borrow_mut() = Some(store);
        queue.spilled.set((SCROLLBACK_STREAM, 0, 0, 0));
        queue.spill_read.set((0, 0));
        queue.unread.set(0);
    });
}

/// How many scrollback pages the overflow holds that the screen has not been given yet.
#[cfg(test)]
pub(crate) fn unread_pages() -> u64 {
    QUEUE.with(|queue| queue.unread.get())
}

/// Pump turns that skipped the lanes because the queue had no room; zero whenever the store holds the overflow.
pub(crate) fn skipped_turns() -> u64 {
    QUEUE.with(|queue| queue.skipped.get())
}

/// Counts a pump turn that did not read the lanes.
pub(crate) fn note_skipped() {
    QUEUE.with(|queue| queue.skipped.set(queue.skipped.get() + 1));
}

fn spill_usable(queue: &Queue) -> bool {
    queue.spill_ok.get() && queue.spill.borrow().as_ref().is_some_and(|store| store.0.is_some())
}

// a memory-only run can only pause; it says so once, straight to stderr, because the ring is full and an unread screen would hold a queued notice back
fn note_pause(queue: &Queue) {
    if queue.paused_noticed.get() {
        return;
    }
    let mut line = Line::<RECORD_BYTES>::new();
    line.text(PAUSE_NOTICE);
    queue.paused_noticed.set(try_stderr(line));
}

#[cfg(test)]
pub(crate) fn pause_noticed() -> bool {
    QUEUE.with(|queue| queue.paused_noticed.get())
}

fn records_for(bytes: usize) -> usize {
    bytes.div_ceil(RECORD_BYTES)
}

fn free_slots(queue: &Queue) -> usize {
    queue.records.capacity() - queue.records.len()
}

// one slot beyond the fragments covers sealing a partly packed tail when the descriptor changes
fn can_take(queue: &Queue, bytes: usize) -> bool {
    free_slots(queue) > records_for(bytes)
}

fn seal(queue: &Queue, tail: &mut Option<Record>) -> bool {
    tail.take().is_none_or(|record| {
        let sealed = queue.records.enqueue(record) == EnqueueOutcome::Enqueued;
        queue.pushed.set(queue.pushed.get() + u64::from(sealed));
        sealed
    })
}

fn at_barrier(queue: &Queue) -> bool {
    queue.barrier.get().is_some_and(|position| queue.taken.get() >= position)
}

// inline pages come first: they are only written once the ring ahead of their barrier has been delivered
fn next_record(queue: &Queue) -> Option<Record> {
    if let Some(page) = queue.inline.dequeue() {
        return Some(page);
    }
    if at_barrier(queue) {
        return None;
    }
    let record = queue
        .records
        .dequeue()
        .or_else(|| {
            let packed = queue.tail.borrow_mut().take();
            queue.pushed.set(queue.pushed.get() + u64::from(packed.is_some()));
            packed
        })
        .or_else(|| read_spill(queue))?;
    queue.taken.set(queue.taken.get() + 1);
    Some(record)
}

// the overflow is older than nothing in the ring and newer than all of it, so it is read only once the ring and its tail are empty;
// a page the store cannot give back is counted abandoned and stands for the rest of the chain, so the barrier arithmetic still closes
fn read_spill(queue: &Queue) -> Option<Record> {
    let owed = queue.unread.get();
    if owed == 0 {
        return None;
    }
    let store = queue.spill.borrow().clone()?;
    let (previous, cursor) = queue.spill_read.get();
    let start = if previous == 0 { queue.spilled.get().1 } else { cursor };
    let mut page = [0_u8; RECORD_BYTES];
    match edge_store::next_page(&store, (previous, start), &mut page) {
        Ok((offset, length)) if length > 0 => {
            queue.unread.set(owed - 1);
            queue.spill_read.set((offset, offset + (HEADER_BYTES + length) as u64));
            let mut record = Record::empty(page[0]);
            record.fill(&page[1..length]);
            Some(record)
        }
        _ => {
            queue.unread.set(0);
            queue.taken.set(queue.taken.get() + owed);
            queue.abandoned.set(queue.abandoned.get() + 1);
            None
        }
    }
}

/// Scope inside which [`write`] queues instead of writing; the whole run holds one, the pump drains through [`Sinks`] after every
/// item, and [`flush_bounded`] drains what the setup and summary reports queued between pumps.
///
/// A caller with no scope, or an append the queue refuses, gets an error: nothing here ever falls back to a blocking write.
pub(crate) struct Capture {
    previous: bool,
}

impl Capture {
    pub(crate) fn enter() -> Self {
        let previous = QUEUE.with(|queue| queue.capturing.replace(true));
        Self { previous }
    }
}

// `flush_bounded` already ran on every exit path; what is still queued here was abandoned, so it is counted and reported, never written blocking.
impl Drop for Capture {
    fn drop(&mut self) {
        QUEUE.with(|queue| queue.capturing.set(self.previous));
        if !self.previous {
            QUEUE.with(|queue| queue.barrier.set(None));
            let queued = core::iter::from_fn(|| QUEUE.with(next_record)).count();
            let abandoned = QUEUE.with(|queue| queue.abandoned.replace(0));
            notify_abandoned(queued + abandoned);
        }
    }
}

// one nonblocking attempt: a full descriptor parks the sink, the line is dropped with it, and EAGAIN costs nothing but this notice
fn notify_abandoned(records: usize) {
    if records == 0 {
        return;
    }
    let mut line = Line::<RECORD_BYTES>::new();
    line.text(b"cargo-val: abandoned ");
    line.number(records);
    line.text(b" status records: the output reader stopped reading\n");
    let _ = try_stderr(line);
}

fn try_stderr(line: Line<RECORD_BYTES>) -> bool {
    let Ok(sink) = edge_output::standard(2) else {
        return false;
    };
    let mut call = UnpinPipe::call(&sink, line);
    Pin::new(&mut call).poll(&mut Context::from_waker(Waker::noop())).is_ready()
}

/// Whether the queue can take `bytes` more as records; the per-event gate of every writer that can emit an unbounded amount.
///
/// Always true while the run store holds the overflow: a writer the ring cannot take is never asked to wait for the screen.
pub(crate) fn room_for(bytes: usize) -> bool {
    QUEUE.with(|queue| {
        let room = spill_usable(queue) || can_take(queue, bytes);
        if !room {
            note_pause(queue);
        }
        room
    })
}

/// Whether the queue can take the worst-case output of the next handled item; true whenever the run store holds the overflow.
pub(crate) fn has_room() -> bool {
    QUEUE.with(|queue| {
        let room = spill_usable(queue) || free_slots(queue) >= ITEM_RECORDS;
        if !room {
            note_pause(queue);
        }
        room
    })
}

/// Seals the packed tail and returns the position every later append queues behind; the position a block holds the ring at.
pub(crate) fn mark() -> u64 {
    QUEUE.with(|queue| {
        let mut tail = queue.tail.borrow_mut();
        let _ = seal(queue, &mut tail);
        queue.pushed.get()
    })
}

/// Holds the ring at `position` (a [`mark`]) until the pages of the block waiting there are delivered; `None` releases it.
pub(crate) fn hold(position: Option<u64>) {
    QUEUE.with(|queue| queue.barrier.set(position));
}

/// Whether the ring ahead of the held position is delivered and the inline lane has room for another page frame.
pub(crate) fn block_ready() -> bool {
    QUEUE.with(|queue| at_barrier(queue) && queue.inline.capacity() - queue.inline.len() >= FRAME_RECORDS)
}

/// Runs `write` with every [`write`] it makes queued in the inline lane, delivered ahead of the ring records held behind the barrier.
pub(crate) fn insert<Output>(write: impl FnOnce() -> Output) -> Output {
    QUEUE.with(|queue| queue.inserting.set(true));
    let output = write();
    QUEUE.with(|queue| queue.inserting.set(false));
    output
}

/// Marks that a writer paused for lack of room; its backlog waits for [`resume_due`].
pub(crate) fn defer() {
    QUEUE.with(|queue| queue.deferred.set(true));
}

/// True once, when a drain has freed room for a paused backlog; the pump then delivers the resume path.
pub(crate) fn resume_due() -> bool {
    QUEUE.with(|queue| {
        let due = queue.deferred.get() && free_slots(queue) >= ITEM_RECORDS;
        queue.deferred.set(queue.deferred.get() && !due);
        due
    })
}

/// Delivers the queue through the readiness-gated sinks for at most `FLUSH_BOUND`. A reader that leaves it unread abandons the
/// remainder: the abandonment is counted for the closing [`Capture`] to report, and the run goes on instead of parking on the gate.
///
/// # Errors
/// Returns the sink's failure to write; the failed line counts as abandoned too.
pub(crate) async fn drain_bounded(sinks: &mut Sinks) -> Result<(), RunError> {
    if QUEUE.with(|queue| queue.abandoned.get()) > 0 {
        return Ok(());
    }
    let delivered = timeout(FLUSH_BOUND, poll_fn(|context| sinks.poll_drain(context))).await;
    match delivered {
        Ok(Ok(())) => Ok(()),
        // the line a sink was mid-way through is gone with the sink, so it counts with the unqueued records
        Ok(Err(error)) => {
            abandon();
            Err(error)
        }
        Err(_) => {
            abandon();
            Ok(())
        }
    }
}

fn abandon() {
    QUEUE.with(|queue| queue.abandoned.set(queue.abandoned.get() + 1));
}

/// The last drain of a run, on every exit path; a failure to write is already counted as abandoned and is not the run's error.
pub(crate) async fn flush_bounded() {
    hold(None);
    let _ = drain_bounded(&mut Sinks::new()).await;
}

/// Queues `bytes` for descriptor 1 or 2.
///
/// # Errors
/// [`RunError::StatusQueue`] when no [`Capture`] scope is open or the queue refuses the append. Every burst writer gates on [`room_for`]
/// or [`has_room`] and [`defer`]s first, so a refusal means a writer skipped its gate: the queue counts it as a drop and the run ends, never silently losing output or writing blocking.
pub(crate) fn write(descriptor: i32, bytes: &[u8]) -> Result<(), RunError> {
    let appended = QUEUE.with(|queue| queue.capturing.get() && if queue.inserting.get() { append_inline(queue, descriptor, bytes) } else { append(queue, descriptor, bytes) });
    if appended { Ok(()) } else { Err(RunError::StatusQueue) }
}

// a block page frame is its own records: it never packs into the ring's tail, which belongs to the writes queued behind the barrier
fn append_inline(queue: &Queue, descriptor: i32, bytes: &[u8]) -> bool {
    let Ok(tag) = u8::try_from(descriptor) else {
        return false;
    };
    if queue.inline.capacity() - queue.inline.len() < records_for(bytes.len()) {
        queue.inline.note_drop();
        return false;
    }
    bytes.chunks(RECORD_BYTES).all(|chunk| {
        let mut record = Record::empty(tag);
        record.fill(chunk);
        queue.inline.enqueue(record) == EnqueueOutcome::Enqueued
    })
}

// once anything is in the overflow every later write joins it until the screen has been given all of it, so append order is delivery order
fn append_spill(queue: &Queue, tag: u8, bytes: &[u8]) -> bool {
    if !seal(queue, &mut queue.tail.borrow_mut()) {
        return false;
    }
    let Some(store) = queue.spill.borrow().clone() else {
        return false;
    };
    bytes.chunks(PAGE_TEXT_BYTES).all(|chunk| {
        let mut page = [0_u8; RECORD_BYTES];
        page[0] = tag;
        page[1..=chunk.len()].copy_from_slice(chunk);
        match edge_store::append(&store, queue.spilled.get(), &page[..=chunk.len()]) {
            Ok(view) => {
                queue.spilled.set(view);
                queue.pushed.set(queue.pushed.get() + 1);
                queue.unread.set(queue.unread.get() + 1);
                true
            }
            Err(error) => {
                edge_store::lose(&store, chunk.len(), &error);
                queue.spill_ok.set(false);
                false
            }
        }
    })
}

fn append(queue: &Queue, descriptor: i32, bytes: &[u8]) -> bool {
    let Ok(tag) = u8::try_from(descriptor) else {
        return false;
    };
    if spill_usable(queue) {
        // the ring keeps one slot beyond the fragments so sealing the packed tail when the overflow begins cannot fail
        if queue.unread.get() != 0 || free_slots(queue) <= records_for(bytes.len()) + 1 {
            return append_spill(queue, tag, bytes);
        }
    } else if queue.unread.get() != 0 || !can_take(queue, bytes.len()) {
        queue.records.note_drop();
        return false;
    }
    let mut tail = queue.tail.borrow_mut();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        if tail.as_ref().is_some_and(|record| record.descriptor != tag) && !seal(queue, &mut tail) {
            return false;
        }
        let record = tail.get_or_insert_with(|| Record::empty(tag));
        remaining = &remaining[record.fill(remaining)..];
        if record.is_full() && !seal(queue, &mut tail) {
            return false;
        }
    }
    true
}

/// Standard output and standard error as [`OutputSink`] pipes, opened on the first byte each receives.
pub(crate) struct Sinks {
    standard: [Option<OutputSink>; 2],
}

impl Sinks {
    pub(crate) const fn new() -> Self {
        Self { standard: [None, None] }
    }

    /// Writes queued status records, in append order, as `Line<512>` steps through the sinks, until a full descriptor parks it on its writable gate.
    ///
    /// A source of `Record` (the ring dequeue) into a sink of `Line` (the `OutputSink` pipe), the shape of the telemetry `Drainer`.
    /// Returns `Pending` with the unwritten tail still queued, so the caller keeps polling its other arms and appending; a line a
    /// sink already took stays in that sink and is finished before the next one, so dropping the poll loses nothing.
    pub(crate) fn poll_drain(&mut self, context: &mut Context<'_>) -> Poll<Result<(), RunError>> {
        for sink in self.standard.iter().flatten() {
            ready!(poll_line(sink, Line::new(), context))?;
        }
        while let Some(record) = QUEUE.with(next_record) {
            let sink = self.sink(i32::from(record.descriptor))?;
            ready!(poll_line(sink, record.line(), context))?;
        }
        Poll::Ready(Ok(()))
    }

    fn sink(&mut self, descriptor: i32) -> Result<&OutputSink, RunError> {
        let slot = self
            .standard
            .get_mut(usize::try_from(descriptor - 1).map_err(|_| RunError::Reactor)?)
            .ok_or(RunError::Reactor)?;
        if slot.is_none() {
            *slot = Some(edge_output::standard(descriptor).map_err(|_: io::Error| RunError::Reactor)?);
        }
        slot.as_ref().ok_or(RunError::Reactor)
    }
}

fn poll_line(sink: &OutputSink, line: Line<RECORD_BYTES>, context: &mut Context<'_>) -> Poll<Result<(), RunError>> {
    let mut call = UnpinPipe::call(sink, line);
    Pin::new(&mut call).poll(context).map_err(|_| RunError::Reactor)
}

#[cfg(all(test, unix))]
mod tests {
    use core::future::poll_fn;
    use core::task::Poll;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use prime::os::core_shard::run_on_this_thread;
    use proxima_runtime::CoreId;

    use std::rc::Rc;

    use super::{Capture, Sinks, write};
    use crate::edge_store::{self, Store};
    use crate::edge_exit::fixture::in_single_threaded_process;

    const STATUS_LINE: &[u8] = b"cargo-val: 12 passed; 0 failed; 3 ignored; finished in 4.21s\n";
    const LATE_LINE: &[u8] = b"cargo-val: total: discovered 3000, passed 3000, failed 0\n";
    const FILL_CHUNK: [u8; 4096] = [b'x'; 4096];
    const STDERR: i32 = 2;

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut ends = [-1; 2];
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "pipe is created");
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
    }

    fn set_nonblocking(descriptor: &OwnedFd) {
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        let status = unsafe {
            libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
        };
        assert_eq!(status, 0, "descriptor turns nonblocking");
    }

    fn fill_until_full(writer: &OwnedFd) {
        while unsafe { libc::write(writer.as_raw_fd(), FILL_CHUNK.as_ptr().cast(), FILL_CHUNK.len()) }
            >= 0
        {}
    }

    fn drain_reader(reader: &mut std::fs::File) -> std::vec::Vec<u8> {
        let mut drained = std::vec::Vec::new();
        let mut buffer = [0_u8; 4096];
        while let Ok(count @ 1..) = reader.read(&mut buffer) {
            drained.extend_from_slice(&buffer[..count]);
        }
        drained
    }

    #[proxima::test]
    async fn drain_parks_on_a_full_stderr_and_completes_after_the_reader_drains() {
        in_single_threaded_process(|| {
            let (reader, writer) = pipe_pair();
            set_nonblocking(&reader);
            set_nonblocking(&writer);
            fill_until_full(&writer);
            let saved_stderr = unsafe { libc::fcntl(STDERR, libc::F_DUPFD_CLOEXEC, 0) };
            assert!(saved_stderr >= 0, "stderr is duplicated for restore");
            assert!(unsafe { libc::dup2(writer.as_raw_fd(), STDERR) } >= 0, "stderr points at the full pipe");

            let outcome = run_on_this_thread(CoreId(0), async move {
                let _capture = Capture::enter();
                write(STDERR, STATUS_LINE).expect("status line queues");
                let mut sinks = Sinks::new();
                let first = poll_fn(|context| Poll::Ready(sinks.poll_drain(context))).await;
                let parked = first.is_pending();
                let queued_while_parked = write(STDERR, LATE_LINE).is_ok();
                let mut reader = std::fs::File::from(reader);
                let blocked = drain_reader(&mut reader);
                let completed = poll_fn(|context| sinks.poll_drain(context)).await;
                (parked, queued_while_parked, blocked.len(), completed.is_ok(), drain_reader(&mut reader))
            });
            unsafe {
                libc::dup2(saved_stderr, STDERR);
                libc::close(saved_stderr);
            }
            let (parked, queued_while_parked, blocked, completed, delivered) = outcome.expect("root completes");

            assert!(parked, "a full descriptor parks the drain instead of blocking the thread");
            assert!(queued_while_parked, "status lines keep queueing while the descriptor is full");
            assert!(blocked > 0, "the reader drained the filled bytes");
            assert!(completed, "the drain completes once the descriptor is writable");
            assert_eq!(
                delivered,
                [STATUS_LINE, LATE_LINE].concat(),
                "both queued status lines arrive whole, in append order"
            );
        });
    }

    fn delivered() -> Option<std::string::String> {
        super::QUEUE.with(super::next_record).map(|record| std::string::String::from_utf8_lossy(&record.bytes[..record.length]).into_owned())
    }

    #[test]
    fn a_held_block_page_prints_after_the_records_ahead_of_it_and_before_the_records_behind_it() {
        let _capture = Capture::enter();
        write(1, b"settle a\n").expect("the line ahead of the block queues");
        let position = super::mark();
        super::hold(Some(position));
        write(1, b"settle b\n").expect("the line behind the block queues");

        let ahead = delivered();
        let held = delivered();
        let ready = super::block_ready();
        super::insert(|| write(1, b"page 1\n")).expect("the page queues inline");
        let page = delivered();
        let still_held = delivered();
        super::hold(None);
        let behind = delivered();

        assert_eq!(ahead.as_deref(), Some("settle a\n"));
        assert_eq!(held, None, "nothing behind the barrier is delivered while the block waits");
        assert!(ready, "the ring ahead is delivered, so the block may write its page");
        assert_eq!(page.as_deref(), Some("page 1\n"), "the page comes before the later line");
        assert_eq!(still_held, None, "the later line still waits for the rest of the block");
        assert_eq!(behind.as_deref(), Some("settle b\n"), "the later line prints once the block releases the ring");
    }

    fn settle_line(index: usize) -> std::string::String {
        std::format!("PASS     views_pair  fixture::settle_{index:04}  0.003s\n")
    }

    fn remaining() -> std::string::String {
        core::iter::from_fn(delivered).collect()
    }

    fn store_in(directory: &tempfile::TempDir) -> Rc<Store> {
        Rc::new(edge_store::open(Some(directory.path())))
    }

    #[test]
    fn two_thousand_settle_lines_behind_a_held_block_overflow_to_the_store_and_print_in_order_after_the_block() {
        let directory = tempfile::tempdir().expect("temporary target");
        super::spill_into(store_in(&directory));
        let _capture = Capture::enter();
        write(1, b"START    views_pair  fixture::fails_big\n").expect("the line ahead of the block queues");
        super::hold(Some(super::mark()));

        let always_room = (0..2000).all(|index| {
            write(1, settle_line(index).as_bytes()).expect("a settle line queues behind the block");
            super::has_room() && super::room_for(super::EVENT_RESERVE)
        });
        let ahead = delivered();
        let held = delivered();
        super::insert(|| write(1, b"block page\n")).expect("the block page queues inline");
        let page = delivered();
        let overflow = super::unread_pages();
        super::hold(None);
        let behind = remaining();

        assert!(always_room, "no writer was ever told to wait for the screen");
        assert!(overflow > 0, "the ring held 64 KiB and the lines are more than that, so the store holds the rest: {overflow} pages");
        assert_eq!(ahead.as_deref(), Some("START    views_pair  fixture::fails_big\n"));
        assert_eq!(held, None, "nothing behind the barrier is delivered while the block waits");
        assert_eq!(page.as_deref(), Some("block page\n"), "the block page comes before every settle line");
        assert_eq!(behind, (0..2000).map(settle_line).collect::<std::string::String>(), "every settle line prints once, in the order it settled");
        assert_eq!(super::unread_pages(), 0, "the overflow was handed over whole");
        assert_eq!(super::skipped_turns(), 0, "no pump turn was recorded as skipping the lanes");
    }

    #[test]
    fn writes_return_to_the_ring_once_the_overflow_has_been_delivered() {
        let directory = tempfile::tempdir().expect("temporary target");
        super::spill_into(store_in(&directory));
        let _capture = Capture::enter();
        super::hold(Some(super::mark()));
        (0..1500).for_each(|index| write(1, settle_line(index).as_bytes()).expect("queues"));
        super::hold(None);
        assert!(super::unread_pages() > 0, "the overflow holds pages");
        let first_pass = remaining();

        write(1, b"after the overflow\n").expect("queues");

        assert_eq!(first_pass, (0..1500).map(settle_line).collect::<std::string::String>());
        assert_eq!(super::unread_pages(), 0, "a line after the drain joins the ring, not the store");
        assert_eq!(delivered().as_deref(), Some("after the overflow\n"));
    }

    #[test]
    fn a_memory_only_run_pauses_with_one_notice_and_loses_no_line() {
        super::spill_into(Rc::new(edge_store::unavailable()));
        let _capture = Capture::enter();
        super::hold(Some(super::mark()));
        let mut written = 0;
        while super::has_room() {
            write(1, settle_line(written).as_bytes()).expect("a line the gate admitted queues");
            written += 1;
        }
        let paused = (super::has_room(), super::has_room());
        super::hold(None);

        let text = remaining();

        assert_eq!(paused, (false, false), "with nowhere to put the lines the lanes pause");
        assert!(super::pause_noticed(), "the pause was said once on stderr, however many turns it lasts");
        assert_eq!(text, (0..written).map(settle_line).collect::<std::string::String>(), "every admitted line printed, in order, with the notice kept out of the scrollback");
    }
}
