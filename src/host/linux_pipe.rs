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

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use proxima_core::io::AsyncRead;

    use super::LinuxPipeReader;
    use crate::artifact_stream::ArtifactFramer;
    use crate::cargo_message::TestArtifact;
    use crate::cargo_source::{CargoSource, CargoSourceStep};
    use crate::host::linux_exit::{LinuxExitEvent, LinuxExitPort};
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

    struct StreamProgress {
        name: &'static str,
        payload: [u8; 3],
        length: usize,
        first_read: usize,
        eof: bool,
        wakes: usize,
    }

    impl StreamProgress {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                payload: [0_u8; 3],
                length: 0,
                first_read: 0,
                eof: false,
                wakes: 0,
            }
        }

        fn service(
            &mut self,
            reader: &mut LinuxPipeReader<'_>,
            context: &mut Context<'_>,
            chunk: usize,
        ) {
            self.wakes += usize::from(reader.wake_read_ready());
            if self.length == self.payload.len() {
                self.probe_eof(reader, context);
                return;
            }
            let limit = (self.length + chunk).min(self.payload.len());
            let destination = &mut self.payload[self.length..limit];
            match Pin::new(reader).poll_read(context, destination) {
                Poll::Ready(Ok(0)) => self.eof = true,
                Poll::Ready(Ok(count)) => {
                    if self.length == 0 {
                        self.first_read = count;
                    }
                    self.length += count;
                }
                Poll::Ready(Err(code)) => panic!("{} read failed: {code}", self.name),
                Poll::Pending => {}
            }
        }

        fn probe_eof(&mut self, reader: &mut LinuxPipeReader<'_>, context: &mut Context<'_>) {
            let mut eof_probe = [0_u8; 1];
            match Pin::new(reader).poll_read(context, &mut eof_probe) {
                Poll::Ready(Ok(0)) => self.eof = true,
                Poll::Ready(Ok(count)) => panic!("unexpected {} bytes: {count}", self.name),
                Poll::Ready(Err(code)) => panic!("{} read failed: {code}", self.name),
                Poll::Pending => {}
            }
        }
    }

    fn fork_writer_child(pipes: &UnixPipePair, release_barrier: [libc::c_int; 2]) -> libc::pid_t {
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "pipe fixture child forks");
        if child != 0 {
            return child;
        }
        unsafe {
            libc::close(release_barrier[1]);
            libc::close(pipes.stdout().parent_read().raw());
            libc::close(pipes.stderr().parent_read().raw());
            let mut release = 0_u8;
            let read = libc::read(
                release_barrier[0],
                (&raw mut release).cast(),
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
        let stderr_written =
            unsafe { libc::write(stderr, standard_error.as_ptr().cast(), standard_error.len()) };
        let short_write = usize::try_from(stdout_written) != Ok(standard_output.len())
            || usize::try_from(stderr_written) != Ok(standard_error.len());
        unsafe { libc::_exit(i32::from(short_write)) }
    }

    #[test]
    fn c40_reads_both_child_streams_without_intermediate_buffers() {
        let pipes = UnixPipePair::new().expect("stdout and stderr pipes are created");
        let mut release_barrier = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(release_barrier.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let child = fork_writer_child(&pipes, release_barrier);

        unsafe { libc::close(release_barrier[0]) };
        let (stdout_descriptor, stderr_descriptor) = pipes.into_parent_reads();
        let port = LinuxExitPort::new().expect("shared epoll port is created");
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
        let release_byte = b"x";
        let release_written = unsafe {
            libc::write(
                release_barrier[1],
                release_byte.as_ptr().cast(),
                release_byte.len(),
            )
        };
        unsafe { libc::close(release_barrier[1]) };
        assert_eq!(release_written, 1);

        let mut child_exits = 0;
        let mut batches = 0;
        let mut out = StreamProgress::new("stdout");
        let mut err = StreamProgress::new("stderr");
        let mut child_status = None;
        while !out.eof || !err.eof || child_status.is_none() {
            let event_count = port
                .wait(&mut events)
                .expect("reactor waits for child output or exit readiness");
            batches += 1;
            for event in events.iter().take(event_count) {
                if event.token == stdout.token() {
                    out.service(&mut stdout, &mut context, 2);
                } else if event.token == stderr.token() {
                    err.service(&mut stderr, &mut context, 3);
                } else if event.token == child_watch.token() {
                    child_exits += 1;
                    child_status = child_watch
                        .reap()
                        .expect("ready child reaps through nonblocking pidfd lifecycle");
                }
            }
        }
        assert_eq!(out.wakes, 1);
        assert_eq!(err.wakes, 1);
        assert_eq!(child_exits, 1);
        assert_eq!(child_status.map(|status| status.code), Some(Some(0)));
        assert_eq!(wake_count.0.load(Ordering::SeqCst), 2);
        assert_eq!(out.length, out.payload.len());
        assert_eq!(out.first_read, 2);
        assert_eq!(err.length, err.payload.len());
        assert_eq!(&out.payload, b"out");
        assert_eq!(&err.payload, b"err");
        let post_eof_events = port
            .poll(&mut events)
            .expect("EOF sources have been removed from level-triggered epoll");
        assert_eq!(post_eof_events, 0);
        std::println!(
            "{{\"premature_events\":{premature_events},\"ready_batches\":{batches},\"stdout_wakes\":{},\"stderr_wakes\":{},\"executor_wakes\":{},\"child_exits\":{child_exits},\"stdout_payload\":\"{}\",\"stdout_first_read\":{},\"stderr_payload\":\"{}\",\"stdout_eof\":{},\"stderr_eof\":{},\"post_eof_events\":{post_eof_events}}}",
            out.wakes,
            err.wakes,
            wake_count.0.load(Ordering::SeqCst),
            core::str::from_utf8(&out.payload).expect("stdout is utf-8"),
            out.first_read,
            core::str::from_utf8(&err.payload).expect("stderr is utf-8"),
            out.eof,
            err.eof
        );
    }
    const CARGO_RECORDS: &[u8] = b"{\"reason\":\"build-finished\",\"success\":true}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/first\"}\n";

    struct ReleaseWriter(libc::c_int);

    impl ReleaseWriter {
        fn release(self) {
            let release = *b"x";
            let written = unsafe { libc::write(self.0, release.as_ptr().cast(), release.len()) };
            assert_eq!(written, 1);
        }
    }

    impl Drop for ReleaseWriter {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    fn fork_fixture_producer(
        pipes: &UnixPipePair,
        release_barrier: [libc::c_int; 2],
    ) -> libc::pid_t {
        let process_id = unsafe { libc::fork() };
        assert!(process_id >= 0, "fixture producer forks");
        if process_id == 0 {
            unsafe {
                libc::close(release_barrier[1]);
                libc::close(pipes.stdout().parent_read().raw());
                libc::close(pipes.stderr().parent_read().raw());
                let written = libc::write(
                    pipes.stdout().child_write().raw(),
                    CARGO_RECORDS.as_ptr().cast(),
                    CARGO_RECORDS.len(),
                );
                let mut release = 0_u8;
                let released = libc::read(release_barrier[0], (&raw mut release).cast(), 1);
                libc::close(release_barrier[0]);
                libc::_exit(i32::from(
                    written != CARGO_RECORDS.len().cast_signed() || released != 1,
                ));
            }
        }
        process_id
    }

    fn drain_stdout(
        source: &mut CargoSource<'_>,
        reader: &mut LinuxPipeReader<'_>,
        framer: &mut ArtifactFramer<'_>,
        scratch: &mut [u8],
        context: &mut Context<'_>,
    ) -> bool {
        loop {
            match source.poll_next(Pin::new(&mut *reader), framer, scratch, 4, context) {
                Poll::Ready(Ok(
                    CargoSourceStep::Read { .. }
                    | CargoSourceStep::NeedMore
                    | CargoSourceStep::Ignored
                    | CargoSourceStep::BinBuilt(_)
                    | CargoSourceStep::BuildFinished,
                )) => {}
                Poll::Ready(Ok(CargoSourceStep::Eof)) => return true,
                Poll::Pending => return false,
                Poll::Ready(Ok(CargoSourceStep::Artifact(_) | CargoSourceStep::BuildScript(_))) => {
                    panic!("only one artifact was emitted")
                }
                Poll::Ready(Err(error)) => panic!("stdout drain failed: {error:?}"),
            }
        }
    }

    #[test]
    fn c49_native_child_stdout_yields_before_the_child_closes_it() {
        let pipes = UnixPipePair::new().expect("native child capture pipes are created");
        let mut release_barrier = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(release_barrier.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let process_id = fork_fixture_producer(&pipes, release_barrier);

        unsafe { libc::close(release_barrier[0]) };
        let release_writer = ReleaseWriter(release_barrier[1]);
        let (stdout_descriptor, _stderr_descriptor) = pipes.into_parent_reads();
        let port = LinuxExitPort::new().expect("shared epoll port is created");
        let stdout_token = port
            .watch_descriptor(stdout_descriptor.raw())
            .expect("stdout readiness is registered");
        let mut child_watch = port.watch(process_id).expect("child exit is registered");
        let mut reader = LinuxPipeReader::new(stdout_descriptor, &port, stdout_token);
        let mut source_storage = [0_u8; 256];
        let mut frame_storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut source = CargoSource::new(&mut source_storage).expect("source storage exists");
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 255).expect("frame storage fits records");
        let mut events = [LinuxExitEvent { token: u64::MAX }; 4];
        let mut context = Context::from_waker(Waker::noop());
        let mut ignored_records = 0;

        let child_alive_at_artifact = loop {
            match source.poll_next(
                Pin::new(&mut reader),
                &mut framer,
                &mut scratch,
                4,
                &mut context,
            ) {
                Poll::Ready(Ok(CargoSourceStep::Artifact(TestArtifact {
                    executable: path,
                    ..
                }))) => {
                    assert_eq!(path, "/fixture/first");
                    break child_watch.reap().expect("exit probe succeeds").is_none();
                }
                Poll::Ready(Ok(
                    CargoSourceStep::Ignored
                    | CargoSourceStep::BinBuilt(_)
                    | CargoSourceStep::BuildFinished,
                )) => ignored_records += 1,
                Poll::Ready(Ok(
                    CargoSourceStep::Read { .. }
                    | CargoSourceStep::NeedMore
                    | CargoSourceStep::BuildScript(_),
                )) => {}
                Poll::Ready(Ok(CargoSourceStep::Eof)) => panic!("child closed before artifact"),
                Poll::Ready(Err(error)) => panic!("native source failed: {error:?}"),
                Poll::Pending => {
                    let ready = port.wait(&mut events).expect("readiness arrives");
                    for event in events.iter().take(ready) {
                        if event.token == stdout_token {
                            reader.wake_read_ready();
                        }
                    }
                }
            }
        };

        assert_eq!(ignored_records, 1);
        assert!(child_alive_at_artifact);
        assert!(matches!(
            source.poll_next(
                Pin::new(&mut reader),
                &mut framer,
                &mut scratch,
                4,
                &mut context,
            ),
            Poll::Pending
        ));
        release_writer.release();

        let mut stdout_eof = false;
        let mut child_status = None;
        while !stdout_eof || child_status.is_none() {
            let ready = port
                .wait(&mut events)
                .expect("completion readiness arrives");
            for event in events.iter().take(ready) {
                if event.token == stdout_token {
                    reader.wake_read_ready();
                }
                if event.token == child_watch.token() {
                    child_status = child_watch.reap().expect("child is reaped");
                }
            }
            if !stdout_eof {
                stdout_eof = drain_stdout(
                    &mut source,
                    &mut reader,
                    &mut framer,
                    &mut scratch,
                    &mut context,
                );
            }
        }
        assert_eq!(child_status.map(|status| status.code), Some(Some(0)));
    }
}
