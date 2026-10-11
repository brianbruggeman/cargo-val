//! Async child-pipe reads from the shared macOS kqueue.

use core::ffi::c_int;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use proxima_core::io::AsyncRead;

use super::macos_exit::MacosExitPort;
use super::unix_pipe_pair::OwnedDescriptor;

/// Caller-buffer reader whose readiness token belongs to `MacosExitPort`.
#[derive(Debug)]
pub struct MacosPipeReader<'port> {
    descriptor: OwnedDescriptor,
    port: &'port MacosExitPort,
    token: u64,
    waker: Option<Waker>,
    eof: bool,
}

impl<'port> MacosPipeReader<'port> {
    /// Connects an owned descriptor and its registered token to the shared kqueue.
    #[must_use]
    pub(crate) fn new(descriptor: OwnedDescriptor, port: &'port MacosExitPort, token: u64) -> Self {
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
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn pause(&self) -> Result<(), c_int> {
        self.port.unwatch_descriptor(self.descriptor.raw())
    }

    /// Registers the same descriptor after the caller has consumed that artifact.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn resume(&mut self) -> Result<(), c_int> {
        self.token = self.port.watch_descriptor(self.descriptor.raw())?;
        Ok(())
    }

    /// Wakes this pending reader after its token appears in a reactor batch.
    pub fn wake_read_ready(&mut self) -> bool {
        if let Some(waker) = self.waker.take() {
            waker.wake();
            true
        } else {
            false
        }
    }
}

impl AsyncRead for MacosPipeReader<'_> {
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
            if let Ok(count) = usize::try_from(read)
                && count > 0
            {
                reader.waker = None;
                return Poll::Ready(Ok(count));
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
    unsafe { *libc::__error() }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll};
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    use proxima_core::io::AsyncRead;

    use super::MacosPipeReader;
    use crate::host::macos_exit::{MacosExitEvent, MacosExitPort};
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

    fn run_pipe_child(pipes: &UnixPipePair, release_barrier: [libc::c_int; 2]) -> ! {
        unsafe {
            libc::close(release_barrier[1]);
            libc::close(pipes.stdout().parent_read().raw());
            libc::close(pipes.stderr().parent_read().raw());
            let mut release = 0_u8;
            let count = libc::read(
                release_barrier[0],
                (&raw mut release).cast(),
                size_of_val(&release),
            );
            libc::close(release_barrier[0]);
            if count != 1 {
                libc::_exit(1);
            }
        }
        let standard_output = b"out";
        let standard_error = b"err";
        let stdout_count = unsafe {
            libc::write(
                pipes.stdout().child_write().raw(),
                standard_output.as_ptr().cast(),
                standard_output.len(),
            )
        };
        let stderr_count = unsafe {
            libc::write(
                pipes.stderr().child_write().raw(),
                standard_error.as_ptr().cast(),
                standard_error.len(),
            )
        };
        unsafe {
            libc::_exit(i32::from(
                stdout_count != standard_output.len().cast_signed()
                    || stderr_count != standard_error.len().cast_signed(),
            ));
        }
    }

    fn release_child(barrier_write: libc::c_int) {
        let release_byte = *b"x";
        let release_count = unsafe {
            libc::write(
                barrier_write,
                release_byte.as_ptr().cast(),
                release_byte.len(),
            )
        };
        unsafe { libc::close(barrier_write) };
        assert_eq!(release_count, 1);
    }

    fn assert_pending(reader: &mut MacosPipeReader<'_>, context: &mut Context<'_>) {
        let mut pending_probe = [0_u8; 1];
        assert!(matches!(
            Pin::new(reader).poll_read(context, &mut pending_probe),
            Poll::Pending
        ));
    }

    fn read_step(
        reader: &mut MacosPipeReader<'_>,
        context: &mut Context<'_>,
        payload: &mut [u8; 3],
        length: &mut usize,
        chunk_limit: usize,
    ) -> (bool, Option<usize>) {
        let mut eof_probe = [0_u8; 1];
        let destination = if *length < payload.len() {
            let output_limit = (*length + chunk_limit).min(payload.len());
            &mut payload[*length..output_limit]
        } else {
            &mut eof_probe[..]
        };
        match Pin::new(reader).poll_read(context, destination) {
            Poll::Ready(Ok(0)) => (true, None),
            Poll::Ready(Ok(count)) if *length < payload.len() => {
                let first_read = (*length == 0).then_some(count);
                *length += count;
                (false, first_read)
            }
            Poll::Ready(Ok(count)) => panic!("unexpected stream bytes: {count}"),
            Poll::Ready(Err(code)) => panic!("stream read failed: {code}"),
            Poll::Pending => (false, None),
        }
    }

    #[test]
    fn c42_reads_both_child_streams_from_the_shared_kqueue() {
        let pipes = UnixPipePair::new().expect("stdout and stderr pipes are created");
        let mut release_barrier = [-1; 2];
        assert_eq!(unsafe { libc::pipe(release_barrier.as_mut_ptr()) }, 0);
        let process_id = unsafe { libc::fork() };
        assert!(process_id >= 0, "pipe fixture child forks");
        if process_id == 0 {
            run_pipe_child(&pipes, release_barrier);
        }

        unsafe { libc::close(release_barrier[0]) };
        let (stdout_descriptor, stderr_descriptor) = pipes.into_parent_reads();
        let port = MacosExitPort::new().expect("shared kqueue port is created");
        let stdout_token = port
            .watch_descriptor(stdout_descriptor.raw())
            .expect("stdout reader registers with shared kqueue");
        let stderr_token = port
            .watch_descriptor(stderr_descriptor.raw())
            .expect("stderr reader registers with shared kqueue");
        let mut child_watch = port
            .watch(process_id)
            .expect("child exit joins the same kqueue");
        let mut stdout = MacosPipeReader::new(stdout_descriptor, &port, stdout_token);
        let mut stderr = MacosPipeReader::new(stderr_descriptor, &port, stderr_token);
        let wake_count = Arc::new(WakeCount(AtomicUsize::new(0)));
        let reader_waker = Waker::from(Arc::clone(&wake_count));
        let mut context = Context::from_waker(&reader_waker);
        assert_pending(&mut stdout, &mut context);
        assert_pending(&mut stderr, &mut context);
        let mut events = [MacosExitEvent {
            token: u64::MAX,
            ..MacosExitEvent::default()
        }; 4];
        let premature_events = port
            .poll(&mut events)
            .expect("held sources have no readiness");
        assert_eq!(premature_events, 0);
        release_child(release_barrier[1]);

        let mut stdout_payload = [0_u8; 3];
        let mut stderr_payload = [0_u8; 3];
        let stderr_chunk_limit = stderr_payload.len();
        let mut stdout_length = 0;
        let mut stdout_first_read = 0;
        let mut stderr_length = 0;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let (mut stdout_wakes, mut stderr_wakes) = (0, 0);
        let mut child_exits = 0;
        let mut batches = 0;
        let mut child_status = None;
        while !stdout_eof || !stderr_eof || child_status.is_none() {
            let event_count = port.wait(&mut events).expect("kqueue waits for readiness");
            batches += 1;
            for event in events.iter().take(event_count) {
                if event.token == stdout.token() {
                    stdout_wakes += usize::from(stdout.wake_read_ready());
                    let (eof, first_read) = read_step(
                        &mut stdout,
                        &mut context,
                        &mut stdout_payload,
                        &mut stdout_length,
                        2,
                    );
                    stdout_eof |= eof;
                    stdout_first_read = first_read.unwrap_or(stdout_first_read);
                } else if event.token == stderr.token() {
                    stderr_wakes += usize::from(stderr.wake_read_ready());
                    let (eof, _) = read_step(
                        &mut stderr,
                        &mut context,
                        &mut stderr_payload,
                        &mut stderr_length,
                        stderr_chunk_limit,
                    );
                    stderr_eof |= eof;
                } else if event.token == child_watch.token() {
                    child_exits += 1;
                    child_watch.observe(event);
                    child_status = child_watch
                        .reap()
                        .expect("ready child reaps through nonblocking pid lifecycle");
                }
            }
        }
        assert_eq!(stdout_length, stdout_payload.len());
        assert_eq!(stdout_first_read, 2);
        assert_eq!(stderr_length, stderr_payload.len());
        assert_eq!(&stdout_payload, b"out");
        assert_eq!(&stderr_payload, b"err");
        assert_eq!(stdout_wakes, 1);
        assert_eq!(stderr_wakes, 1);
        assert_eq!(wake_count.0.load(Ordering::SeqCst), 2);
        assert_eq!(child_exits, 1);
        assert_eq!(child_status.map(|status| status.code), Some(Some(0)));
        let post_eof_events = port.poll(&mut events).expect("EOF sources were removed");
        assert_eq!(post_eof_events, 0);
        std::println!(
            "{{\"premature_events\":{premature_events},\"ready_batches\":{batches},\"stdout_wakes\":{stdout_wakes},\"stderr_wakes\":{stderr_wakes},\"task_wake_notifications\":{},\"child_exits\":{child_exits},\"stdout_payload\":\"out\",\"stdout_first_read\":{stdout_first_read},\"stderr_payload\":\"err\",\"stdout_eof\":{},\"stderr_eof\":{},\"post_eof_events\":{post_eof_events}}}",
            wake_count.0.load(Ordering::SeqCst),
            stdout_eof,
            stderr_eof
        );
    }
}
