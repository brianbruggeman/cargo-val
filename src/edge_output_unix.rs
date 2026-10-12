use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::task::{Context, Poll};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use libc::c_int;
use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Pipe, UnpinPipe};

use crate::line::Line;

const LINE_CAPACITY: usize = 512;
const TTY_NAME_BYTES: usize = 256;

#[cfg(target_os = "linux")]
const SEND_FLAGS: c_int = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
#[cfg(target_os = "macos")]
const SEND_FLAGS: c_int = libc::MSG_DONTWAIT;

/// Sink pipe (`Line<512> -> io::Result<()>`) for one output descriptor: one nonblocking write per step, a prime writable gate on `EAGAIN`.
///
/// The line being written lives here, with its offset, never in the call future: dropping a
/// pending call loses nothing, and the next call finishes the remainder before it starts its own
/// line, so lines never interleave. Regular files and character devices write directly with no
/// gate; a tty is reopened `O_NONBLOCK`; a pipe is reopened through `/proc/self/fd` on linux, or
/// has `O_NONBLOCK` set on the inherited description and restored on drop on macOS; a socket
/// uses `send` with `MSG_DONTWAIT`. Built on `prime::os::readiness::Readiness::writable`;
/// a line is copied once into the sink's fixed buffer, nothing is allocated per line.
pub struct OutputSink {
    descriptor: OwnedFd,
    gate: RefCell<Readiness>,
    pending: RefCell<[u8; LINE_CAPACITY]>,
    length: Cell<usize>,
    offset: Cell<usize>,
    gated: bool,
    socket: bool,
    restore_flags: Option<c_int>,
}

impl OutputSink {
    /// Classify `descriptor` once with `fstat` and prepare it for nonblocking writes.
    ///
    /// # Errors
    /// Returns the OS error from `fstat`, the reopen, or the flag change.
    pub fn new(descriptor: OwnedFd) -> io::Result<Self> {
        let mode = file_mode(&descriptor)? & libc::S_IFMT;
        let socket = mode == libc::S_IFSOCK;
        let tty = mode == libc::S_IFCHR && unsafe { libc::isatty(descriptor.as_raw_fd()) } == 1;
        let pipe = mode == libc::S_IFIFO;
        let (descriptor, restore_flags) = if tty {
            (reopen_tty(&descriptor)?, None)
        } else if pipe {
            adopt_pipe(descriptor)?
        } else {
            (descriptor, None)
        };
        if socket {
            suppress_sigpipe(&descriptor)?;
        }
        Ok(Self {
            gate: RefCell::new(Readiness::writable(descriptor.as_raw_fd())),
            descriptor,
            pending: RefCell::new([0; LINE_CAPACITY]),
            length: Cell::new(0),
            offset: Cell::new(0),
            gated: socket || tty || pipe,
            socket,
            restore_flags,
        })
    }

    fn flush(&self, context: &Context<'_>) -> Poll<io::Result<()>> {
        while self.offset.get() < self.length.get() {
            match self.write_once() {
                Ok(0) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Ok(count) => self.offset.set(self.offset.get() + count),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if self.gated && error.kind() == io::ErrorKind::WouldBlock => {
                    match self.gate.borrow_mut().poll(context) {
                        Ok(ReadyState::Retry) => {}
                        Ok(ReadyState::Parked) => return Poll::Pending,
                        Ok(ReadyState::OffWorker) => {
                            return Poll::Ready(Err(io::Error::other(
                                "output gate polled off a prime worker",
                            )));
                        }
                        Err(error) => return Poll::Ready(Err(error)),
                    }
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        Poll::Ready(Ok(()))
    }

    fn write_once(&self) -> io::Result<usize> {
        let pending = self.pending.borrow();
        let remaining = &pending[self.offset.get()..self.length.get()];
        let descriptor = self.descriptor.as_raw_fd();
        let written = if self.socket {
            unsafe {
                libc::send(
                    descriptor,
                    remaining.as_ptr().cast(),
                    remaining.len(),
                    SEND_FLAGS,
                )
            }
        } else {
            unsafe { libc::write(descriptor, remaining.as_ptr().cast(), remaining.len()) }
        };
        usize::try_from(written).map_err(|_| io::Error::last_os_error())
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
        if let Some(flags) = self.restore_flags {
            unsafe { libc::fcntl(self.descriptor.as_raw_fd(), libc::F_SETFL, flags) };
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

/// Writes all of `bytes` to standard output (`1`) or standard error (`2`), blocking until the kernel takes them.
///
/// The synchronous twin of [`OutputSink`] for the sync report paths that cannot await a gate.
///
/// # Errors
///
/// Returns the OS error of a failed write, or `WriteZero` when the descriptor accepts no bytes.
pub fn write(descriptor: i32, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // safety: the pointer and length come from a live slice, and the descriptor is a plain integer.
        let written = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        let count = usize::try_from(written).map_err(|_| io::Error::last_os_error())?;
        if count == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[count..];
    }
    Ok(())
}

fn file_mode(descriptor: &OwnedFd) -> io::Result<libc::mode_t> {
    let mut status: libc::stat = unsafe { core::mem::zeroed() };
    if unsafe { libc::fstat(descriptor.as_raw_fd(), &raw mut status) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(status.st_mode)
}

fn open_nonblocking(path: *const libc::c_char) -> io::Result<OwnedFd> {
    let flags = libc::O_WRONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
    let opened = unsafe { libc::open(path, flags) };
    if opened < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(opened) })
}

fn reopen_tty(descriptor: &OwnedFd) -> io::Result<OwnedFd> {
    let mut name = [0 as libc::c_char; TTY_NAME_BYTES];
    let status = unsafe { libc::ttyname_r(descriptor.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    open_nonblocking(name.as_ptr())
}

#[cfg(target_os = "linux")]
fn adopt_pipe(descriptor: OwnedFd) -> io::Result<(OwnedFd, Option<c_int>)> {
    let path = CString::new(std::format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
        .map_err(io::Error::other)?;
    Ok((open_nonblocking(path.as_ptr())?, None))
}

#[cfg(target_os = "macos")]
fn adopt_pipe(descriptor: OwnedFd) -> io::Result<(OwnedFd, Option<c_int>)> {
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::fcntl(
            descriptor.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((descriptor, Some(flags)))
}

#[cfg(target_os = "linux")]
fn suppress_sigpipe(_descriptor: &OwnedFd) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn suppress_sigpipe(descriptor: &OwnedFd) -> io::Result<()> {
    let enabled: c_int = 1;
    let length = libc::socklen_t::try_from(size_of::<c_int>()).map_err(io::Error::other)?;
    let status = unsafe {
        libc::setsockopt(
            descriptor.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&raw const enabled).cast(),
            length,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::pin::Pin;
    use core::task::Poll;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::UnpinPipe;
    use proxima_runtime::CoreId;

    use super::{LINE_CAPACITY, OutputSink};
    use crate::edge_exit::fixture::in_single_threaded_process;
    use crate::line::Line;
    use crate::test_dir::tempdir;

    const REPORT_LINE: &[u8] = b"cargo-val: 12 passed; 0 failed; 3 ignored; finished in 4.21s\n";
    const FILL_CHUNK: [u8; 4096] = [b'x'; 4096];

    fn line(text: &[u8]) -> Line<LINE_CAPACITY> {
        let mut line = Line::new();
        line.text(text);
        line
    }

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut ends = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe(ends.as_mut_ptr()) },
            0,
            "pipe is created"
        );
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
    }

    fn set_nonblocking(descriptor: &OwnedFd) {
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        let status = unsafe {
            libc::fcntl(
                descriptor.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        };
        assert_eq!(status, 0, "descriptor turns nonblocking");
    }

    fn duplicate(descriptor: &OwnedFd) -> OwnedFd {
        let copy = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(copy >= 0, "descriptor duplicates");
        unsafe { OwnedFd::from_raw_fd(copy) }
    }

    fn fill_until_full(writer: &OwnedFd) -> usize {
        let mut filled = 0;
        loop {
            let written = unsafe {
                libc::write(
                    writer.as_raw_fd(),
                    FILL_CHUNK.as_ptr().cast(),
                    FILL_CHUNK.len(),
                )
            };
            if written < 0 {
                return filled;
            }
            filled += usize::try_from(written).expect("write count is nonnegative");
        }
    }

    fn drain(reader: &OwnedFd) -> Vec<u8> {
        let mut drained = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read =
                unsafe { libc::read(reader.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            let Ok(count) = usize::try_from(read) else {
                return drained;
            };
            if count == 0 {
                return drained;
            }
            drained.extend_from_slice(&buffer[..count]);
        }
    }

    #[cfg(target_os = "macos")]
    fn flags(descriptor: &OwnedFd) -> libc::c_int {
        unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) }
    }

    #[test]
    fn writes_block_without_blocking_thread() {
        in_single_threaded_process(|| {
            let (reader, writer) = pipe_pair();
            set_nonblocking(&reader);
            set_nonblocking(&writer);
            let filled = fill_until_full(&writer);

            let outcome = run_on_this_thread(CoreId(0), async move {
                let sink = OutputSink::new(duplicate(&writer)).expect("sink adopts the pipe");
                let mut call = UnpinPipe::call(&sink, line(REPORT_LINE));
                let first = poll_fn(|context| Poll::Ready(Pin::new(&mut call).poll(context))).await;
                let blocked = drain(&reader);
                let completed = call.await;
                (
                    filled > 0,
                    first.is_pending(),
                    blocked,
                    completed.is_ok(),
                    drain(&reader),
                )
            })
            .expect("root completes");

            assert!(outcome.0, "the pipe accepted bytes before EAGAIN");
            assert!(outcome.1, "a full pipe parks the call instead of blocking");
            assert_eq!(
                outcome.2.len(),
                filled,
                "the reader drained exactly what was filled"
            );
            assert!(outcome.3, "the call completes once the reader drained");
            assert_eq!(
                outcome.4, REPORT_LINE,
                "the line arrives whole after the fill"
            );
        });
    }

    #[test]
    fn dropped_call_loses_no_line_and_keeps_order() {
        in_single_threaded_process(|| {
            let (reader, writer) = pipe_pair();
            set_nonblocking(&reader);
            set_nonblocking(&writer);
            let filled = fill_until_full(&writer);

            let outcome = run_on_this_thread(CoreId(0), async move {
                let sink = OutputSink::new(duplicate(&writer)).expect("sink adopts the pipe");
                let mut abandoned = UnpinPipe::call(&sink, line(b"first line\n"));
                let first =
                    poll_fn(|context| Poll::Ready(Pin::new(&mut abandoned).poll(context))).await;
                drop(abandoned);
                let blocked = drain(&reader);
                let second = UnpinPipe::call(&sink, line(b"second line\n")).await;
                (
                    first.is_pending(),
                    blocked.len(),
                    second.is_ok(),
                    drain(&reader),
                )
            })
            .expect("root completes");

            assert!(outcome.0, "the abandoned call was parked");
            assert_eq!(outcome.1, filled);
            assert!(outcome.2, "the next call completes");
            assert_eq!(outcome.3, b"first line\nsecond line\n");
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_pipe_description_flags_restored_on_drop() {
        in_single_threaded_process(|| {
            let (reader, writer) = pipe_pair();
            let before = flags(&writer);

            let sink = OutputSink::new(duplicate(&writer)).expect("sink adopts the pipe");
            let during = flags(&writer);
            drop(sink);
            let after = flags(&writer);

            assert_eq!(
                before & libc::O_NONBLOCK,
                0,
                "inherited pipe starts blocking"
            );
            assert_ne!(
                during & libc::O_NONBLOCK,
                0,
                "the sink made the description nonblocking"
            );
            assert_eq!(after, before, "F_GETFL is restored after drop");
            drop(reader);
        });
    }

    #[test]
    fn socketpair_receives_the_line() {
        in_single_threaded_process(|| {
            let (writer, mut peer) = UnixStream::pair().expect("socketpair");
            let outcome = run_on_this_thread(CoreId(0), async move {
                let sink = OutputSink::new(OwnedFd::from(writer)).expect("sink adopts the socket");
                UnpinPipe::call(&sink, line(REPORT_LINE)).await
            })
            .expect("root completes");
            let mut received = [0_u8; REPORT_LINE.len()];
            peer.read_exact(&mut received).expect("peer reads the line");

            assert!(outcome.is_ok());
            assert_eq!(received, REPORT_LINE);
        });
    }

    #[test]
    fn tempfile_receives_the_line_without_a_gate() {
        in_single_threaded_process(|| {
            let directory = tempdir().expect("temporary directory");
            let path = directory.path().join("report.txt");
            let file = std::fs::File::create(&path).expect("report file");
            let outcome = run_on_this_thread(CoreId(0), async move {
                let sink = OutputSink::new(OwnedFd::from(file)).expect("sink adopts the file");
                let first = UnpinPipe::call(&sink, line(REPORT_LINE)).await;
                let second = UnpinPipe::call(&sink, line(b"second\n")).await;
                (first.is_ok(), second.is_ok())
            })
            .expect("root completes");

            assert_eq!(outcome, (true, true));
            let mut expected = REPORT_LINE.to_vec();
            expected.extend_from_slice(b"second\n");
            assert_eq!(std::fs::read(&path).expect("file reads back"), expected);
        });
    }

    #[test]
    fn openpty_slave_receives_the_line() {
        in_single_threaded_process(|| {
            let (mut master, mut slave) = (-1, -1);
            let status = unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            };
            assert_eq!(status, 0, "openpty allocates a pair");
            let master = unsafe { OwnedFd::from_raw_fd(master) };
            let slave = unsafe { OwnedFd::from_raw_fd(slave) };
            let held_open = duplicate(&slave);

            let outcome = run_on_this_thread(CoreId(0), async move {
                let sink = OutputSink::new(slave).expect("sink adopts the tty");
                UnpinPipe::call(&sink, line(b"cargo-val ok\n")).await
            })
            .expect("root completes");
            let mut received = [0_u8; 14];
            let count = unsafe { libc::read(master.as_raw_fd(), received.as_mut_ptr().cast(), 14) };

            drop(held_open);
            assert!(outcome.is_ok());
            assert_eq!(
                &received[..usize::try_from(count).expect("master read")],
                b"cargo-val ok\r\n"
            );
        });
    }
}
