use core::cell::{Cell, Ref, RefCell};
use core::future::poll_fn;
use core::mem::MaybeUninit;
use core::ops::{ControlFlow, Range};
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};
use std::boxed::Box;
use std::io;

use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, Pipe, UnpinPipe};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED_ENTRY};

use crate::edge_spawn_windows::PipeEnd;
use crate::host::windows_pipe::{
    WindowsPipeReadCompletion, WindowsPipeReadSlot, WindowsPipeReadStart, WindowsPipeReadStartError,
};

pub(crate) const CAPTURE_BYTES: u32 = 16 * 1024;
const SCRATCH_BYTES: u32 = 4096;
const STAGING_BYTES: usize = CAPTURE_BYTES as usize;

/// Source pipe (`() -> io::Result<Range<u32>>`) for one child output stream: a prime completion gate, then one overlapped read per item.
///
/// Each item is the range of the bytes just read into a fixed capture window this source owns;
/// read them with [`ChildStream::window`]. Retained bytes are never rewritten. Once the window is
/// full the source keeps draining the pipe (so the child never blocks) into a fixed scratch tail
/// and yields ranges offset past `CAPTURE_BYTES`; such a range is valid only until the next read,
/// and [`ChildStream::truncated`] turns true. EOF resolves `Err(Exhausted)`,
/// as does every call after it or after a fault. Gate, staging buffer and window live here, never in
/// the call future, so dropping a pending call loses nothing. A read ends in a staging buffer the
/// kernel owns until its completion packet is dequeued, and is copied once into the window: Windows
/// reads cannot append at a moving offset of storage that readers borrow. Built on
/// `prime::os::readiness::Readiness::completion` and [`WindowsPipeReadSlot`]; a read that finishes
/// at `start` (a closed pipe) queues no packet, so it yields without waiting on the gate. Merge
/// several with `proxima_primitives::pipe::FanIn`.
pub struct ChildStream {
    end: PipeEnd,
    slot: RefCell<WindowsPipeReadSlot<'static>>,
    staging: NonNull<[MaybeUninit<u8>]>,
    window: RefCell<[u8; CAPTURE_BYTES as usize]>,
    scratch: RefCell<[u8; SCRATCH_BYTES as usize]>,
    length: Cell<u32>,
    scratch_length: Cell<u32>,
    ended: Cell<bool>,
}

impl ChildStream {
    #[must_use]
    pub fn new(end: PipeEnd) -> Self {
        let staging = Box::into_raw(Box::new_uninit_slice(STAGING_BYTES));
        Self {
            end,
            slot: RefCell::new(WindowsPipeReadSlot::new(&mut [])),
            // safety: Box::into_raw never returns null.
            staging: unsafe { NonNull::new_unchecked(staging) },
            window: RefCell::new([0; CAPTURE_BYTES as usize]),
            scratch: RefCell::new([0; SCRATCH_BYTES as usize]),
            length: Cell::new(0),
            scratch_length: Cell::new(0),
            ended: Cell::new(false),
        }
    }

    /// The bytes a yielded range covers, or `None` if the range reaches past what was read.
    pub fn window(&self, range: Range<u32>) -> Option<Ref<'_, [u8]>> {
        if range.start > range.end {
            return None;
        }
        if range.start >= CAPTURE_BYTES {
            let (start, end) = (range.start - CAPTURE_BYTES, range.end - CAPTURE_BYTES);
            if end > self.scratch_length.get() {
                return None;
            }
            return Ref::filter_map(self.scratch.borrow(), |bytes| {
                bytes.get(start as usize..end as usize)
            })
            .ok();
        }
        if range.end > self.length.get() {
            return None;
        }
        Ref::filter_map(self.window.borrow(), |bytes| {
            bytes.get(range.start as usize..range.end as usize)
        })
        .ok()
    }

    /// Whether any bytes went past the retained window into the scratch tail.
    pub fn truncated(&self) -> bool {
        self.length.get() >= CAPTURE_BYTES && self.scratch_length.get() > 0
    }

    fn raw(&self) -> HANDLE {
        self.end.raw()
    }

    fn advance(&self, context: &Context<'_>) -> Poll<Result<io::Result<Range<u32>>, Exhausted>> {
        if self.ended.get() {
            return Poll::Ready(Err(Exhausted));
        }
        match self.drive(context) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Some(range))) => Poll::Ready(Ok(Ok(range))),
            Ok(Poll::Ready(None)) => {
                self.ended.set(true);
                self.end.drained();
                Poll::Ready(Err(Exhausted))
            }
            Err(error) => self.fault(error),
        }
    }

    fn fault(&self, error: io::Error) -> Poll<Result<io::Result<Range<u32>>, Exhausted>> {
        self.ended.set(true);
        Poll::Ready(Ok(Err(error)))
    }

    fn drive(&self, context: &Context<'_>) -> io::Result<Poll<Option<Range<u32>>>> {
        self.arm(context)?;
        loop {
            if self.idle() {
                let step = match self.start()? {
                    WindowsPipeReadStart::Completed(done) => self.finish(done)?,
                    WindowsPipeReadStart::Pending => ControlFlow::Continue(()),
                };
                if let ControlFlow::Break(outcome) = step {
                    return Ok(Poll::Ready(outcome));
                }
            }
            if !self.pending() {
                continue;
            }
            let Poll::Ready(done) = self.next_completion(context)? else {
                return Ok(Poll::Pending);
            };
            if let ControlFlow::Break(outcome) = self.finish(done)? {
                return Ok(Poll::Ready(outcome));
            }
        }
    }

    // the handle joins the worker's completion port at the first poll, before any read can queue a packet
    fn arm(&self, context: &Context<'_>) -> io::Result<()> {
        let mut gate = self.end.gate().borrow_mut();
        if gate.is_some() {
            return Ok(());
        }
        let mut opened = Readiness::completion(&self.end.borrowed())?;
        if opened.poll(context)? == ReadyState::OffWorker {
            return Err(io::Error::other("stream gate polled off a prime worker"));
        }
        *gate = Some(opened);
        Ok(())
    }

    fn idle(&self) -> bool {
        self.slot.borrow().state() == crate::host::windows_pipe::WindowsPipeReadState::Idle
    }

    fn pending(&self) -> bool {
        self.slot.borrow().state() == crate::host::windows_pipe::WindowsPipeReadState::Pending
    }

    fn start(&self) -> io::Result<WindowsPipeReadStart> {
        let wanted = match CAPTURE_BYTES.checked_sub(self.length.get()) {
            Some(room) if room > 0 => (room as usize).min(STAGING_BYTES),
            _ => SCRATCH_BYTES as usize,
        };
        // safety: the staging buffer outlives every slot built over it and only the slot touches it.
        let buffer = unsafe {
            core::slice::from_raw_parts_mut(self.staging.as_ptr().cast::<MaybeUninit<u8>>(), wanted)
        };
        let mut slot = self.slot.borrow_mut();
        *slot = WindowsPipeReadSlot::new(buffer);
        // safety: the stream sits in a lane's Box and is never moved once polled; `Drop` waits out a pending read.
        let started = unsafe { Pin::new_unchecked(&mut *slot).start(self.raw()) };
        started.map_err(|error| match error {
            WindowsPipeReadStartError::ReadFailed { code } => {
                io::Error::from_raw_os_error(code.cast_signed())
            }
            other => io::Error::other(std::format!("overlapped read refused: {other:?}")),
        })
    }

    fn next_completion(&self, context: &Context<'_>) -> io::Result<Poll<WindowsPipeReadCompletion>> {
        let mut guard = self.end.gate().borrow_mut();
        let gate = guard
            .as_mut()
            .ok_or_else(|| io::Error::other("stream gate is not armed"))?;
        loop {
            match gate.poll(context)? {
                ReadyState::Parked => return Ok(Poll::Pending),
                ReadyState::OffWorker => {
                    return Err(io::Error::other("stream gate polled off a prime worker"));
                }
                ReadyState::Retry => {
                    if let Some(entry) = gate.take_completion() {
                        return self.complete(&entry).map(Poll::Ready);
                    }
                }
            }
        }
    }

    fn complete(&self, entry: &OVERLAPPED_ENTRY) -> io::Result<WindowsPipeReadCompletion> {
        let mut transferred = 0;
        let finished = unsafe {
            GetOverlappedResult(self.raw(), entry.lpOverlapped, &raw mut transferred, 0)
        };
        let code = if finished == 0 { unsafe { GetLastError() } } else { 0 };
        let mut slot = self.slot.borrow_mut();
        // safety: the entry is the packet this gate just dequeued for the read the slot started, consumed once.
        unsafe { Pin::new_unchecked(&mut *slot).complete(entry.lpOverlapped, transferred, code) }
            .map_err(|error| io::Error::other(std::format!("completion refused: {error:?}")))
    }

    fn finish(
        &self,
        done: WindowsPipeReadCompletion,
    ) -> io::Result<ControlFlow<Option<Range<u32>>>> {
        match done {
            WindowsPipeReadCompletion::Bytes { length } => {
                let range = self.retain(length)?;
                Ok(ControlFlow::Break(Some(range)))
            }
            WindowsPipeReadCompletion::Eof => Ok(ControlFlow::Break(None)),
            WindowsPipeReadCompletion::Empty | WindowsPipeReadCompletion::Cancelled => {
                Ok(ControlFlow::Continue(()))
            }
            WindowsPipeReadCompletion::Failed { code } => {
                Err(io::Error::from_raw_os_error(code.cast_signed()))
            }
        }
    }

    fn retain(&self, length: usize) -> io::Result<Range<u32>> {
        let count = u32::try_from(length)
            .map_err(|_| io::Error::other("read exceeds the capture window"))?;
        let mut slot = self.slot.borrow_mut();
        // safety: the slot is pinned in place for as long as the stream is.
        let mut pinned = unsafe { Pin::new_unchecked(&mut *slot) };
        let range = {
            let bytes = pinned
                .as_ref()
                .completed_bytes()
                .ok_or_else(|| io::Error::other("completed read holds no bytes"))?;
            self.copy_out(bytes, count)?
        };
        pinned.as_mut().release_completed().map_err(|state| {
            io::Error::other(std::format!("completed read was not releasable in {state:?}"))
        })?;
        Ok(range)
    }

    fn copy_out(&self, bytes: &[u8], count: u32) -> io::Result<Range<u32>> {
        let start = self.length.get();
        if start >= CAPTURE_BYTES {
            let mut scratch = self
                .scratch
                .try_borrow_mut()
                .map_err(|_| io::Error::other("scratch tail is borrowed during a read"))?;
            scratch[..bytes.len()].copy_from_slice(bytes);
            self.scratch_length.set(count);
            return Ok(CAPTURE_BYTES..CAPTURE_BYTES + count);
        }
        let mut window = self
            .window
            .try_borrow_mut()
            .map_err(|_| io::Error::other("capture window is borrowed during a read"))?;
        window[start as usize..start as usize + bytes.len()].copy_from_slice(bytes);
        self.length.set(start + count);
        Ok(start..start + count)
    }
}

impl Drop for ChildStream {
    fn drop(&mut self) {
        let raw = self.raw();
        let slot = self.slot.get_mut();
        // safety: the stream is in place, and abandon waits until the kernel is done with the staging buffer.
        unsafe { Pin::new_unchecked(slot).abandon(raw) };
        // safety: `staging` came from Box::into_raw in `new`, and the slot over it was just abandoned.
        drop(unsafe { Box::from_raw(self.staging.as_ptr()) });
    }
}

impl DropSafe for ChildStream {}

impl UnpinPipe for ChildStream {
    type In = ();
    type Out = io::Result<Range<u32>>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> + Unpin {
        poll_fn(move |context| self.advance(context))
    }
}

impl Pipe for ChildStream {
    type In = ();
    type Out = io::Result<Range<u32>>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> {
        UnpinPipe::call(self, ())
    }
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::io;
    use std::os::windows::io::{BorrowedHandle, OwnedHandle};
    use crate::edge_spawn_windows::PooledEnd;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::{Exhausted, UnpinPipe};
    use proxima_runtime::CoreId;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::WriteFile;

    use super::{CAPTURE_BYTES, ChildStream};
    use crate::edge_spawn_windows::pipe_pair::WindowsPipePair;

    const RECORD: &[u8] = b"{\"reason\":\"build-finished\",\"success\":true}\n";

    static SEQUENCE: AtomicU32 = AtomicU32::new(0);

    fn pipe_pair() -> WindowsPipePair {
        let name = format!(
            "\\\\.\\pipe\\cargo-val-stream-{}-{}\0",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let units: Vec<u16> = name.encode_utf16().collect();
        WindowsPipePair::new(&units).expect("overlapped pipe pair is created")
    }

    fn stream_over(pair: &WindowsPipePair) -> ChildStream {
        // safety: the pair owns the reader for as long as this borrow lasts.
        let reader = unsafe { BorrowedHandle::borrow_raw(pair.parent_read_handle()) };
        let duplicate: OwnedHandle = reader.try_clone_to_owned().expect("reader duplicates");
        ChildStream::new(PooledEnd::unpooled(duplicate))
    }

    fn write_all(handle: HANDLE, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut written = 0;
            let remaining = u32::try_from(bytes.len() - offset).expect("fixture write fits u32");
            let ok = unsafe {
                WriteFile(
                    handle,
                    bytes[offset..].as_ptr(),
                    remaining,
                    &raw mut written,
                    core::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "child side write succeeds");
            offset += written as usize;
        }
    }

    async fn next(stream: &ChildStream) -> Result<io::Result<core::ops::Range<u32>>, Exhausted> {
        UnpinPipe::call(stream, ()).await
    }

    #[test]
    fn c45c_yields_each_record_then_ends_when_the_child_closes_its_writer() {
        let mut pair = pipe_pair();
        let writer = pair.child_write_handle().expect("child writer is open");
        let stream = stream_over(&pair);
        write_all(writer, RECORD);

        let (bytes, ended) = run_on_this_thread(CoreId(0), async move {
            let range = next(&stream)
                .await
                .expect("a read is yielded")
                .expect("read succeeds");
            let bytes = stream.window(range).expect("range is retained").to_vec();
            pair.close_child_write_handle();
            let ended = matches!(next(&stream).await, Err(Exhausted));
            (bytes, ended)
        })
        .expect("root completes");

        assert_eq!(bytes, RECORD);
        assert!(ended, "the closed writer ends the stream");
    }

    #[test]
    fn c45c_reports_end_for_a_pipe_whose_writer_closed_before_the_first_read() {
        let mut pair = pipe_pair();
        pair.close_child_write_handle();
        let stream = stream_over(&pair);

        let ended = run_on_this_thread(CoreId(0), async move {
            let _pair = pair;
            matches!(next(&stream).await, Err(Exhausted))
        })
        .expect("root completes");

        assert!(ended, "a closed pipe ends without waiting for a packet that never comes");
    }

    #[test]
    fn c45c_keeps_draining_into_the_scratch_tail_once_the_window_is_full() {
        let pair = pipe_pair();
        let writer = pair.child_write_handle().expect("child writer is open") as usize;
        let stream = stream_over(&pair);
        let total = CAPTURE_BYTES as usize + 1024;
        let producer = std::thread::spawn(move || {
            write_all(writer as HANDLE, &std::vec![b'x'; total]);
        });

        let (retained, truncated) = run_on_this_thread(CoreId(0), async move {
            let _pair = pair;
            let mut retained = 0;
            while retained < total {
                let range = next(&stream)
                    .await
                    .expect("a read is yielded")
                    .expect("read succeeds");
                retained += (range.end - range.start) as usize;
            }
            (retained, stream.truncated())
        })
        .expect("root completes");
        producer.join().expect("producer finishes");

        assert_eq!(retained, total);
        assert!(truncated);
    }
}
