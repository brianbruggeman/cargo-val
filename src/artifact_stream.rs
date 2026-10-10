use core::cell::Cell;
use core::future::Future;
use core::marker::PhantomData;

use proxima_codec::{DelimiterCodec, FrameCodec, FrameError, FrameLimits};
use proxima_primitives::pipe::Pipe;

use crate::{ArtifactRecord, ExecutablePath, ParseError, parse_artifact};

/// The storage that owns an executable path returned by [`ArtifactPipe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStorage {
    /// The selected path bytes remain in the Cargo record buffer.
    Record,
    /// The selected path bytes were decoded into the caller's scratch buffer.
    Scratch,
}

/// A selected path borrowing its caller-owned record or scratch buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactSelection<'buffer> {
    /// Buffer containing the path bytes.
    pub storage: PathStorage,
    /// Selected executable path, valid while its source buffer is borrowed.
    pub path: &'buffer str,
}

/// Bounded framing, EOF, and record parsing failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStreamError {
    /// Proxima's borrowed frame decoder rejected the frame.
    Framing(FrameError),
    /// The C2 parser rejected a consumed record; callers should stop the stream.
    Parsing(ParseError),
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

impl From<ParseError> for ArtifactStreamError {
    fn from(error: ParseError) -> Self {
        Self::Parsing(error)
    }
}

/// One source chunk, the framer that owns its accumulated bytes, and path scratch.
pub struct ArtifactInput<'call, 'buffer> {
    /// Caller-owned framer that stores partial Cargo records.
    pub framer: &'call mut ArtifactFramer<'buffer>,
    /// Next byte chunk from the Cargo stdout source.
    pub chunk: &'call [u8],
    /// Caller-owned storage for a JSON escaped executable path.
    pub scratch: &'call mut [u8],
    /// Largest accepted JSON container depth.
    pub max_json_depth: usize,
}

/// The consumed source-byte count and at most one discovered executable.
#[derive(Debug)]
pub struct ArtifactOutput<'path> {
    /// Number of bytes accepted from the chunk; replay its remaining tail before reading more.
    pub consumed_bytes: usize,
    /// Whether one complete framed record was consumed, even if it was ignored.
    pub record_consumed: bool,
    /// One path discovered from a complete Cargo record, if present.
    pub selection: Option<ArtifactSelection<'path>>,
}

/// Proxima [`Pipe`] that frames one byte chunk and parses at most one record.
///
/// The result borrows the executable path from caller-owned input or scratch
/// storage, so the caller cannot compact either buffer while using the path.
pub struct ArtifactPipe<'call, 'buffer>(PhantomData<&'call mut ArtifactFramer<'buffer>>);

impl<'call, 'buffer> ArtifactPipe<'call, 'buffer> {
    /// Creates a chunk-to-record pipe; its framer retains partial input state.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'call, 'buffer> Pipe for ArtifactPipe<'call, 'buffer> {
    type In = ArtifactInput<'call, 'buffer>;
    type Out = ArtifactOutput<'call>;
    type Err = ArtifactStreamError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            input
                .framer
                .next_artifact(input.chunk, input.scratch, input.max_json_depth)
        }
    }
}

/// Fixed caller-owned storage for incrementally framing Cargo JSON lines with
/// Proxima's borrowed [`DelimiterCodec`](proxima_codec::DelimiterCodec).
pub struct ArtifactFramer<'buffer> {
    buffer: &'buffer mut [u8],
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
            buffered: 0,
            consumed: Cell::new(0),
            max_record_bytes,
            codec: DelimiterCodec::new(b"\n", FrameLimits::new(max_record_bytes, false)),
            source_state: Cell::new(SourceState::Open),
            scan_state: Cell::new(ScanState::Scanning { scanned: 0 }),
        })
    }

    /// Copies as much of a transport chunk as fits and returns bytes consumed.
    /// A short count leaves the tail with the caller until a frame is drained.
    pub fn push(&mut self, chunk: &[u8]) -> Result<usize, ArtifactStreamError> {
        self.validate_input(chunk)?;
        if self.source_state.get() == SourceState::Eof {
            return Ok(0);
        }
        self.compact();
        let available = self.buffer.len() - self.buffered;
        let copied = chunk.len().min(available);
        self.buffer[self.buffered..self.buffered + copied].copy_from_slice(&chunk[..copied]);
        self.buffered += copied;
        Ok(copied)
    }

    /// Checks whether a source chunk is valid for the current stream state.
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

    /// Returns the next complete record, or `None` while its newline is absent.
    pub fn next_frame(&self) -> Result<Option<&[u8]>, ArtifactStreamError> {
        let Some((frame_start, frame_end)) = self.next_frame_bounds()? else {
            return Ok(None);
        };
        self.consume_frame()?;
        Ok(Some(&self.buffer[frame_start..frame_end]))
    }

    /// Checks whether a complete frame is buffered without consuming it.
    pub fn frame_ready(&self) -> Result<bool, ArtifactStreamError> {
        self.has_complete_frame()
    }

    /// Returns the next complete frame without advancing the stream cursor.
    pub fn peek_frame(&self) -> Result<Option<&[u8]>, ArtifactStreamError> {
        let Some((frame_start, frame_end)) = self.next_frame_bounds()? else {
            return Ok(None);
        };
        self.scan_state.set(ScanState::FrameReady {
            frame_length: frame_end - frame_start,
        });
        Ok(Some(&self.buffer[frame_start..frame_end]))
    }

    /// Advances past the current complete frame after its borrowed result is handled.
    pub fn consume_frame(&self) -> Result<(), ArtifactStreamError> {
        let Some((frame_start, frame_end)) = self.next_frame_bounds()? else {
            return Ok(());
        };
        let pending = &self.buffer[self.consumed.get()..self.buffered];
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

    fn next_artifact<'path>(
        &'path mut self,
        chunk: &[u8],
        scratch: &'path mut [u8],
        max_json_depth: usize,
    ) -> Result<ArtifactOutput<'path>, ArtifactStreamError> {
        if self.source_state.get() == SourceState::Failed {
            return Err(ArtifactStreamError::Poisoned);
        }
        if self.source_state.get() == SourceState::Eof && !chunk.is_empty() {
            return Err(ArtifactStreamError::InputAfterEof);
        }
        let consumed_bytes =
            if self.has_complete_frame()? || self.source_state.get() == SourceState::Eof {
                0
            } else {
                self.push(chunk)?
            };
        let frame_bounds = self.next_frame_bounds()?;
        let record_consumed = frame_bounds.is_some();
        if frame_bounds.is_some() {
            self.consume_frame()?;
        }
        let selection = match frame_bounds {
            Some((frame_start, frame_end)) => {
                let parse_result = parse_artifact(
                    &self.buffer[frame_start..frame_end],
                    scratch,
                    self.max_record_bytes,
                    max_json_depth,
                );
                let parsed = match parse_result {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        self.source_state.set(SourceState::Failed);
                        return Err(ArtifactStreamError::Parsing(error));
                    }
                };
                match parsed {
                    ArtifactRecord::Ignored => None,
                    ArtifactRecord::TestExecutable(ExecutablePath::Borrowed(path)) => {
                        Some(ArtifactSelection {
                            storage: PathStorage::Record,
                            path,
                        })
                    }
                    ArtifactRecord::TestExecutable(ExecutablePath::Scratch(path)) => {
                        Some(ArtifactSelection {
                            storage: PathStorage::Scratch,
                            path,
                        })
                    }
                }
            }
            None => None,
        };
        Ok(ArtifactOutput {
            consumed_bytes,
            record_consumed,
            selection,
        })
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
        let pending = &self.buffer[self.consumed.get()..self.buffered];
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
        let newline_offset = self.buffer[scan_start..self.buffered]
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
            if pending_length > self.max_record_bytes {
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

    fn compact(&mut self) {
        let consumed = self.consumed.get();
        if consumed == 0 {
            return;
        }
        let remaining = self.buffered - consumed;
        self.buffer.copy_within(consumed..self.buffered, 0);
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
