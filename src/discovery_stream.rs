use core::future::Future;
use core::marker::PhantomData;

use proxima_primitives::pipe::Pipe;

use crate::artifact_stream::{ArtifactFramer, ArtifactStreamError};
use crate::libtest_listing::{LibtestFramePipe, ListingError, ListingLine};

/// One call to the incremental libtest listing discovery pipe.
pub struct ListingDiscoveryInput<'call, 'buffer> {
    /// Caller-owned fixed-capacity newline framer.
    pub framer: &'call mut ArtifactFramer<'buffer>,
    /// Next source chunk; replay its unconsumed tail before reading more.
    pub chunk: &'call [u8],
    /// Maximum bytes in one test name line, excluding its newline.
    pub max_line_bytes: usize,
}

/// One bounded discovery step and at most one borrowed test name.
pub struct ListingDiscoveryOutput<'line> {
    /// Bytes copied from this chunk; keep and replay the rest before reading again.
    pub consumed_bytes: usize,
    /// One complete test line was consumed, including an ignored summary line.
    pub line_consumed: bool,
    /// Borrowed test name, `Ignored`, or no complete line yet.
    pub line: Option<ListingLine<'line>>,
}

/// Category of a failure while consuming one listing chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingDiscoveryErrorKind {
    /// The shared fixed-capacity delimiter adapter rejected the stream.
    Framing(ArtifactStreamError),
    /// The libtest line parser rejected one complete frame.
    Listing(ListingError),
}

/// Failures while framing or parsing one test listing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingDiscoveryError {
    /// Bytes accepted from the supplied chunk before this error.
    pub consumed_bytes: usize,
    /// Failure category from the framing or parsing stage.
    pub kind: ListingDiscoveryErrorKind,
}

/// Proxima `Pipe` that discovers no more than one test name per call.
pub struct ListingDiscoveryPipe<'call, 'buffer>(PhantomData<&'call mut ArtifactFramer<'buffer>>);

impl<'call, 'buffer> ListingDiscoveryPipe<'call, 'buffer> {
    /// Creates one incremental test listing discovery step.
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'call, 'buffer> Pipe for ListingDiscoveryPipe<'call, 'buffer> {
    type In = ListingDiscoveryInput<'call, 'buffer>;
    type Out = ListingDiscoveryOutput<'call>;
    type Err = ListingDiscoveryError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move {
            let parser = LibtestFramePipe::new(input.max_line_bytes);
            input
                .framer
                .validate_input(input.chunk)
                .map_err(|error| ListingDiscoveryError {
                    consumed_bytes: 0,
                    kind: ListingDiscoveryErrorKind::Framing(error),
                })?;
            let frame_ready =
                input
                    .framer
                    .frame_ready()
                    .map_err(|error| ListingDiscoveryError {
                        consumed_bytes: 0,
                        kind: ListingDiscoveryErrorKind::Framing(error),
                    })?;
            let consumed_bytes = if frame_ready {
                0
            } else {
                input
                    .framer
                    .push(input.chunk)
                    .map_err(|error| ListingDiscoveryError {
                        consumed_bytes: 0,
                        kind: ListingDiscoveryErrorKind::Framing(error),
                    })?
            };
            let frame = input
                .framer
                .peek_frame()
                .map_err(|error| ListingDiscoveryError {
                    consumed_bytes,
                    kind: ListingDiscoveryErrorKind::Framing(error),
                })?;
            let line =
                match frame {
                    Some(frame) => match parser.call(frame).await {
                        Ok(line) => {
                            input.framer.consume_frame().map_err(|error| {
                                ListingDiscoveryError {
                                    consumed_bytes,
                                    kind: ListingDiscoveryErrorKind::Framing(error),
                                }
                            })?;
                            Some(line)
                        }
                        Err(error) => {
                            input.framer.consume_frame().map_err(|error| {
                                ListingDiscoveryError {
                                    consumed_bytes,
                                    kind: ListingDiscoveryErrorKind::Framing(error),
                                }
                            })?;
                            input.framer.poison();
                            return Err(ListingDiscoveryError {
                                consumed_bytes,
                                kind: ListingDiscoveryErrorKind::Listing(error),
                            });
                        }
                    },
                    None => None,
                };
            Ok(ListingDiscoveryOutput {
                consumed_bytes,
                line_consumed: line.is_some(),
                line,
            })
        }
    }
}
