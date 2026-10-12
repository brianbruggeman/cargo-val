//! Async reads from nonblocking child pipes through the shared Linux reactor.

use core::ffi::c_int;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use proxima_core::io::AsyncRead;

use super::linux_exit::LinuxExitPort;
use super::unix_pipe_pair::OwnedDescriptor;

/// Caller-buffer reader whose readiness token belongs to `LinuxExitPort`.
#[derive(Debug)]
pub struct LinuxPipeReader<'port> {
    descriptor: OwnedDescriptor,
    port: &'port LinuxExitPort,
    token: u64,
    waker: Option<Waker>,
    eof: bool,
}

impl<'port> LinuxPipeReader<'port> {
    /// Connects an owned descriptor and its registered token to the shared reactor.
    #[must_use]
    pub(crate) fn new(descriptor: OwnedDescriptor, port: &'port LinuxExitPort, token: u64) -> Self {
        Self {
            descriptor,
            port,
            token,
            waker: None,
            eof: false,
        }
    }

    /// Token emitted by the shared reactor when this pipe becomes readable.
    #[must_use]
    pub const fn token(&self) -> u64 {
        self.token
    }

    /// Suspends source readiness while the caller consumes the current artifact.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn pause(&self) -> Result<(), c_int> {
        self.port.unwatch_descriptor(self.descriptor.raw())
    }

    /// Registers the same descriptor after the caller has consumed that artifact.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn resume(&mut self) -> Result<(), c_int> {
        self.token = self.port.watch_descriptor(self.descriptor.raw())?;
        Ok(())
    }

    /// Wakes the pending reader after its token appears in a reactor batch.
    pub fn wake_read_ready(&mut self) -> bool {
        if let Some(waker) = self.waker.take() {
            waker.wake();
            true
        } else {
            false
        }
    }
}

impl AsyncRead for LinuxPipeReader<'_> {
    type Error = c_int;

    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Self::Error>> {
        let reader = self.get_mut();
        if reader.eof || buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            let read = unsafe {
                libc::read(
                    reader.descriptor.raw(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            match usize::try_from(read) {
                Ok(0) => {
                    if let Err(code) = reader.port.unwatch_descriptor(reader.descriptor.raw()) {
                        return Poll::Ready(Err(code));
                    }
                    reader.eof = true;
                    reader.waker = None;
                    return Poll::Ready(Ok(0));
                }
                Ok(count) => {
                    reader.waker = None;
                    return Poll::Ready(Ok(count));
                }
                Err(_) => {}
            }
            let code = last_error();
            if code == libc::EINTR {
                continue;
            }
            if code == libc::EAGAIN || code == libc::EWOULDBLOCK {
                match reader.waker.as_mut() {
                    Some(waker) => waker.clone_from(context.waker()),
                    None => reader.waker = Some(context.waker().clone()),
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(code));
        }
    }
}

fn last_error() -> c_int {
    unsafe { *libc::__errno_location() }
}
