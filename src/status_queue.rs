use core::cell::{Cell, RefCell};
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

use proxima_primitives::pipe::{BoundedQueue, EnqueueOutcome, FailMode, UnpinPipe};

use crate::build_defaults::STATUS_QUEUE_BYTES;
use crate::driver::{CAPTURE_CAPACITY, NAME_CAPACITY, RunError};
use crate::edge_output::{self, OutputSink};
use crate::edge_stream::CAPTURE_BYTES;
use crate::line::Line;
use crate::summary::ID_CAPACITY;

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
const CAPTURE_PAIR: usize = 2 * CAPTURE_CAPACITY + HANDLER_FRAMING_BYTES;
const UNGATED_BYTES: usize = if LANE_CHUNK > CAPTURE_PAIR { LANE_CHUNK } else { CAPTURE_PAIR };
// small writes each take a whole record, so an item also reserves a count of writes beyond its byte volume.
const WRITES_PER_ITEM: usize = 16;
// every unbounded writer is gated per event, so one item queues at most one lane chunk or capture pair ungated, plus the event the gate admitted.
const ITEM_RECORDS: usize = (UNGATED_BYTES + EVENT_RESERVE).div_ceil(RECORD_BYTES) + WRITES_PER_ITEM;

const _: () = assert!(SLOTS >= ITEM_RECORDS, "status queue must hold one item");

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
struct Queue {
    records: BoundedQueue<Record>,
    tail: RefCell<Option<Record>>,
    capturing: Cell<bool>,
    deferred: Cell<bool>,
}

std::thread_local! {
    // allocated once, on first use by the pump thread; FailClosed so a status line is never silently dropped.
    static QUEUE: Queue = Queue {
        records: BoundedQueue::new(SLOTS, FailMode::FailClosed),
        tail: RefCell::new(None),
        capturing: Cell::new(false),
        deferred: Cell::new(false),
    };
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
    tail.take().is_none_or(|record| queue.records.enqueue(record) == EnqueueOutcome::Enqueued)
}

fn next_record(queue: &Queue) -> Option<Record> {
    queue.records.dequeue().or_else(|| queue.tail.borrow_mut().take())
}

/// Scope inside which [`write`] queues instead of writing; the async pump holds one and drains through [`Sinks`] after every item.
///
/// Callers outside a scope (the setup and summary reports that run between pumps) keep the plain
/// synchronous [`edge_output::write`]. The banner and help paths in `main` never reach this module.
pub(crate) struct Capture {
    previous: bool,
}

impl Capture {
    pub(crate) fn enter() -> Self {
        let previous = QUEUE.with(|queue| queue.capturing.replace(true));
        Self { previous }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        QUEUE.with(|queue| queue.capturing.set(self.previous));
    }
}

/// Whether the queue can take `bytes` more as records; the per-event gate of every writer that can emit an unbounded amount.
pub(crate) fn room_for(bytes: usize) -> bool {
    QUEUE.with(|queue| can_take(queue, bytes))
}

/// Whether the queue can take the worst-case output of the next handled item.
pub(crate) fn has_room() -> bool {
    QUEUE.with(|queue| free_slots(queue) >= ITEM_RECORDS)
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

pub(crate) fn write(descriptor: i32, bytes: &[u8]) -> Result<(), RunError> {
    if QUEUE.with(|queue| queue.capturing.get() && append(queue, descriptor, bytes)) {
        return Ok(());
    }
    edge_output::write(descriptor, bytes).map_err(|_| RunError::Reactor)
}

// the per-event gates make a refusal unreachable; the caller then writes synchronously rather than lose the bytes
fn append(queue: &Queue, descriptor: i32, bytes: &[u8]) -> bool {
    let Ok(tag) = u8::try_from(descriptor) else {
        return false;
    };
    if !can_take(queue, bytes.len()) {
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

    use super::{Capture, Sinks, write};
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
}
