//! Incremental Cargo stdout parsing over Proxima's poll-driven read contract.

use core::pin::Pin;
use core::task::{Context, Poll};

use proxima_core::io::AsyncRead;

use crate::ParseError;
use crate::artifact_stream::{ArtifactFramer, ArtifactStreamError, RecordFault, RecordStep};
use crate::cargo_message::{BuildScript, CargoMessage, TestArtifact, parse_cargo_message};

/// One source read or Cargo record failure.
#[derive(Debug, PartialEq, Eq)]
pub enum CargoSourceError<ReadError> {
    /// Source storage cannot accept any bytes.
    EmptyBuffer,
    /// The reader failed while producing a chunk.
    Read(ReadError),
    /// The reader reported more bytes than the supplied storage can hold.
    InvalidReadLength { length: usize, capacity: usize },
    /// The framer rejected a Cargo JSON line or the C2 parser rejected a framed one.
    Record(RecordFault<ParseError>),
    /// The framer rejected end of stream or its own state outside a record step.
    Framing(ArtifactStreamError),
}

/// One poll result; every variant except `NeedMore` and `Ignored` consumed a read or EOF.
#[derive(Debug)]
pub enum CargoSourceStep<'path> {
    /// One test executable can be launched while Cargo stdout remains open.
    Artifact(TestArtifact<'path>),
    /// One build script reported search paths and variables for its package's test executables.
    BuildScript(BuildScript<'path>),
    /// A non-test `bin` executable of the package with this id was built; its `CARGO_BIN_EXE_<name>` path now exists.
    BinBuilt {
        /// package id such as `path+file:///repo/parser#0.3.1`.
        package_id: &'path str,
        /// the uplifted path such as `/repo/target/debug/parser`, as cargo reported it.
        executable: &'path str,
    },
    /// Cargo finished building; no artifact follows.
    BuildFinished {
        /// false when a unit failed to build, so executables still waiting for a bin will never get it.
        success: bool,
    },
    /// The reader produced bytes in this step, or zero when it reported EOF.
    Read {
        /// Bytes newly read from Cargo stdout in this step.
        bytes: usize,
    },
    /// Input was staged without completing a record, or no input was pending.
    NeedMore,
    /// A complete non-test Cargo record was consumed in this step.
    Ignored,
    /// Cargo stdout reached EOF and all framed records were drained.
    Eof,
}

/// Caller-owned source storage and offsets for one Cargo stdout reader.
///
/// The source lends each complete executable path while retaining any unread
/// suffix in the same input buffer. Its reader is supplied by the host loop so
/// readiness events can wake that reader between calls to [`Self::poll_next`].
pub struct CargoSource<'buffer> {
    buffer: &'buffer mut [u8],
    consumed: usize,
    filled: usize,
    eof: bool,
}

impl<'buffer> CargoSource<'buffer> {
    /// Binds fixed caller storage to one child stdout stream.
    ///
    /// # Errors
    ///
    /// Returns [`CargoSourceError::EmptyBuffer`] when `buffer` has no capacity.
    pub fn new(
        buffer: &'buffer mut [u8],
    ) -> Result<Self, CargoSourceError<core::convert::Infallible>> {
        if buffer.is_empty() {
            return Err(CargoSourceError::EmptyBuffer);
        }
        Ok(Self {
            buffer,
            consumed: 0,
            filled: 0,
            eof: false,
        })
    }

    /// Polls one read or parse step without waiting for producer EOF.
    ///
    /// The returned path borrows the source, framer, or scratch storage; no next
    /// read can overwrite it while the caller uses the path.
    pub fn poll_next<'call, Reader: AsyncRead>(
        &'call mut self,
        mut reader: Pin<&mut Reader>,
        framer: &'call mut ArtifactFramer<'_>,
        scratch: &'call mut [u8],
        max_json_depth: usize,
        context: &mut Context<'_>,
    ) -> Poll<Result<CargoSourceStep<'call>, CargoSourceError<Reader::Error>>> {
        let frame_ready = match framer.frame_ready() {
            Ok(ready) => ready,
            Err(error) => return Poll::Ready(Err(CargoSourceError::Framing(error))),
        };
        if frame_ready || self.consumed < self.filled {
            let chunk = if frame_ready {
                &[][..]
            } else {
                &self.buffer[self.consumed..self.filled]
            };
            return Poll::Ready(
                Self::record_step(framer, chunk, scratch, max_json_depth).map(
                    |(step, consumed_bytes)| {
                        self.consumed += consumed_bytes;
                        step
                    },
                ),
            );
        }

        if self.eof {
            return Poll::Ready(Self::finish(framer));
        }
        match reader.as_mut().poll_read(context, self.buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(CargoSourceError::Read(error))),
            Poll::Ready(Ok(0)) => {
                self.eof = true;
                Poll::Ready(Ok(CargoSourceStep::Read { bytes: 0 }))
            }
            Poll::Ready(Ok(length)) if length <= self.buffer.len() => {
                self.consumed = 0;
                self.filled = length;
                Poll::Ready(Ok(CargoSourceStep::Read { bytes: length }))
            }
            Poll::Ready(Ok(length)) => Poll::Ready(Err(CargoSourceError::InvalidReadLength {
                length,
                capacity: self.buffer.len(),
            })),
        }
    }

    /// Parses one chunk lent by whoever owns its storage, without copying it.
    ///
    /// The returned count is how much of `chunk` was consumed; the caller replays
    /// the unconsumed tail on the next call and releases the storage only once
    /// the count reaches the chunk length. Completion-based readers (overlapped
    /// pipes) lend their completed slot bytes here instead of a [`AsyncRead`].
    pub fn poll_lent<'call>(
        framer: &'call mut ArtifactFramer<'_>,
        chunk: &'call [u8],
        scratch: &'call mut [u8],
        max_json_depth: usize,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(CargoSourceStep<'call>, usize), CargoSourceError<core::convert::Infallible>>>
    {
        let frame_ready = match framer.frame_ready() {
            Ok(ready) => ready,
            Err(error) => return Poll::Ready(Err(CargoSourceError::Framing(error))),
        };
        if !frame_ready && chunk.is_empty() {
            return Poll::Ready(Ok((CargoSourceStep::NeedMore, 0)));
        }
        let lent = if frame_ready { &[][..] } else { chunk };
        Poll::Ready(Self::record_step(framer, lent, scratch, max_json_depth))
    }

    /// Runs the end-of-stream drain for a completion owner that observed EOF.
    pub fn poll_lent_eof<'call>(
        framer: &'call mut ArtifactFramer<'_>,
        scratch: &'call mut [u8],
        max_json_depth: usize,
        _context: &mut Context<'_>,
    ) -> Poll<Result<CargoSourceStep<'call>, CargoSourceError<core::convert::Infallible>>> {
        let frame_ready = match framer.frame_ready() {
            Ok(ready) => ready,
            Err(error) => return Poll::Ready(Err(CargoSourceError::Framing(error))),
        };
        if frame_ready {
            return Poll::Ready(
                Self::record_step(framer, &[], scratch, max_json_depth).map(|(step, _)| step),
            );
        }
        Poll::Ready(Self::finish(framer))
    }

    fn finish<ReadError>(
        framer: &mut ArtifactFramer<'_>,
    ) -> Result<CargoSourceStep<'static>, CargoSourceError<ReadError>> {
        framer
            .finish()
            .map(|()| CargoSourceStep::Eof)
            .map_err(CargoSourceError::Framing)
    }

    fn record_step<'call, ReadError>(
        framer: &'call mut ArtifactFramer<'_>,
        chunk: &'call [u8],
        scratch: &'call mut [u8],
        max_json_depth: usize,
    ) -> Result<(CargoSourceStep<'call>, usize), CargoSourceError<ReadError>> {
        let max_record_bytes = framer.max_record_bytes();
        let step = framer
            .next_record(chunk, |record| {
                parse_selection(record, scratch, max_record_bytes, max_json_depth)
            })
            .map_err(CargoSourceError::Record)?;
        Ok(match step {
            RecordStep::NeedMore { consumed_bytes } => (CargoSourceStep::NeedMore, consumed_bytes),
            RecordStep::Parsed {
                parsed: Some(CargoMessage::TestArtifact(artifact)),
                consumed_bytes,
            } => (CargoSourceStep::Artifact(artifact), consumed_bytes),
            RecordStep::Parsed {
                parsed: Some(CargoMessage::BuildScript(script)),
                consumed_bytes,
            } => (CargoSourceStep::BuildScript(script), consumed_bytes),
            RecordStep::Parsed {
                parsed:
                    Some(CargoMessage::BinArtifact {
                        package_id,
                        executable,
                    }),
                consumed_bytes,
            } => (
                CargoSourceStep::BinBuilt {
                    package_id,
                    executable,
                },
                consumed_bytes,
            ),
            RecordStep::Parsed {
                parsed: Some(CargoMessage::BuildFinished { success }),
                consumed_bytes,
            } => (CargoSourceStep::BuildFinished { success }, consumed_bytes),
            RecordStep::Parsed {
                parsed: Some(CargoMessage::Ignored) | None,
                consumed_bytes,
            } => (CargoSourceStep::Ignored, consumed_bytes),
        })
    }
}

pub(crate) fn parse_selection<'path>(
    record: &'path [u8],
    scratch: &'path mut [u8],
    max_record_bytes: usize,
    max_json_depth: usize,
) -> Result<Option<CargoMessage<'path>>, ParseError> {
    Ok(
        match parse_cargo_message(record, scratch, max_record_bytes, max_json_depth)? {
            CargoMessage::Ignored => None,
            message => Some(message),
        },
    )
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};

    use proxima_core::io::AsyncRead;

    use super::{CargoSource, CargoSourceError, CargoSourceStep, parse_selection};
    use crate::artifact_stream::{ArtifactFramer, RecordStep};
    use crate::cargo_message::{CargoMessage, TestArtifact};
    #[cfg(target_os = "macos")]
    use crate::host::macos_exit::{MacosExitEvent, MacosExitPort};
    #[cfg(target_os = "macos")]
    use crate::host::macos_pipe::MacosPipeReader;
    #[cfg(target_os = "macos")]
    use crate::host::unix_pipe_pair::UnixPipePair;

    const CARGO_RECORDS: &[u8] = b"{\"reason\":\"build-finished\",\"success\":true}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/first\"}\n";

    #[derive(Clone, Copy)]
    struct TimingObservation {
        artifact_emitted: bool,
        eof_seen: bool,
    }

    fn artifact_before_eof(observation: TimingObservation) -> bool {
        observation.artifact_emitted && !observation.eof_seen
    }

    struct HeldReader {
        records: Option<&'static [u8]>,
        release_eof: bool,
        read_calls: usize,
        eof_seen: bool,
    }

    impl AsyncRead for HeldReader {
        type Error = Infallible;

        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<Result<usize, Self::Error>> {
            let reader = self.get_mut();
            reader.read_calls += 1;
            if let Some(records) = reader.records.take() {
                buffer[..records.len()].copy_from_slice(records);
                return Poll::Ready(Ok(records.len()));
            }
            if reader.release_eof {
                reader.eof_seen = true;
                Poll::Ready(Ok(0))
            } else {
                Poll::Pending
            }
        }
    }

    #[test]
    fn c49_emits_borrowed_artifact_before_reader_eof() {
        let (observed, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut source_storage = [0_u8; 256];
            let mut frame_storage = [0_u8; 256];
            let mut scratch = [0_u8; 64];
            let mut source = CargoSource::new(&mut source_storage).expect("source storage exists");
            let source_pointer = source.buffer.as_ptr();
            let path_offset = CARGO_RECORDS
                .windows(b"/fixture/first".len())
                .position(|window| window == b"/fixture/first")
                .expect("fixture path exists");
            let mut framer = ArtifactFramer::new(&mut frame_storage, 255)
                .expect("record storage fits the fixture");
            let mut reader = HeldReader {
                records: Some(CARGO_RECORDS),
                release_eof: false,
                read_calls: 0,
                eof_seen: false,
            };
            let mut context = Context::from_waker(Waker::noop());
            let mut ignored_records = 0;
            let mut artifacts_before_eof = 0;
            let borrowed_pointer_identity = loop {
                let step = source.poll_next(
                    Pin::new(&mut reader),
                    &mut framer,
                    &mut scratch,
                    4,
                    &mut context,
                );
                match step {
                    Poll::Ready(Ok(CargoSourceStep::Artifact(TestArtifact {
                        executable: path,
                        ..
                    }))) => {
                        artifacts_before_eof +=
                            usize::from(artifact_before_eof(TimingObservation {
                                artifact_emitted: true,
                                eof_seen: reader.eof_seen,
                            }));
                        let pointer_identity =
                            path.as_ptr() == source_pointer.wrapping_add(path_offset);
                        assert_eq!(path, "/fixture/first");
                        break pointer_identity;
                    }
                    Poll::Ready(Ok(
                        CargoSourceStep::Ignored
                        | CargoSourceStep::BinBuilt { .. }
                        | CargoSourceStep::BuildFinished { .. },
                    )) => {
                        ignored_records += 1;
                    }
                    Poll::Ready(Ok(
                        CargoSourceStep::Read { .. }
                        | CargoSourceStep::NeedMore
                        | CargoSourceStep::BuildScript(_),
                    )) => {}
                    Poll::Ready(Ok(CargoSourceStep::Eof)) => {
                        panic!("producer EOF arrived before artifact")
                    }
                    Poll::Ready(Err(error)) => panic!("cargo source failed: {error:?}"),
                    Poll::Pending => panic!("held reader has already supplied complete records"),
                }
            };
            (
                artifacts_before_eof,
                ignored_records,
                borrowed_pointer_identity,
                reader.read_calls,
            )
        });
        assert_eq!(observed, (1, 1, true, 1));
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"artifacts_before_eof\":1,\"non_test_ignored\":1,\"whole_output_collections\":0,\"borrowed_pointer_identity\":1,\"source_read_calls\":1,\"source_step_allocations\":{allocations}}}"
        );
    }

    #[test]
    fn c49_eof_wait_control_withholds_the_same_artifact() {
        let mut reader = HeldReader {
            records: Some(CARGO_RECORDS),
            release_eof: false,
            read_calls: 0,
            eof_seen: false,
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut source_storage = [0_u8; 256];
        let first_read = Pin::new(&mut reader).poll_read(&mut context, &mut source_storage);
        assert!(matches!(first_read, Poll::Ready(Ok(length)) if length == CARGO_RECORDS.len()));
        let premature_eof = Pin::new(&mut reader).poll_read(&mut context, &mut source_storage);
        assert!(matches!(premature_eof, Poll::Pending));
        let withheld_observation = TimingObservation {
            artifact_emitted: false,
            eof_seen: reader.eof_seen,
        };
        assert!(!artifact_before_eof(withheld_observation));

        reader.release_eof = true;
        let mut eof_probe = [0_u8; 1];
        let end_of_source = Pin::new(&mut reader).poll_read(&mut context, &mut eof_probe);
        assert!(matches!(end_of_source, Poll::Ready(Ok(0))));
        let mut frame_storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 255).expect("frame storage fits records");
        let max_record_bytes = framer.max_record_bytes();
        let first_step = framer
            .next_record(&source_storage[..CARGO_RECORDS.len()], |record| {
                parse_selection(record, &mut scratch, max_record_bytes, 4)
            })
            .expect("held parse succeeds");
        let RecordStep::Parsed {
            parsed,
            consumed_bytes: consumed,
        } = first_step
        else {
            panic!("ignored record produced no parsed step");
        };
        assert!(matches!(parsed, Some(CargoMessage::BuildFinished { success: true })));
        let second_step = framer
            .next_record(&source_storage[consumed..CARGO_RECORDS.len()], |record| {
                parse_selection(record, &mut scratch, max_record_bytes, 4)
            })
            .expect("held artifact parse succeeds");
        let RecordStep::Parsed { parsed, .. } = second_step else {
            panic!("test artifact produced no parsed step");
        };
        let Some(CargoMessage::TestArtifact(artifact)) = parsed else {
            panic!("test artifact follows ignored record");
        };
        assert_eq!(artifact.executable, "/fixture/first");
        assert_eq!(artifact.package_id, "path+file:///fixture#0.1.0");
        assert_eq!(artifact.manifest_path, "/fixture/Cargo.toml");
        let artifact_emitted = true;
        let late_observation = TimingObservation {
            artifact_emitted,
            eof_seen: reader.eof_seen,
        };
        assert!(!artifact_before_eof(late_observation));
        std::println!(
            "{{\"producer_open\":1,\"eof_wait_pending\":1,\"artifact_after_eof_by_control\":1,\"same_comparator_rejected\":1}}"
        );
    }

    #[cfg(target_os = "macos")]
    struct ReleaseWriter(libc::c_int);

    #[cfg(target_os = "macos")]
    impl ReleaseWriter {
        fn release(self) {
            let release = *b"x";
            let written = unsafe { libc::write(self.0, release.as_ptr().cast(), release.len()) };
            assert_eq!(written, 1);
        }
    }

    #[cfg(target_os = "macos")]
    fn unset_events() -> [MacosExitEvent; 4] {
        [MacosExitEvent {
            token: u64::MAX,
            ..MacosExitEvent::default()
        }; 4]
    }

    #[cfg(target_os = "macos")]
    impl Drop for ReleaseWriter {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
    fn drain_stdout(
        source: &mut CargoSource<'_>,
        reader: &mut MacosPipeReader<'_>,
        framer: &mut ArtifactFramer<'_>,
        scratch: &mut [u8],
        context: &mut Context<'_>,
    ) -> bool {
        loop {
            match source.poll_next(Pin::new(&mut *reader), framer, scratch, 4, context) {
                Poll::Ready(Ok(
                    CargoSourceStep::Read { .. }
                    | CargoSourceStep::NeedMore
                    | CargoSourceStep::Ignored,
                )) => {}
                Poll::Ready(Ok(CargoSourceStep::Eof)) => return true,
                Poll::Pending => return false,
                Poll::Ready(Ok(
                    CargoSourceStep::Artifact(_)
                    | CargoSourceStep::BuildScript(_)
                    | CargoSourceStep::BinBuilt { .. }
                    | CargoSourceStep::BuildFinished { .. },
                )) => {
                    panic!("only one artifact was emitted")
                }
                Poll::Ready(Err(error)) => panic!("stdout drain failed: {error:?}"),
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn c49_native_child_stdout_yields_before_the_child_closes_it() {
        use CargoSourceStep as Step;
        let pipes = UnixPipePair::new().expect("native child capture pipes are created");
        let mut release_barrier = [-1; 2];
        assert_eq!(unsafe { libc::pipe(release_barrier.as_mut_ptr()) }, 0);
        let process_id = fork_fixture_producer(&pipes, release_barrier);

        unsafe { libc::close(release_barrier[0]) };
        let release_writer = ReleaseWriter(release_barrier[1]);
        let (stdout_descriptor, _stderr_descriptor) = pipes.into_parent_reads();
        let port = MacosExitPort::new().expect("shared kqueue is created");
        let stdout_token = port
            .watch_descriptor(stdout_descriptor.raw())
            .expect("stdout readiness is registered");
        let mut child_watch = port.watch(process_id).expect("child exit is registered");
        let mut reader = MacosPipeReader::new(stdout_descriptor, &port, stdout_token);
        let mut source_storage = [0_u8; 256];
        let mut frame_storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut source = CargoSource::new(&mut source_storage).expect("source storage exists");
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 255).expect("frame storage fits records");
        let mut events = unset_events();
        let mut context = Context::from_waker(Waker::noop());
        let mut ignored_records = 0;

        let artifact_before_eof = loop {
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
                Poll::Ready(Ok(Step::Ignored | Step::BinBuilt { .. } | Step::BuildFinished { .. })) => {
                    ignored_records += 1;
                }
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
        assert!(artifact_before_eof);
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
        let mut child_reaped = false;
        while !stdout_eof || !child_reaped {
            let ready = port
                .wait(&mut events)
                .expect("completion readiness arrives");
            for event in events.iter().take(ready) {
                if event.token == stdout_token {
                    reader.wake_read_ready();
                }
                if event.token == child_watch.token() {
                    child_watch.observe(event);
                    child_reaped = child_watch.reap().expect("child is reaped").is_some();
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
        std::println!(
            "{{\"native_child_artifacts_before_eof\":1,\"native_non_test_ignored\":{ignored_records},\"pending_while_child_stdout_open\":1,\"stdout_eof_after_release\":1,\"child_reaped\":1}}"
        );
    }

    const THREE_ARTIFACTS: &[u8] = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/alpha\"}\n{\"reason\":\"build-script-executed\",\"package_id\":\"path+file:///fixture#0.1.0\",\"linked_libs\":[],\"linked_paths\":[],\"cfgs\":[],\"env\":[],\"out_dir\":\"/fixture/out\"}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/beta\"}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/gamma\"}\n{\"reason\":\"build-finished\",\"success\":true}\n";

    struct WholeReader {
        bytes: Option<&'static [u8]>,
    }

    impl AsyncRead for WholeReader {
        type Error = Infallible;

        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<Result<usize, Self::Error>> {
            match self.get_mut().bytes.take() {
                Some(bytes) => {
                    buffer[..bytes.len()].copy_from_slice(bytes);
                    Poll::Ready(Ok(bytes.len()))
                }
                None => Poll::Ready(Ok(0)),
            }
        }
    }

    fn paths_via_async_read(records: &'static [u8]) -> std::vec::Vec<std::string::String> {
        let mut source_storage = [0_u8; 1024];
        let mut frame_storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut source = CargoSource::new(&mut source_storage).expect("source storage exists");
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 255).expect("record storage fits the fixture");
        let mut reader = WholeReader {
            bytes: Some(records),
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut paths = std::vec::Vec::new();
        loop {
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
                    paths.push(std::string::String::from(path));
                }
                Poll::Ready(Ok(
                    CargoSourceStep::Read { .. }
                    | CargoSourceStep::NeedMore
                    | CargoSourceStep::Ignored
                    | CargoSourceStep::BuildScript(_)
                    | CargoSourceStep::BinBuilt { .. }
                    | CargoSourceStep::BuildFinished { .. },
                )) => {}
                Poll::Ready(Ok(CargoSourceStep::Eof)) => break paths,
                other => panic!("async read path failed: {other:?}"),
            }
        }
    }

    fn feed_lent(
        chunk: &[u8],
        framer: &mut ArtifactFramer<'_>,
        scratch: &mut [u8],
        paths: &mut std::vec::Vec<std::string::String>,
    ) -> usize {
        let mut context = Context::from_waker(Waker::noop());
        let mut consumed = 0;
        while consumed < chunk.len() || framer.frame_ready().expect("framer state is readable") {
            match CargoSource::poll_lent(framer, &chunk[consumed..], scratch, 4, &mut context) {
                Poll::Ready(Ok((step, consumed_bytes))) => {
                    if let CargoSourceStep::Artifact(TestArtifact {
                        executable: path, ..
                    }) = step
                    {
                        paths.push(std::string::String::from(path));
                    }
                    consumed += consumed_bytes;
                }
                other => panic!("lent chunk failed: {other:?}"),
            }
        }
        consumed
    }

    fn drain_lent_eof(
        framer: &mut ArtifactFramer<'_>,
        scratch: &mut [u8],
        paths: &mut std::vec::Vec<std::string::String>,
    ) {
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match CargoSource::poll_lent_eof(framer, scratch, 4, &mut context) {
                Poll::Ready(Ok(CargoSourceStep::Eof)) => break,
                Poll::Ready(Ok(CargoSourceStep::Artifact(TestArtifact {
                    executable: path,
                    ..
                }))) => {
                    paths.push(std::string::String::from(path));
                }
                Poll::Ready(Ok(
                    CargoSourceStep::Read { .. }
                    | CargoSourceStep::NeedMore
                    | CargoSourceStep::Ignored,
                )) => {}
                other => panic!("lent eof failed: {other:?}"),
            }
        }
    }

    fn assert_every_split_matches(records: &'static [u8], expected_artifacts: usize) {
        let reference = paths_via_async_read(records);
        assert_eq!(reference.len(), expected_artifacts);
        let mut split_cases = 0;
        let mut artifacts_matched = 0;
        for split in 1..records.len() {
            let mut frame_storage = [0_u8; 256];
            let mut scratch = [0_u8; 64];
            let mut framer =
                ArtifactFramer::new(&mut frame_storage, 255).expect("record storage fits");
            let mut paths = std::vec::Vec::new();
            for chunk in [&records[..split], &records[split..]] {
                let consumed = feed_lent(chunk, &mut framer, &mut scratch, &mut paths);
                assert_eq!(consumed, chunk.len(), "split {split}");
            }
            drain_lent_eof(&mut framer, &mut scratch, &mut paths);
            assert_eq!(paths, reference, "split {split}");
            split_cases += 1;
            artifacts_matched += paths.len();
        }
        assert_eq!(split_cases, records.len() - 1);
        std::println!(
            "{{\"split_cases_checked\":{split_cases},\"artifacts_matched\":{artifacts_matched}}}"
        );
    }

    #[test]
    fn lent_chunk_every_split_matches_async_read_for_the_single_artifact_fixture() {
        assert_every_split_matches(CARGO_RECORDS, 1);
    }

    #[test]
    fn lent_chunk_every_split_matches_async_read_for_the_three_artifact_fixture() {
        assert_every_split_matches(THREE_ARTIFACTS, 3);
    }

    #[test]
    fn lent_eof_with_unterminated_record_returns_the_artifact_framing_error() {
        let mut frame_storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut framer = ArtifactFramer::new(&mut frame_storage, 255).expect("record storage fits");
        let mut paths = std::vec::Vec::new();
        let partial = &CARGO_RECORDS[..CARGO_RECORDS.len() / 2];
        assert_eq!(
            feed_lent(partial, &mut framer, &mut scratch, &mut paths),
            partial.len()
        );
        let mut context = Context::from_waker(Waker::noop());
        let outcome = CargoSource::poll_lent_eof(&mut framer, &mut scratch, 4, &mut context);
        std::println!("lent_eof_unterminated={outcome:?}");
        assert!(matches!(
            outcome,
            Poll::Ready(Err(CargoSourceError::Framing(_)))
        ));
    }

    #[test]
    fn lent_zero_length_chunk_is_zero_consumed_progress_and_leaves_the_framer_untouched() {
        let mut frame_storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut framer = ArtifactFramer::new(&mut frame_storage, 255).expect("record storage fits");
        let mut context = Context::from_waker(Waker::noop());
        let outcome = CargoSource::poll_lent(&mut framer, &[], &mut scratch, 4, &mut context);
        assert!(matches!(
            outcome,
            Poll::Ready(Ok((CargoSourceStep::NeedMore, 0)))
        ));
        assert!(framer.is_empty());
    }
}
