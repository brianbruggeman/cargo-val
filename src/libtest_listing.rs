use core::future::Future;
use core::marker::PhantomData;
use core::str;

use proxima_codec::{DelimiterCodec, FrameCodec, FrameError, FrameLimits};
use proxima_primitives::pipe::Pipe;

/// One borrowed libtest listing result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingLine<'line> {
    /// A test name emitted by `--list`.
    Test(&'line str),
    /// A summary or non-test listing line.
    Ignored,
}

/// Failures while decoding one newline-terminated libtest line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingError {
    /// Proxima's borrowed delimiter codec rejected the line boundary.
    Framing(FrameError),
    /// The line contains bytes that are not valid UTF-8.
    InvalidUtf8,
    /// The input contains bytes after the first complete line.
    TrailingBytes,
    /// The complete input line exceeds the caller-provided byte limit.
    LineTooLarge { limit: usize, actual: usize },
}

impl From<FrameError> for ListingError {
    fn from(error: FrameError) -> Self {
        Self::Framing(error)
    }
}

/// Proxima [`Pipe`] that parses one borrowed libtest line within a caller cap.
///
/// This composes Proxima's [`DelimiterCodec`] and [`Pipe`] so callers can
/// apply the same bounded framing contract without owning or copying names.
pub struct LibtestListingPipe<'line> {
    max_line_bytes: usize,
    line: PhantomData<&'line [u8]>,
}

/// Proxima [`Pipe`] that parses one already framed libtest line payload.
pub struct LibtestFramePipe<'line> {
    max_line_bytes: usize,
    line: PhantomData<&'line [u8]>,
}

impl<'line> LibtestFramePipe<'line> {
    /// Creates the parser with a maximum framed payload size in bytes.
    #[must_use]
    pub const fn new(max_line_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            line: PhantomData,
        }
    }
}

impl<'line> Pipe for LibtestFramePipe<'line> {
    type In = &'line [u8];
    type Out = ListingLine<'line>;
    type Err = ListingError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move { parse_listing_content(input, self.max_line_bytes) }
    }
}

impl<'line> LibtestListingPipe<'line> {
    /// Creates the parser with a maximum complete line size in bytes.
    #[must_use]
    pub const fn new(max_line_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            line: PhantomData,
        }
    }
}

impl<'line> Pipe for LibtestListingPipe<'line> {
    type In = &'line [u8];
    type Out = ListingLine<'line>;
    type Err = ListingError;

    fn call(&self, input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
        async move { parse_listing_line(input, self.max_line_bytes) }
    }
}

fn parse_listing_line(line: &[u8], max_line_bytes: usize) -> Result<ListingLine<'_>, ListingError> {
    if line.len() > max_line_bytes {
        return Err(ListingError::LineTooLarge {
            limit: max_line_bytes,
            actual: line.len(),
        });
    }
    let codec = DelimiterCodec::new(b"\n", FrameLimits::new(max_line_bytes, false));
    let (frame, consumed) = codec.parse_frame(line)?;
    if consumed != line.len() {
        return Err(ListingError::TrailingBytes);
    }
    parse_listing_content(frame, max_line_bytes)
}

pub(crate) fn parse_listing_content(
    frame: &[u8],
    max_line_bytes: usize,
) -> Result<ListingLine<'_>, ListingError> {
    if frame.len() > max_line_bytes {
        return Err(ListingError::LineTooLarge {
            limit: max_line_bytes,
            actual: frame.len(),
        });
    }
    let line = match frame.strip_suffix(b"\r") {
        Some(line) => line,
        None => frame,
    };
    let line = str::from_utf8(line).map_err(|_| ListingError::InvalidUtf8)?;
    let Some(name) = line.strip_suffix(": test") else {
        return Ok(ListingLine::Ignored);
    };
    if name.is_empty() {
        return Ok(ListingLine::Ignored);
    }
    Ok(ListingLine::Test(name))
}
