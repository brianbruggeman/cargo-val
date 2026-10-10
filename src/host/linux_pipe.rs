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
            if read > 0 {
                reader.waker = None;
                return Poll::Ready(Ok(read as usize));
            }
            if read == 0 {
                if let Err(code) = reader.port.unwatch_descriptor(reader.descriptor.raw()) {
                    return Poll::Ready(Err(code));
                }
                reader.eof = true;
                reader.waker = None;
                return Poll::Ready(Ok(0));
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

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    use proxima_core::io::AsyncRead;

    use super::LinuxPipeReader;
    use crate::host::linux_exit::LinuxExitPort;
    use crate::host::unix_pipe_pair::UnixPipePair;

    #[derive(Debug)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn c40_reads_both_child_streams_without_intermediate_buffers() {
        let pipes = UnixPipePair::new().expect("stdout and stderr pipes are created");
        let mut release_barrier = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(release_barrier.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "pipe fixture child forks");
        if child == 0 {
            unsafe {
                libc::close(release_barrier[1]);
                libc::close(pipes.stdout().parent_read().raw());
                libc::close(pipes.stderr().parent_read().raw());
                let mut release = 0_u8;
                let read = libc::read(
                    release_barrier[0],
                    (&mut release as *mut u8).cast(),
                    size_of_val(&release),
                );
                libc::close(release_barrier[0]);
                if read != 1 {
                    libc::_exit(1);
                }
            }
            let stdout = pipes.stdout().child_write().raw();
            let stderr = pipes.stderr().child_write().raw();
            let standard_output = b"out";
            let standard_error = b"err";
            let stdout_written = unsafe {
                libc::write(
                    stdout,
                    standard_output.as_ptr().cast(),
                    standard_output.len(),
                )
            };
            let stderr_written = unsafe {
                libc::write(stderr, standard_error.as_ptr().cast(), standard_error.len())
            };
            unsafe {
                libc::_exit(i32::from(
                    stdout_written != standard_output.len() as isize
                        || stderr_written != standard_error.len() as isize,
                ));
            }
        }

        unsafe { libc::close(release_barrier[0]) };
        let (stdout_descriptor, stderr_descriptor) = pipes.into_parent_reads();
        let mut port = LinuxExitPort::new().expect("shared epoll port is created");
        let stdout_token = port
            .watch_descriptor(stdout_descriptor.raw())
            .expect("stdout reader registers with the shared reactor");
        let stderr_token = port
            .watch_descriptor(stderr_descriptor.raw())
            .expect("stderr reader registers with the shared reactor");
        let mut child_watch = port
            .watch(child)
            .expect("child exit joins the same reactor");
        let mut stdout = LinuxPipeReader::new(stdout_descriptor, &port, stdout_token);
        let mut stderr = LinuxPipeReader::new(stderr_descriptor, &port, stderr_token);
        let mut pending_probe = [0_u8; 1];
        let wake_count = Arc::new(WakeCount(AtomicUsize::new(0)));
        let reader_waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&reader_waker);
        assert!(matches!(
            Pin::new(&mut stdout).poll_read(&mut context, &mut pending_probe),
            Poll::Pending
        ));
        assert!(matches!(
            Pin::new(&mut stderr).poll_read(&mut context, &mut pending_probe),
            Poll::Pending
        ));
        let mut events = [super::super::linux_exit::LinuxExitEvent { token: u64::MAX }; 4];
        let premature_events = port
            .poll(&mut events)
            .expect("held pipe sources have no readiness");
        assert_eq!(premature_events, 0);
        let release_byte = [b'x'];
        let release_written = unsafe {
            libc::write(
                release_barrier[1],
                release_byte.as_ptr().cast(),
                release_byte.len(),
            )
        };
        unsafe { libc::close(release_barrier[1]) };
        assert_eq!(release_written, 1);

        let mut stdout_wakes = 0;
        let mut stderr_wakes = 0;
        let mut child_exits = 0;
        let mut batches = 0;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let mut stdout_payload = [0_u8; 3];
        let mut stderr_payload = [0_u8; 3];
        let mut eof_probe = [0_u8; 1];
        let mut stdout_length = 0;
        let mut stdout_first_read = 0;
        let mut stderr_length = 0;
        let mut child_status = None;
        while !stdout_eof || !stderr_eof || child_status.is_none() {
            let event_count = port
                .wait(&mut events)
                .expect("reactor waits for child output or exit readiness");
            batches += 1;
            for event in events.iter().take(event_count) {
                if event.token == stdout.token() {
                    stdout_wakes += usize::from(stdout.wake_read_ready());
                    if stdout_length == stdout_payload.len() {
                        match Pin::new(&mut stdout).poll_read(&mut context, &mut eof_probe) {
                            Poll::Ready(Ok(0)) => stdout_eof = true,
                            Poll::Ready(Ok(count)) => panic!("unexpected stdout bytes: {count}"),
                            Poll::Ready(Err(code)) => panic!("stdout read failed: {code}"),
                            Poll::Pending => {}
                        }
                    } else {
                        let output_limit = (stdout_length + 2).min(stdout_payload.len());
                        match Pin::new(&mut stdout).poll_read(
                            &mut context,
                            &mut stdout_payload[stdout_length..output_limit],
                        ) {
                            Poll::Ready(Ok(0)) => stdout_eof = true,
                            Poll::Ready(Ok(count)) => {
                                if stdout_length == 0 {
                                    stdout_first_read = count;
                                }
                                stdout_length += count;
                            }
                            Poll::Ready(Err(code)) => panic!("stdout read failed: {code}"),
                            Poll::Pending => {}
                        }
                    }
                } else if event.token == stderr.token() {
                    stderr_wakes += usize::from(stderr.wake_read_ready());
                    if stderr_length == stderr_payload.len() {
                        match Pin::new(&mut stderr).poll_read(&mut context, &mut eof_probe) {
                            Poll::Ready(Ok(0)) => stderr_eof = true,
                            Poll::Ready(Ok(count)) => panic!("unexpected stderr bytes: {count}"),
                            Poll::Ready(Err(code)) => panic!("stderr read failed: {code}"),
                            Poll::Pending => {}
                        }
                    } else {
                        match Pin::new(&mut stderr)
                            .poll_read(&mut context, &mut stderr_payload[stderr_length..])
                        {
                            Poll::Ready(Ok(0)) => stderr_eof = true,
                            Poll::Ready(Ok(count)) => stderr_length += count,
                            Poll::Ready(Err(code)) => panic!("stderr read failed: {code}"),
                            Poll::Pending => {}
                        }
                    }
                } else if event.token == child_watch.token() {
                    child_exits += 1;
                    child_status = child_watch
                        .reap()
                        .expect("ready child reaps through nonblocking pidfd lifecycle");
                }
            }
        }
        assert_eq!(stdout_wakes, 1);
        assert_eq!(stderr_wakes, 1);
        assert_eq!(child_exits, 1);
        assert_eq!(child_status.map(|status| status.code), Some(Some(0)));
        assert_eq!(wake_count.0.load(Ordering::SeqCst), 2);
        assert_eq!(stdout_length, stdout_payload.len());
        assert_eq!(stdout_first_read, 2);
        assert_eq!(stderr_length, stderr_payload.len());
        assert_eq!(&stdout_payload, b"out");
        assert_eq!(&stderr_payload, b"err");
        let post_eof_events = port
            .poll(&mut events)
            .expect("EOF sources have been removed from level-triggered epoll");
        assert_eq!(post_eof_events, 0);
        std::println!(
            "{{\"premature_events\":{premature_events},\"ready_batches\":{batches},\"stdout_wakes\":{stdout_wakes},\"stderr_wakes\":{stderr_wakes},\"executor_wakes\":{},\"child_exits\":{child_exits},\"stdout_payload\":\"{}\",\"stdout_first_read\":{stdout_first_read},\"stderr_payload\":\"{}\",\"stdout_eof\":{},\"stderr_eof\":{},\"post_eof_events\":{post_eof_events}}}",
            wake_count.0.load(Ordering::SeqCst),
            core::str::from_utf8(&stdout_payload).expect("stdout is utf-8"),
            core::str::from_utf8(&stderr_payload).expect("stderr is utf-8"),
            stdout_eof,
            stderr_eof
        );
    }
}
