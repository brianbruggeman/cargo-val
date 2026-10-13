use core::cell::RefCell;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

use proxima_primitives::pipe::UnpinPipe;

use crate::build_defaults::STATUS_QUEUE_BYTES;
use crate::driver::{CAPTURE_CAPACITY, NAME_CAPACITY, RunError};
use crate::edge_output::{self, OutputSink};
use crate::edge_stream::CAPTURE_BYTES;
use crate::line::Line;

const CHUNK: usize = 512;
const HEADER: usize = 5;
// worst case one handled item queues: stdout and stderr captures with their headers and truncation markers.
const HANDLER_FRAMING_BYTES: usize = 256;
// one gated event: a result line is a name, a label and its header; a failure line travels with its headers.
const REPORT_FRAMING_BYTES: usize = 64;
pub(crate) const EVENT_RESERVE: usize = HEADER + NAME_CAPACITY + REPORT_FRAMING_BYTES;
const LANE_CHUNK: usize = CAPTURE_BYTES as usize;
const CAPTURE_PAIR: usize = 2 * CAPTURE_CAPACITY + HANDLER_FRAMING_BYTES;
const UNGATED_BYTES: usize = if LANE_CHUNK > CAPTURE_PAIR { LANE_CHUNK } else { CAPTURE_PAIR };
// every unbounded writer is gated per event, so one item queues at most one lane chunk or capture pair ungated, plus the event the gate admitted.
const ITEM_RESERVE: usize = HEADER + UNGATED_BYTES + EVENT_RESERVE;

const _: () = assert!(STATUS_QUEUE_BYTES >= ITEM_RESERVE, "status queue must hold one item");

struct Queue {
    bytes: [u8; STATUS_QUEUE_BYTES],
    read: usize,
    length: usize,
    capturing: bool,
    deferred: bool,
}

std::thread_local! {
    static QUEUE: RefCell<Queue> = const {
        RefCell::new(Queue { bytes: [0; STATUS_QUEUE_BYTES], read: 0, length: 0, capturing: false, deferred: false })
    };
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
        let previous = QUEUE.with_borrow_mut(|queue| core::mem::replace(&mut queue.capturing, true));
        Self { previous }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        QUEUE.with_borrow_mut(|queue| queue.capturing = self.previous);
    }
}

/// Whether the queue can take `bytes` more, with their header; the per-event gate of every writer that can emit an unbounded amount.
pub(crate) fn room_for(bytes: usize) -> bool {
    QUEUE.with_borrow(|queue| STATUS_QUEUE_BYTES - (queue.length - queue.read) >= HEADER + bytes)
}

/// Whether the queue can take the worst-case output of the next handled item.
pub(crate) fn has_room() -> bool {
    room_for(ITEM_RESERVE - HEADER)
}

/// Marks that a writer paused for lack of room; its backlog waits for [`resume_due`].
pub(crate) fn defer() {
    QUEUE.with_borrow_mut(|queue| queue.deferred = true);
}

/// True once, when a drain has freed room for a paused backlog; the pump then delivers the resume path.
pub(crate) fn resume_due() -> bool {
    QUEUE.with_borrow_mut(|queue| {
        let due = queue.deferred && STATUS_QUEUE_BYTES - (queue.length - queue.read) >= ITEM_RESERVE;
        queue.deferred &= !due;
        due
    })
}

pub(crate) fn write(descriptor: i32, bytes: &[u8]) -> Result<(), RunError> {
    let queued = QUEUE.with_borrow_mut(|queue| queue.capturing && append(queue, descriptor, bytes));
    if queued {
        return Ok(());
    }
    edge_output::write(descriptor, bytes).map_err(|_| RunError::Reactor)
}

// the per-event gates make a refusal unreachable; the caller then writes synchronously rather than lose the bytes
fn append(queue: &mut Queue, descriptor: i32, bytes: &[u8]) -> bool {
    let (Ok(length), Ok(tag)) = (u32::try_from(bytes.len()), u8::try_from(descriptor)) else {
        return false;
    };
    if queue.length + HEADER + bytes.len() > STATUS_QUEUE_BYTES {
        compact(queue);
    }
    let end = queue.length + HEADER + bytes.len();
    if end > STATUS_QUEUE_BYTES {
        return false;
    }
    let start = queue.length;
    queue.bytes[start] = tag;
    queue.bytes[start + 1..start + HEADER].copy_from_slice(&length.to_le_bytes());
    queue.bytes[start + HEADER..end].copy_from_slice(bytes);
    queue.length = end;
    true
}

fn compact(queue: &mut Queue) {
    queue.bytes.copy_within(queue.read..queue.length, 0);
    queue.length -= queue.read;
    queue.read = 0;
}

/// Standard output and standard error as [`OutputSink`] pipes, opened on the first byte each receives.
pub(crate) struct Sinks {
    standard: [Option<OutputSink>; 2],
}

impl Sinks {
    pub(crate) const fn new() -> Self {
        Self { standard: [None, None] }
    }

    /// Writes queued status output, in append order, as `Line<512>` steps through the sinks, until a full descriptor parks it on its writable gate.
    ///
    /// Returns `Pending` with the unwritten tail still queued, so the caller keeps polling its other arms and appending; a line a
    /// sink already took stays in that sink and is finished before the next one, so dropping the poll loses nothing.
    pub(crate) fn poll_drain(&mut self, context: &mut Context<'_>) -> Poll<Result<(), RunError>> {
        for sink in self.standard.iter().flatten() {
            ready!(poll_line(sink, Line::new(), context))?;
        }
        while let Some((descriptor, chunk)) = next_chunk() {
            let sink = self.sink(descriptor)?;
            ready!(poll_line(sink, chunk, context))?;
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

fn poll_line(sink: &OutputSink, line: Line<CHUNK>, context: &mut Context<'_>) -> Poll<Result<(), RunError>> {
    let mut call = UnpinPipe::call(sink, line);
    Pin::new(&mut call).poll(context).map_err(|_| RunError::Reactor)
}

fn next_chunk() -> Option<(i32, Line<CHUNK>)> {
    QUEUE.with_borrow_mut(|queue| {
        if queue.read >= queue.length {
            queue.read = 0;
            queue.length = 0;
            return None;
        }
        let read = queue.read;
        let descriptor = i32::from(queue.bytes[read]);
        let mut header = [0; 4];
        header.copy_from_slice(&queue.bytes[read + 1..read + HEADER]);
        let remaining = u32::from_le_bytes(header) as usize;
        let take = remaining.min(CHUNK);
        let start = read + HEADER;
        let mut line = Line::<CHUNK>::new();
        line.text(&queue.bytes[start..start + take]);
        if take == remaining {
            queue.read = start + take;
        } else {
            let rest = u32::try_from(remaining - take).ok()?;
            queue.bytes[start + take - HEADER] = u8::try_from(descriptor).ok()?;
            queue.bytes[start + take - HEADER + 1..start + take].copy_from_slice(&rest.to_le_bytes());
            queue.read = start + take - HEADER;
        }
        Some((descriptor, line))
    })
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

    #[test]
    fn drain_parks_on_a_full_stderr_and_completes_after_the_reader_drains() {
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
