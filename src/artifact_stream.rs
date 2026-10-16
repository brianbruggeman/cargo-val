use std::vec;
use std::vec::Vec;

use core::cell::Cell;

use proxima_codec::{DelimiterCodec, FrameCodec, FrameError, FrameLimits};

/// Bounded framing and EOF failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStreamError {
    /// Proxima's borrowed frame decoder rejected the frame.
    Framing(FrameError),
    /// EOF arrived while a nonempty record had no newline.
    UnterminatedRecord { length: usize },
    /// EOF arrived before a complete buffered record was drained.
    UndrainedFrame { length: usize },
    /// New input was supplied after the stream accepted EOF.
    InputAfterEof,
    /// The stream was already terminated by a prior parse error.
    Poisoned,
}

impl From<FrameError> for ArtifactStreamError {
    fn from(error: FrameError) -> Self {
        Self::Framing(error)
    }
}

/// One framing transition: bytes consumed from the chunk and at most one parsed record.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub enum RecordStep<Parsed> {
    /// No complete record yet; chunk bytes were staged or the stream is idle.
    NeedMore {
        /// Bytes accepted from the chunk; replay the tail before reading more.
        consumed_bytes: usize,
    },
    /// One complete record was framed and handed to the caller's parser.
    Parsed {
        /// The parser's result, which may borrow the chunk or the framer's storage.
        parsed: Parsed,
        /// Bytes accepted from the chunk; replay the tail before reading more.
        consumed_bytes: usize,
    },
}

/// Why [`ArtifactFramer::next_record`] stopped, with the chunk bytes already accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordFault<ParseError> {
    /// Framing, EOF, or poison rejected the step; the framer holds no new record.
    Framing {
        /// Bytes accepted from the chunk before the rejection.
        consumed_bytes: usize,
        /// The framing failure.
        error: ArtifactStreamError,
    },
    /// The caller's parser rejected a consumed record; the framer is now poisoned.
    Parse {
        /// Bytes accepted from the chunk, including the rejected record.
        consumed_bytes: usize,
        /// The parser's failure.
        error: ParseError,
    },
}

/// Fixed caller-owned storage for incrementally framing Cargo JSON lines with
/// Proxima's borrowed [`DelimiterCodec`](proxima_codec::DelimiterCodec).
pub struct ArtifactFramer<'buffer> {
    buffer: &'buffer mut [u8],
    grown: Vec<u8>,
    ceiling: usize,
    buffered: usize,
    consumed: Cell<usize>,
    max_record_bytes: usize,
    codec: DelimiterCodec,
    source_state: Cell<SourceState>,
    scan_state: Cell<ScanState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceState {
    Open,
    Eof,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Scanning { scanned: usize },
    FrameReady { frame_length: usize },
}

impl<'buffer> ArtifactFramer<'buffer> {
    /// Creates a framer whose buffer can hold the configured record and newline.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactStreamError::Framing`] when the record limit overflows or the buffer cannot hold a record and its newline.
    pub fn new(
        buffer: &'buffer mut [u8],
        max_record_bytes: usize,
    ) -> Result<Self, ArtifactStreamError> {
        let required = max_record_bytes
            .checked_add(1)
            .ok_or(ArtifactStreamError::Framing(FrameError::FrameTooLarge {
                len: usize::MAX,
            }))?;
        if buffer.len() < required {
            return Err(ArtifactStreamError::Framing(FrameError::FrameTooLarge {
                len: buffer.len(),
            }));
        }
        Ok(Self {
            buffer,
            grown: Vec::new(),
            ceiling: max_record_bytes,
            buffered: 0,
            consumed: Cell::new(0),
            max_record_bytes,
            codec: DelimiterCodec::new(b"\n", FrameLimits::new(max_record_bytes, false)),
            source_state: Cell::new(SourceState::Open),
            scan_state: Cell::new(ScanState::Scanning { scanned: 0 }),
        })
    }

    /// Lets a record that fills the caller's storage grow into a larger heap buffer, up to `ceiling` bytes; the bytes already framed are copied across.
    /// The steady state never allocates: the heap buffer exists only after a record did not fit.
    #[must_use]
    pub fn growing_to(mut self, ceiling: usize) -> Self {
        self.ceiling = ceiling.max(self.max_record_bytes);
        self
    }

    fn storage(&self) -> &[u8] {
        if self.grown.is_empty() { &*self.buffer } else { &self.grown }
    }

    fn storage_mut(&mut self) -> &mut [u8] {
        if self.grown.is_empty() { &mut *self.buffer } else { &mut self.grown }
    }

    // doubles the storage when a partial record fills it, so the record can finish; the codec limit follows the new size
    fn grow(&mut self) {
        let doubled = (self.storage().len() * 2).min(self.ceiling + 1);
        let mut larger = vec![0; doubled];
        larger[..self.buffered].copy_from_slice(&self.storage()[..self.buffered]);
        self.grown = larger;
        self.max_record_bytes = doubled - 1;
        self.codec = DelimiterCodec::new(b"\n", FrameLimits::new(self.max_record_bytes, false));
    }

    /// Copies as much of a transport chunk as fits and returns bytes consumed.
    /// A short count leaves the tail with the caller until a frame is drained.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactStreamError::InputAfterEof`] for data after EOF, or [`ArtifactStreamError::Poisoned`] after a rejected record.
    pub fn push(&mut self, chunk: &[u8]) -> Result<usize, ArtifactStreamError> {
        self.validate_input(chunk)?;
        if self.source_state.get() == SourceState::Eof {
            return Ok(0);
        }
        self.compact();
        if self.buffered == self.storage().len() && !chunk.is_empty() && self.max_record_bytes < self.ceiling {
            self.grow();
        }
        let available = self.storage().len() - self.buffered;
        let copied = chunk.len().min(available);
        let start = self.buffered;
        self.storage_mut()[start..start + copied].copy_from_slice(&chunk[..copied]);
        self.buffered += copied;
        Ok(copied)
    }

    /// Checks whether a source chunk is valid for the current stream state.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactStreamError::InputAfterEof`] for data after EOF, or [`ArtifactStreamError::Poisoned`] after a rejected record.
    pub fn validate_input(&self, chunk: &[u8]) -> Result<(), ArtifactStreamError> {
        match self.source_state.get() {
            SourceState::Open => Ok(()),
            SourceState::Eof if chunk.is_empty() => Ok(()),
            SourceState::Eof => Err(ArtifactStreamError::InputAfterEof),
            SourceState::Failed => Err(ArtifactStreamError::Poisoned),
        }
    }

    /// Makes the stream terminal after a downstream parser rejects a consumed frame.
    pub fn poison(&self) {
        self.source_state.set(SourceState::Failed);
    }

    /// Checks whether a complete frame is buffered without consuming it.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactStreamError`] when the stream is poisoned, ended mid-record, or a record exceeds the configured limit.
    pub fn frame_ready(&self) -> Result<bool, ArtifactStreamError> {
        self.has_complete_frame()
    }

    /// Whether no partial or complete frame remains in caller-owned storage.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffered == self.consumed.get()
    }

    /// Advances past the current complete frame after its borrowed result is handled.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactStreamError`] when the stream is poisoned, ended mid-record, or a record exceeds the configured limit.
    pub fn consume_frame(&self) -> Result<(), ArtifactStreamError> {
        let Some((frame_start, frame_end)) = self.next_frame_bounds()? else {
            return Ok(());
        };
        let pending = &self.storage()[self.consumed.get()..self.buffered];
        let (_, consumed) = self.codec.parse_frame(pending)?;
        let next_consumed = self.consumed.get() + consumed;
        if frame_start + consumed != frame_end + 1 {
            return Err(ArtifactStreamError::Framing(FrameError::Incomplete));
        }
        self.consumed.set(next_consumed);
        self.scan_state.set(ScanState::Scanning {
            scanned: next_consumed,
        });
        Ok(())
    }

    /// Frames at most one record from `chunk` and hands it to `parse`.
    ///
    /// A record that arrives whole in `chunk` is lent to `parse` without copying;
    /// a fragmented record is staged in the framer's storage and lent from there.
    /// `parse` runs here so a rejected record poisons the framer in the same step.
    ///
    /// # Errors
    ///
    /// Returns [`RecordFault::Framing`] when the stream is poisoned, past EOF, or a
    /// record exceeds the limit, and [`RecordFault::Parse`] when `parse` fails.
    pub fn next_record<'call, Parsed, ParseError>(
        &'call mut self,
        chunk: &'call [u8],
        parse: impl FnOnce(&'call [u8]) -> Result<Parsed, ParseError>,
    ) -> Result<RecordStep<Parsed>, RecordFault<ParseError>> {
        self.next_record_unless(chunk, parse, |_| false)
    }

    /// [`Self::next_record`] that leaves a record unconsumed when `hold` says so.
    ///
    /// A held record reports [`RecordStep::NeedMore`]: a record whole in `chunk` consumes zero bytes
    /// so the caller replays it, and a staged record stays framed so [`Self::frame_ready`] stays true.
    /// Either way the next call parses the same record again, so `hold` sees it once per attempt.
    ///
    /// # Errors
    ///
    /// The same faults as [`Self::next_record`].
    pub fn next_record_unless<'call, Parsed, ParseError>(
        &'call mut self,
        chunk: &'call [u8],
        parse: impl FnOnce(&'call [u8]) -> Result<Parsed, ParseError>,
        hold: impl FnOnce(&Parsed) -> bool,
    ) -> Result<RecordStep<Parsed>, RecordFault<ParseError>> {
        self.validate_input(chunk)
            .map_err(|error| Self::framing_fault(0, error))?;
        let whole_record_possible = self.source_state.get() == SourceState::Open
            && self.buffered == self.consumed.get()
            && !self
                .has_complete_frame()
                .map_err(|error| Self::framing_fault(0, error))?;
        if whole_record_possible {
            let oversize = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .filter(|newline_offset| *newline_offset > self.max_record_bytes);
            if let Some(newline_offset) = oversize
                && newline_offset > self.ceiling
            {
                return Err(Self::framing_fault(
                    0,
                    ArtifactStreamError::Framing(FrameError::FrameTooLarge {
                        len: newline_offset,
                    }),
                ));
            }
            // a record that passes the current storage but not the ceiling is staged below, where the storage grows
            match oversize.map_or_else(|| self.codec.parse_frame(chunk), |_| Err(FrameError::Incomplete)) {
                Ok((record, consumed_bytes)) => {
                    return self.parse_record(record, (consumed_bytes, 0), parse, hold);
                }
                Err(FrameError::Incomplete) => {}
                Err(error) => {
                    return Err(Self::framing_fault(0, ArtifactStreamError::Framing(error)));
                }
            }
        }
        let staged = self
            .has_complete_frame()
            .map_err(|error| Self::framing_fault(0, error))?
            || self.source_state.get() == SourceState::Eof;
        let consumed_bytes = if staged {
            0
        } else {
            self.push(chunk)
                .map_err(|error| Self::framing_fault(0, error))?
        };
        let framer: &'call Self = self;
        let Some((frame_start, frame_end)) = framer
            .next_frame_bounds()
            .map_err(|error| Self::framing_fault(consumed_bytes, error))?
        else {
            return Ok(RecordStep::NeedMore { consumed_bytes });
        };
        let step = framer.parse_record(
            &framer.storage()[frame_start..frame_end],
            (consumed_bytes, consumed_bytes),
            parse,
            hold,
        )?;
        if matches!(step, RecordStep::Parsed { .. }) {
            framer
                .consume_frame()
                .map_err(|error| Self::framing_fault(consumed_bytes, error))?;
        }
        Ok(step)
    }

    /// Largest record, excluding its newline, this framer accepts.
    #[must_use]
    pub const fn max_record_bytes(&self) -> usize {
        self.max_record_bytes
    }

    const fn framing_fault<ParseError>(
        consumed_bytes: usize,
        error: ArtifactStreamError,
    ) -> RecordFault<ParseError> {
        RecordFault::Framing {
            consumed_bytes,
            error,
        }
    }

    fn parse_record<'record, Parsed, ParseError>(
        &self,
        record: &'record [u8],
        (consumed_bytes, held_bytes): (usize, usize),
        parse: impl FnOnce(&'record [u8]) -> Result<Parsed, ParseError>,
        hold: impl FnOnce(&Parsed) -> bool,
    ) -> Result<RecordStep<Parsed>, RecordFault<ParseError>> {
        match parse(record) {
            Ok(parsed) if hold(&parsed) => Ok(RecordStep::NeedMore {
                consumed_bytes: held_bytes,
            }),
            Ok(parsed) => Ok(RecordStep::Parsed {
                parsed,
                consumed_bytes,
            }),
            Err(error) => {
                self.poison();
                Err(RecordFault::Parse {
                    consumed_bytes,
                    error,
                })
            }
        }
    }

    fn next_frame_bounds(&self) -> Result<Option<(usize, usize)>, ArtifactStreamError> {
        if self.source_state.get() == SourceState::Failed {
            return Err(ArtifactStreamError::Poisoned);
        }
        if self.source_state.get() == SourceState::Eof && self.consumed.get() == self.buffered {
            return Ok(None);
        }
        if !self.has_complete_frame()? {
            return Ok(None);
        }
        let ScanState::FrameReady { frame_length } = self.scan_state.get() else {
            return Err(ArtifactStreamError::Framing(FrameError::Incomplete));
        };
        let pending = &self.storage()[self.consumed.get()..self.buffered];
        let (_, consumed) = self.codec.parse_frame(pending)?;
        let frame_start = self.consumed.get();
        if consumed != frame_length + 1 {
            return Err(ArtifactStreamError::Framing(FrameError::Incomplete));
        }
        Ok(Some((frame_start, frame_start + frame_length)))
    }

    fn has_complete_frame(&self) -> Result<bool, ArtifactStreamError> {
        if matches!(self.scan_state.get(), ScanState::FrameReady { .. }) {
            return Ok(true);
        }
        let ScanState::Scanning { scanned } = self.scan_state.get() else {
            return Ok(true);
        };
        let consumed = self.consumed.get();
        let scan_start = scanned.max(consumed);
        let newline_offset = self.storage()[scan_start..self.buffered]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|relative_offset| scan_start + relative_offset);
        if let Some(newline_offset) = newline_offset {
            let frame_length = newline_offset - consumed;
            if frame_length > self.max_record_bytes {
                Err(ArtifactStreamError::Framing(FrameError::FrameTooLarge {
                    len: frame_length,
                }))
            } else {
                self.scan_state.set(ScanState::FrameReady { frame_length });
                Ok(true)
            }
        } else {
            let pending_length = self.buffered - consumed;
            if pending_length > self.ceiling {
                return Err(ArtifactStreamError::Framing(FrameError::FrameTooLarge {
                    len: pending_length,
                }));
            }
            self.scan_state.set(ScanState::Scanning {
                scanned: self.buffered,
            });
            Ok(false)
        }
    }

    /// Marks the byte source complete and rejects leftover partial or undrained records.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactStreamError::Poisoned`] after a rejected record, or an undrained or unterminated record error when bytes remain buffered.
    pub fn finish(&mut self) -> Result<(), ArtifactStreamError> {
        if self.source_state.get() == SourceState::Failed {
            return Err(ArtifactStreamError::Poisoned);
        }
        self.source_state.set(SourceState::Eof);
        let pending_length = self.buffered - self.consumed.get();
        if pending_length > 0 {
            if self.has_complete_frame()? {
                return Err(ArtifactStreamError::UndrainedFrame {
                    length: pending_length,
                });
            }
            return Err(ArtifactStreamError::UnterminatedRecord {
                length: pending_length,
            });
        }
        Ok(())
    }

    /// Forgets every buffered byte and state so the same storage frames the next stream.
    pub fn reset(&mut self) {
        self.buffered = 0;
        self.consumed.set(0);
        self.source_state.set(SourceState::Open);
        self.scan_state.set(ScanState::Scanning { scanned: 0 });
    }

    fn compact(&mut self) {
        let consumed = self.consumed.get();
        if consumed == 0 {
            return;
        }
        let remaining = self.buffered - consumed;
        let buffered = self.buffered;
        self.storage_mut().copy_within(consumed..buffered, 0);
        let adjusted_scan_state = match self.scan_state.get() {
            ScanState::Scanning { scanned } => ScanState::Scanning {
                scanned: scanned.saturating_sub(consumed),
            },
            ScanState::FrameReady { frame_length } => ScanState::FrameReady { frame_length },
        };
        self.buffered = remaining;
        self.consumed.set(0);
        self.scan_state.set(adjusted_scan_state);
    }
}

#[cfg(test)]
mod tests {
    use proxima_codec::FrameError;

    use super::{ArtifactFramer, ArtifactStreamError, RecordFault, RecordStep};
    use crate::cargo_message::CargoMessage;
    use crate::cargo_source::parse_selection;

    fn fault_error<Parsed: core::fmt::Debug, ParseError>(
        outcome: Result<RecordStep<Parsed>, RecordFault<ParseError>>,
    ) -> ArtifactStreamError {
        match outcome {
            Err(RecordFault::Framing { error, .. }) => error,
            Err(RecordFault::Parse { .. }) => panic!("framing was expected to fail first"),
            Ok(step) => panic!("record was accepted: {step:?}"),
        }
    }

    #[proxima::test]
    async fn c49z_borrows_complete_record_and_stages_fragmented_record() {
        let complete_record = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/first\"}\n";
        let (observed, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut storage = [0_u8; 256];
            let mut scratch = [0_u8; 64];
            let mut framer = ArtifactFramer::new(&mut storage, 255)
                .expect("fixed artifact storage fits the record limit");
            let step = framer
                .next_record(complete_record, |record| {
                    parse_selection(record, &mut scratch, 255, 4)
                })
                .expect("complete record parses");
            let RecordStep::Parsed {
                parsed,
                consumed_bytes,
            } = step
            else {
                panic!("complete record produced no step");
            };
            let Some(CargoMessage::TestArtifact(artifact)) = parsed else {
                panic!("complete Cargo test artifact is selected");
            };
            let path = artifact.executable;
            let expected_path = b"/fixture/first";
            let path_offset = complete_record
                .windows(expected_path.len())
                .position(|window| window == expected_path)
                .expect("fixture path occurs in the record");
            let pointer_identity = path.as_ptr() == complete_record[path_offset..].as_ptr();
            assert_eq!(path, "/fixture/first");
            assert_eq!(consumed_bytes, complete_record.len());
            (pointer_identity, path.as_ptr())
        });
        let (pointer_identity, original_path_pointer) = observed;

        assert!(pointer_identity);
        assert_eq!(allocations, 0);

        let copied_path_storage = *b"/fixture/first";
        let copied_path = core::str::from_utf8(&copied_path_storage)
            .expect("copied fixture path remains valid UTF-8");
        assert_ne!(copied_path.as_ptr(), original_path_pointer);
        std::println!(
            "{{\"complete_line_pointer_identity\":1,\"complete_line_payload_copies\":0,\"copy_comparator_control_rejected\":1,\"allocations\":{allocations}}}"
        );

        let first_fragment = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/second\"";
        let last_fragment = b"}\n";
        let mut storage = [0_u8; 256];
        let mut scratch = [0_u8; 64];
        let mut framer =
            ArtifactFramer::new(&mut storage, 255).expect("fixed fragment storage is large");
        let first = framer
            .next_record(first_fragment, |record| {
                parse_selection(record, &mut scratch, 255, 4)
            })
            .expect("partial record stages");
        assert_eq!(
            first,
            RecordStep::NeedMore {
                consumed_bytes: first_fragment.len()
            }
        );

        let second = framer
            .next_record(last_fragment, |record| {
                parse_selection(record, &mut scratch, 255, 4)
            })
            .expect("fragmented record parses");
        let RecordStep::Parsed { parsed, .. } = second else {
            panic!("fragmented record produced no step");
        };
        let Some(CargoMessage::TestArtifact(artifact)) = parsed else {
            panic!("fragmented Cargo artifact is selected");
        };
        let staged_path = artifact.executable;
        assert_eq!(staged_path, "/fixture/second");
        assert_ne!(staged_path.as_ptr(), last_fragment.as_ptr());
        std::println!("{{\"fragmented_record_staged\":1,\"fragmented_path_borrows_framer\":1}}");
    }

    #[proxima::test]
    async fn c49z_oversized_complete_record_matches_buffered_framing_error() {
        let oversized_record = b"123456789\n";
        let expected = ArtifactStreamError::Framing(FrameError::FrameTooLarge { len: 9 });
        let mut direct_storage = [0_u8; 16];
        let mut direct_scratch = [0_u8; 16];
        let mut direct_framer =
            ArtifactFramer::new(&mut direct_storage, 8).expect("storage fits test record");
        let direct_error = fault_error(direct_framer.next_record(oversized_record, |record| {
            parse_selection(record, &mut direct_scratch, 8, 4)
        }));
        assert_eq!(direct_error, expected);
        let direct_retry = fault_error(direct_framer.next_record(oversized_record, |record| {
            parse_selection(record, &mut direct_scratch, 8, 4)
        }));
        assert_eq!(direct_retry, direct_error);

        let mut buffered_storage = [0_u8; 16];
        let mut buffered_scratch = [0_u8; 16];
        let mut buffered_framer =
            ArtifactFramer::new(&mut buffered_storage, 8).expect("storage fits test record");
        let staged = buffered_framer
            .next_record(b"12345678", |record| {
                parse_selection(record, &mut buffered_scratch, 8, 4)
            })
            .expect("eight bytes stage without a newline");
        assert_eq!(staged, RecordStep::NeedMore { consumed_bytes: 8 });
        let buffered_error = fault_error(buffered_framer.next_record(b"9\n", |record| {
            parse_selection(record, &mut buffered_scratch, 8, 4)
        }));
        assert_eq!(direct_error, buffered_error);
        let buffered_retry = fault_error(buffered_framer.next_record(b"", |record| {
            parse_selection(record, &mut buffered_scratch, 8, 4)
        }));
        assert_eq!(buffered_retry, buffered_error);
        std::println!(
            "{{\"direct_buffered_oversize_error_equal\":1,\"direct_buffered_streams_not_poisoned\":1,\"error\":\"frame_too_large:9\"}}"
        );
    }

    #[proxima::test]
    async fn a_held_whole_record_consumes_nothing_and_is_taken_on_the_retry() {
        let chunk = b"declared::loads_whole_checkpoint: test\ndeclared::next: test\n";
        let mut storage = [0_u8; 256];
        let mut framer = ArtifactFramer::new(&mut storage, 255).expect("storage fits the record limit");

        let postponed = framer
            .next_record_unless(chunk, Ok::<_, core::convert::Infallible>, |_| true)
            .expect("a whole record parses before it is held");
        assert_eq!(postponed, RecordStep::NeedMore { consumed_bytes: 0 });
        let taken = framer
            .next_record_unless(chunk, Ok::<_, core::convert::Infallible>, |_| false)
            .expect("the same record parses again once released");

        assert_eq!(
            taken,
            RecordStep::Parsed {
                parsed: b"declared::loads_whole_checkpoint: test".as_slice(),
                consumed_bytes: 39
            }
        );
    }

    #[proxima::test]
    async fn a_held_staged_record_stays_framed_for_the_retry() {
        let mut storage = [0_u8; 256];
        let mut framer = ArtifactFramer::new(&mut storage, 255).expect("storage fits the record limit");
        let head = framer.next_record(b"declared::loads_", Ok::<_, core::convert::Infallible>).expect("a fragment stages");
        assert_eq!(head, RecordStep::NeedMore { consumed_bytes: 16 });

        let postponed = framer
            .next_record_unless(b"whole: test\n", Ok::<_, core::convert::Infallible>, |_| true)
            .expect("the completed record parses before it is held");
        assert_eq!(postponed, RecordStep::NeedMore { consumed_bytes: 12 });
        let ready = framer.frame_ready().expect("framer state is readable");
        assert!(ready);
        let taken = framer
            .next_record_unless(b"", Ok::<_, core::convert::Infallible>, |_| false)
            .expect("the framed record is taken on the retry");

        assert_eq!(
            taken,
            RecordStep::Parsed {
                parsed: b"declared::loads_whole: test".as_slice(),
                consumed_bytes: 0
            }
        );
    }
}
