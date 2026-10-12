use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::task::{Context, Poll};
use std::boxed::Box;
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::string::String;
use std::sync::Once;
use std::vec::Vec;

use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Pipe, UnpinPipe};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_HANDLE, ERROR_IO_PENDING, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_CHAR, FILE_TYPE_DISK,
    FILE_TYPE_PIPE, GetFileType, ReOpenFile, WriteFile,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, WriteConsoleW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::line::Line;

const LINE_CAPACITY: usize = 512;
const STANDARD_OUTPUT: i32 = 1;
const STANDARD_ERROR: i32 = 2;
const SINK_CARRY: usize = 2;

static SYNCHRONOUS_NOTICE: Once = Once::new();

std::thread_local! {
    static CONSOLE_CARRY: RefCell<[Vec<u8>; 3]> = const { RefCell::new([Vec::new(), Vec::new(), Vec::new()]) };
}

/// How an output handle accepts a line, decided once from `GetFileType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Console,
    Synchronous,
    Overlapped,
}

/// Sink pipe (`Line<512> -> io::Result<()>`) for one output handle: a console is written synchronously with `WriteConsoleW`,
/// a file with `WriteFile`, and a pipe through an overlapped reopen completed on the prime port.
///
/// The console has no asynchronous I/O, so a console line blocks the worker for the duration of one write; a file write
/// does not wait on any reader. A pipe is reopened with `FILE_FLAG_OVERLAPPED`; when Windows refuses the reopen, or the
/// handle is another kind, the sink writes synchronously and says so once on standard error. The line being written
/// lives here, with its offset, never in the call future: dropping a pending call loses nothing, and the next call
/// finishes the remainder before it starts its own line, so lines never interleave. Built on
/// `prime::os::readiness::Readiness::completion`; a line is copied once into the sink's fixed buffer, nothing is
/// allocated per line.
pub struct OutputSink {
    handle: OwnedHandle,
    mode: Mode,
    gate: RefCell<Option<Readiness>>,
    pending: Box<RefCell<[u8; LINE_CAPACITY]>>,
    overlapped: RefCell<Box<OVERLAPPED>>,
    inflight: Cell<bool>,
    length: Cell<usize>,
    offset: Cell<usize>,
}

impl OutputSink {
    /// Classify `handle` once with `GetFileType` and prepare it for writes.
    ///
    /// # Errors
    /// Returns the OS error from `GetFileType`.
    pub fn new(handle: OwnedHandle) -> io::Result<Self> {
        let (handle, mode) = match file_type(handle.as_raw_handle())? {
            FILE_TYPE_CHAR if is_console(handle.as_raw_handle()) => (handle, Mode::Console),
            FILE_TYPE_DISK => (handle, Mode::Synchronous),
            FILE_TYPE_PIPE => match reopen_overlapped(&handle) {
                Some(reopened) => (reopened, Mode::Overlapped),
                None => (handle, synchronous_with_notice()),
            },
            _ => (handle, synchronous_with_notice()),
        };
        Ok(Self {
            handle,
            mode,
            gate: RefCell::new(None),
            pending: Box::new(RefCell::new([0; LINE_CAPACITY])),
            overlapped: RefCell::new(Box::new(OVERLAPPED::default())),
            inflight: Cell::new(false),
            length: Cell::new(0),
            offset: Cell::new(0),
        })
    }

    /// Which write path the handle took: `console`, `synchronous`, or `overlapped`.
    #[must_use]
    pub const fn mode(&self) -> &'static str {
        match self.mode {
            Mode::Console => "console",
            Mode::Synchronous => "synchronous",
            Mode::Overlapped => "overlapped",
        }
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    fn flush(&self, context: &Context<'_>) -> Poll<io::Result<()>> {
        while self.offset.get() < self.length.get() {
            let written = match self.mode {
                Mode::Console | Mode::Synchronous => self.write_once(),
                Mode::Overlapped => core::task::ready!(self.write_overlapped(context)),
            };
            match written {
                Ok(0) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Ok(count) => self.offset.set(self.offset.get() + count),
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        Poll::Ready(Ok(()))
    }

    fn write_once(&self) -> io::Result<usize> {
        let pending = self.pending.borrow();
        let remaining = &pending[self.offset.get()..self.length.get()];
        match self.mode {
            Mode::Console => {
                write_console(self.raw(), SINK_CARRY, remaining)?;
                Ok(remaining.len())
            }
            _ => write_file(self.raw(), remaining),
        }
    }

    fn write_overlapped(&self, context: &Context<'_>) -> Poll<io::Result<usize>> {
        if let Err(error) = self.arm(context) {
            return Poll::Ready(Err(error));
        }
        if !self.inflight.get()
            && let Err(error) = self.start_write()
        {
            return Poll::Ready(Err(error));
        }
        let mut guard = self.gate.borrow_mut();
        let Some(gate) = guard.as_mut() else {
            return Poll::Ready(Err(io::Error::other("output gate is not armed")));
        };
        loop {
            match gate.poll(context) {
                Ok(ReadyState::Parked) => return Poll::Pending,
                Ok(ReadyState::OffWorker) => {
                    return Poll::Ready(Err(io::Error::other(
                        "output gate polled off a prime worker",
                    )));
                }
                Ok(ReadyState::Retry) => {
                    if let Some(entry) = gate.take_completion() {
                        return Poll::Ready(self.finish_write(entry.lpOverlapped));
                    }
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }

    // the handle joins the worker's completion port at the first poll, before any write can queue a packet
    fn arm(&self, context: &Context<'_>) -> io::Result<()> {
        let mut gate = self.gate.borrow_mut();
        if gate.is_some() {
            return Ok(());
        }
        let mut opened = Readiness::completion(&self.handle.as_handle())?;
        if opened.poll(context)? == ReadyState::OffWorker {
            return Err(io::Error::other("output gate polled off a prime worker"));
        }
        *gate = Some(opened);
        Ok(())
    }

    fn start_write(&self) -> io::Result<()> {
        let pending = self.pending.borrow();
        let remaining = &pending[self.offset.get()..self.length.get()];
        let wanted = u32::try_from(remaining.len()).map_err(|_| io::Error::other("line exceeds a write"))?;
        let mut overlapped = self.overlapped.borrow_mut();
        **overlapped = OVERLAPPED::default();
        let started = unsafe {
            WriteFile(
                self.raw(),
                remaining.as_ptr(),
                wanted,
                core::ptr::null_mut(),
                &raw mut **overlapped,
            )
        };
        if started == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(io::Error::last_os_error());
        }
        self.inflight.set(true);
        Ok(())
    }

    fn finish_write(&self, completed: *mut OVERLAPPED) -> io::Result<usize> {
        self.inflight.set(false);
        let mut written = 0;
        if unsafe { GetOverlappedResult(self.raw(), completed, &raw mut written, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    fn load(&self, line: &Line<LINE_CAPACITY>) {
        let bytes = line.bytes();
        self.pending.borrow_mut()[..bytes.len()].copy_from_slice(bytes);
        self.length.set(bytes.len());
        self.offset.set(0);
    }
}

impl Drop for OutputSink {
    fn drop(&mut self) {
        if self.inflight.get() {
            let mut written = 0;
            let overlapped = &raw mut **self.overlapped.get_mut();
            unsafe {
                CancelIoEx(self.raw(), overlapped);
                GetOverlappedResult(self.raw(), overlapped, &raw mut written, 1);
            }
        }
    }
}

impl DropSafe for OutputSink {}

impl UnpinPipe for OutputSink {
    type In = Line<LINE_CAPACITY>;
    type Out = ();
    type Err = io::Error;

    fn call(
        &self,
        line: Line<LINE_CAPACITY>,
    ) -> impl Future<Output = Result<(), io::Error>> + Unpin {
        let mut staged = Some(line);
        poll_fn(move |context| {
            if let Err(error) = core::task::ready!(self.flush(context)) {
                return Poll::Ready(Err(error));
            }
            if let Some(line) = staged.take() {
                self.load(&line);
            }
            self.flush(context)
        })
    }
}

impl Pipe for OutputSink {
    type In = Line<LINE_CAPACITY>;
    type Out = ();
    type Err = io::Error;

    fn call(&self, line: Line<LINE_CAPACITY>) -> impl Future<Output = Result<(), io::Error>> {
        UnpinPipe::call(self, line)
    }
}

/// Writes all of `bytes` to standard output (`1`) or standard error (`2`), blocking until Windows takes them.
///
/// The synchronous twin of [`OutputSink`] for the sync report paths that cannot await a gate. A console takes
/// `WriteConsoleW`, which is synchronous by nature; anything else takes `WriteFile`.
///
/// # Errors
///
/// Returns the OS error of a failed write, `WriteZero` when the handle accepts no bytes, or `ERROR_INVALID_HANDLE` when the process has no such standard handle.
pub fn write(descriptor: i32, bytes: &[u8]) -> io::Result<()> {
    let (slot, which) = match descriptor {
        STANDARD_OUTPUT => (STD_OUTPUT_HANDLE, 0),
        STANDARD_ERROR => (STD_ERROR_HANDLE, 1),
        _ => return Err(io::ErrorKind::InvalidInput.into()),
    };
    let handle = unsafe { GetStdHandle(slot) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::from_raw_os_error(ERROR_INVALID_HANDLE.cast_signed()));
    }
    if file_type(handle)? == FILE_TYPE_CHAR && is_console(handle) {
        return write_console(handle, which, bytes);
    }
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let written = write_file(handle, remaining)?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        remaining = &remaining[written..];
    }
    Ok(())
}

fn file_type(handle: HANDLE) -> io::Result<u32> {
    let kind = unsafe { GetFileType(handle) };
    if kind == 0 && unsafe { GetLastError() } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(kind)
}

fn is_console(handle: HANDLE) -> bool {
    let mut mode = 0;
    unsafe { GetConsoleMode(handle, &raw mut mode) != 0 }
}

fn write_file(handle: HANDLE, bytes: &[u8]) -> io::Result<usize> {
    let wanted = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let mut written = 0;
    let ok = unsafe {
        WriteFile(handle, bytes.as_ptr(), wanted, &raw mut written, core::ptr::null_mut())
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(written as usize)
}

// a multi-byte character split across two writes waits in the carry for its tail, since the console takes whole UTF-16 text
fn write_console(handle: HANDLE, which: usize, bytes: &[u8]) -> io::Result<()> {
    let units = CONSOLE_CARRY.with(|carry| {
        let mut carry = carry.borrow_mut();
        let mut text = core::mem::take(&mut carry[which]);
        text.extend_from_slice(bytes);
        let split = match core::str::from_utf8(&text) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => text.len(),
        };
        let (ready, rest) = text.split_at(split);
        carry[which].extend_from_slice(rest);
        String::from_utf8_lossy(ready).encode_utf16().collect::<Vec<u16>>()
    });
    let mut remaining = units.as_slice();
    while !remaining.is_empty() {
        let wanted = u32::try_from(remaining.len()).unwrap_or(u32::MAX);
        let mut written = 0;
        let ok = unsafe {
            WriteConsoleW(handle, remaining.as_ptr().cast(), wanted, &raw mut written, core::ptr::null())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        remaining = &remaining[written as usize..];
    }
    Ok(())
}

fn reopen_overlapped(handle: &OwnedHandle) -> Option<OwnedHandle> {
    let reopened = unsafe {
        ReOpenFile(
            handle.as_raw_handle(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_FLAG_OVERLAPPED,
        )
    };
    // safety: ReOpenFile returned a new handle that nothing else owns.
    (reopened != INVALID_HANDLE_VALUE && !reopened.is_null())
        .then(|| unsafe { OwnedHandle::from_raw_handle(reopened) })
}

fn synchronous_with_notice() -> Mode {
    SYNCHRONOUS_NOTICE.call_once(|| {
        let _ = write(
            STANDARD_ERROR,
            b"cargo-val: the output handle cannot be reopened for overlapped writes; writing synchronously\n",
        );
    });
    Mode::Synchronous
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::pin::Pin;
    use std::io::{Read, pipe};
    use std::os::windows::io::OwnedHandle;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::UnpinPipe;
    use proxima_runtime::CoreId;

    use super::{LINE_CAPACITY, OutputSink};
    use crate::line::Line;
    use crate::test_dir::tempdir;

    const REPORT_LINE: &[u8] = b"cargo-val: 12 passed; 0 failed; 3 ignored; finished in 4.21s\n";

    fn line(text: &[u8]) -> Line<LINE_CAPACITY> {
        let mut line = Line::new();
        line.text(text);
        line
    }

    fn deliver(sink: OutputSink, text: &'static [u8]) -> OutputSink {
        run_on_this_thread(CoreId(0), async move {
            let mut call = UnpinPipe::call(&sink, line(text));
            poll_fn(|context| Pin::new(&mut call).poll(context))
                .await
                .expect("the line is written");
            drop(call);
            sink
        })
        .expect("root completes")
    }

    #[test]
    fn a_file_takes_the_synchronous_write_branch() {
        let directory = tempdir().expect("scratch directory");
        let path = directory.path().join("report.txt");
        let file = std::fs::File::create(&path).expect("report file opens");

        let sink = OutputSink::new(OwnedHandle::from(file)).expect("file classifies");
        let sink = deliver(sink, REPORT_LINE);

        assert_eq!(sink.mode(), "synchronous");
        assert_eq!(std::fs::read(&path).expect("report reads back"), REPORT_LINE);
        std::println!("{{\"output_branch\":\"{}\"}}", sink.mode());
    }

    #[test]
    fn an_anonymous_pipe_reports_which_branch_carried_the_line() {
        let (mut reader, writer) = pipe().expect("anonymous pipe is created");

        let sink = OutputSink::new(OwnedHandle::from(writer)).expect("pipe classifies");
        let branch = sink.mode();
        let sink = deliver(sink, REPORT_LINE);
        drop(sink);
        let mut received = std::vec::Vec::new();
        reader.read_to_end(&mut received).expect("pipe drains");

        assert!(matches!(branch, "overlapped" | "synchronous"), "{branch}");
        assert_eq!(received, REPORT_LINE);
        std::println!("{{\"output_branch\":\"{branch}\"}}");
    }

    #[test]
    fn standard_descriptors_outside_one_and_two_are_refused() {
        let error = super::write(7, b"ignored").expect_err("descriptor 7 is not a standard stream");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
