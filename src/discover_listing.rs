//! Reads of libtest output: the doctest report, and the `--list` names behind capacity-gated I/O.

use core::pin::Pin;
use core::str;
use core::task::{Context, Poll};

use proxima_core::io::AsyncRead;

use crate::ExecutablePath;
use crate::artifact_stream::{ArtifactFramer, ArtifactStreamError, RecordFault, RecordStep};
use crate::schedule::SessionSlots;
use crate::selection::{Disposition, NameFilter};
use crate::test_intent::ExactTestIntent;

const LINE_CAPACITY: usize = 1100;
const RESULT_MARKER: &[u8] = b" ... ";

/// Outcome of one doctest line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocOutcome {
    Passed,
    Failed,
    Ignored,
}

/// One meaningful line of the doctest report, borrowed from the parser's line buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocEvent<'line> {
    /// A `running N tests` header; `count` is 0 for a crate with no doctests.
    Running { count: usize },
    /// A `test <name> ... ok|FAILED|ignored` line.
    Result {
        name: &'line [u8],
        outcome: DocOutcome,
    },
    /// One line of a failing doctest's captured output, without its newline.
    FailureLine(&'line [u8]),
    /// A `test result:` summary closing one `running` block.
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Results,
    BeforeSections,
    InSection,
    Summary,
}

/// Byte-at-a-time line reader and state machine; lines past the buffer are truncated, not dropped.
#[derive(Debug)]
pub struct DocParser {
    line: [u8; LINE_CAPACITY],
    length: usize,
    phase: Phase,
}

impl DocParser {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            line: [0; LINE_CAPACITY],
            length: 0,
            phase: Phase::Results,
        }
    }

    /// Feeds stdout bytes and calls `emit` once per recognized line as soon as its newline arrives.
    ///
    /// # Errors
    ///
    /// Returns the first error `emit` returns; bytes after that line are not consumed.
    pub fn feed<Error>(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(DocEvent<'_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for byte in bytes {
            if *byte == b'\n' {
                let length = strip_carriage_return(&self.line[..self.length]).len();
                self.length = 0;
                self.phase = Self::step(self.phase, &self.line[..length], &mut emit)?;
            } else if self.length < LINE_CAPACITY {
                self.line[self.length] = *byte;
                self.length += 1;
            }
        }
        Ok(())
    }

    fn step<Error>(
        phase: Phase,
        line: &[u8],
        emit: &mut impl FnMut(DocEvent<'_>) -> Result<(), Error>,
    ) -> Result<Phase, Error> {
        if line.starts_with(b"test result:") {
            emit(DocEvent::Finished)?;
            return Ok(Phase::Results);
        }
        match phase {
            Phase::Results => Self::results_line(line, emit),
            Phase::BeforeSections if line.starts_with(b"---- ") => {
                emit(DocEvent::FailureLine(line))?;
                Ok(Phase::InSection)
            }
            Phase::InSection if line == b"failures:" => Ok(Phase::Summary),
            Phase::InSection => {
                emit(DocEvent::FailureLine(line))?;
                Ok(Phase::InSection)
            }
            Phase::BeforeSections | Phase::Summary => Ok(phase),
        }
    }

    fn results_line<Error>(
        line: &[u8],
        emit: &mut impl FnMut(DocEvent<'_>) -> Result<(), Error>,
    ) -> Result<Phase, Error> {
        if line == b"failures:" {
            return Ok(Phase::BeforeSections);
        }
        if let Some(count) = running_count(line) {
            emit(DocEvent::Running { count })?;
        } else if let Some((name, outcome)) = result_line(line) {
            emit(DocEvent::Result { name, outcome })?;
        }
        Ok(Phase::Results)
    }
}

impl Default for DocParser {
    fn default() -> Self {
        Self::new()
    }
}

fn strip_carriage_return(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn running_count(line: &[u8]) -> Option<usize> {
    let rest = line.strip_prefix(b"running ")?;
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let (number, tail) = rest.split_at(digits);
    if tail != b" tests" && tail != b" test" {
        return None;
    }
    core::str::from_utf8(number).ok()?.parse().ok()
}

fn result_line(line: &[u8]) -> Option<(&[u8], DocOutcome)> {
    let body = line.strip_prefix(b"test ")?;
    let split = body
        .windows(RESULT_MARKER.len())
        .rposition(|window| window == RESULT_MARKER)?;
    let name = &body[..split];
    let verdict = &body[split + RESULT_MARKER.len()..];
    let outcome = if verdict == b"ok" {
        DocOutcome::Passed
    } else if verdict == b"FAILED" {
        DocOutcome::Failed
    } else if verdict.starts_with(b"ignored") {
        DocOutcome::Ignored
    } else {
        return None;
    };
    Some((name, outcome))
}

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
    /// The line contains bytes that are not valid UTF-8.
    InvalidUtf8,
}

/// Parses one already framed libtest line; the framer owns the line-length bound.
///
/// # Errors
///
/// Returns [`ListingError::InvalidUtf8`] when the line is not UTF-8.
pub fn parse_listing_line(frame: &[u8]) -> Result<ListingLine<'_>, ListingError> {
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

/// One listing read or record failure.
#[derive(Debug, PartialEq, Eq)]
pub enum ListingSourceError<ReadError> {
    /// Source storage cannot accept any bytes.
    EmptyBuffer,
    /// The reader failed while producing a chunk.
    Read(ReadError),
    /// The reader reported more bytes than the supplied storage can hold.
    InvalidReadLength { length: usize, capacity: usize },
    /// The framer rejected a listing line or the libtest parser rejected a framed one.
    Record(RecordFault<ListingError>),
    /// The framer rejected end of stream or its own state outside a record step.
    Framing(ArtifactStreamError),
}

/// One capacity-gated poll result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingSourceStep<'executable, 'scratch, 'name> {
    /// Launch this exact test before asking for another listing byte.
    Launch(ExactTestIntent<'executable, 'scratch, 'name>),
    /// No slot is available; the source left bytes and reader untouched.
    Full,
    /// The reader produced bytes in this step, or zero when it reported EOF.
    Read {
        /// Bytes newly read from listing stdout in this step.
        bytes: usize,
    },
    /// Input was staged without completing a line, or no input was pending.
    NeedMore,
    /// A complete non-test listing line was consumed in this step.
    Ignored,
    /// One test name did not match the configured filter.
    Filtered,
    /// One `#[ignore]`d test name that this run reports without launching.
    IgnoredTest(&'name str),
    /// Listing stdout reached EOF and all framed lines were drained.
    Eof,
}

/// Fixed caller-owned listing chunk and its unread suffix.
pub struct ListingSource<'buffer> {
    buffer: &'buffer mut [u8],
    consumed: usize,
    filled: usize,
    eof: bool,
}

impl<'buffer> ListingSource<'buffer> {
    /// Binds caller-owned storage to one libtest listing stdout reader.
    ///
    /// # Errors
    ///
    /// Returns [`ListingSourceError::EmptyBuffer`] when `buffer` has no capacity.
    pub fn new(
        buffer: &'buffer mut [u8],
    ) -> Result<Self, ListingSourceError<core::convert::Infallible>> {
        if buffer.is_empty() {
            return Err(ListingSourceError::EmptyBuffer);
        }
        Ok(Self {
            buffer,
            consumed: 0,
            filled: 0,
            eof: false,
        })
    }

    /// Polls one read or parse step only while a child slot is free.
    ///
    /// A launched intent borrows the source chunk or framer storage. The caller
    /// must launch or otherwise consume it before polling this source again.
    pub fn poll_next<'call, 'executable, 'scratch, Reader: AsyncRead, T, const CAPACITY: usize>(
        &'call mut self,
        mut reader: Pin<&mut Reader>,
        framer: &'call mut ArtifactFramer<'_>,
        sessions: &SessionSlots<T, CAPACITY>,
        executable: ExecutablePath<'executable, 'scratch>,
        filter: NameFilter<'_>,
        context: &mut Context<'_>,
    ) -> Poll<
        Result<ListingSourceStep<'executable, 'scratch, 'call>, ListingSourceError<Reader::Error>>,
    > {
        if sessions.is_full() {
            return Poll::Ready(Ok(ListingSourceStep::Full));
        }
        let frame_ready = match framer.frame_ready() {
            Ok(ready) => ready,
            Err(error) => return Poll::Ready(Err(ListingSourceError::Framing(error))),
        };
        if frame_ready || self.consumed < self.filled {
            let chunk = if frame_ready {
                &[][..]
            } else {
                &self.buffer[self.consumed..self.filled]
            };
            return Poll::Ready(Self::record_step(framer, chunk, executable, filter).map(
                |(step, consumed_bytes)| {
                    self.consumed += consumed_bytes;
                    step
                },
            ));
        }

        if self.eof {
            return Poll::Ready(Self::finish(framer));
        }
        match reader.as_mut().poll_read(context, self.buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(ListingSourceError::Read(error))),
            Poll::Ready(Ok(0)) => {
                self.eof = true;
                Poll::Ready(Ok(ListingSourceStep::Read { bytes: 0 }))
            }
            Poll::Ready(Ok(length)) if length <= self.buffer.len() => {
                self.consumed = 0;
                self.filled = length;
                Poll::Ready(Ok(ListingSourceStep::Read { bytes: length }))
            }
            Poll::Ready(Ok(length)) => Poll::Ready(Err(ListingSourceError::InvalidReadLength {
                length,
                capacity: self.buffer.len(),
            })),
        }
    }

    /// Parses `chunk`, lent by whoever owns its storage, without copying.
    ///
    /// The returned count is how much of the chunk was consumed; the caller
    /// replays the unconsumed tail and releases the storage once the count
    /// reaches the chunk length. A full session table returns
    /// [`ListingSourceStep::Full`] with zero consumed, exactly as
    /// [`Self::poll_next`] does.
    pub fn poll_lent<'call, 'executable, 'scratch, T, const CAPACITY: usize>(
        framer: &'call mut ArtifactFramer<'_>,
        sessions: &SessionSlots<T, CAPACITY>,
        chunk: &'call [u8],
        executable: ExecutablePath<'executable, 'scratch>,
        filter: NameFilter<'_>,
        _context: &mut Context<'_>,
    ) -> Poll<
        Result<
            (ListingSourceStep<'executable, 'scratch, 'call>, usize),
            ListingSourceError<core::convert::Infallible>,
        >,
    > {
        if sessions.is_full() {
            return Poll::Ready(Ok((ListingSourceStep::Full, 0)));
        }
        let frame_ready = match framer.frame_ready() {
            Ok(ready) => ready,
            Err(error) => return Poll::Ready(Err(ListingSourceError::Framing(error))),
        };
        if !frame_ready && chunk.is_empty() {
            return Poll::Ready(Ok((ListingSourceStep::NeedMore, 0)));
        }
        let lent = if frame_ready { &[][..] } else { chunk };
        Poll::Ready(Self::record_step(framer, lent, executable, filter))
    }

    /// Runs the end-of-stream drain for a completion owner that observed EOF.
    pub fn poll_lent_eof<'call, 'executable, 'scratch, T, const CAPACITY: usize>(
        framer: &'call mut ArtifactFramer<'_>,
        sessions: &SessionSlots<T, CAPACITY>,
        executable: ExecutablePath<'executable, 'scratch>,
        filter: NameFilter<'_>,
        _context: &mut Context<'_>,
    ) -> Poll<
        Result<
            ListingSourceStep<'executable, 'scratch, 'call>,
            ListingSourceError<core::convert::Infallible>,
        >,
    > {
        if sessions.is_full() {
            return Poll::Ready(Ok(ListingSourceStep::Full));
        }
        let frame_ready = match framer.frame_ready() {
            Ok(ready) => ready,
            Err(error) => return Poll::Ready(Err(ListingSourceError::Framing(error))),
        };
        if frame_ready {
            return Poll::Ready(
                Self::record_step(framer, &[], executable, filter).map(|(step, _)| step),
            );
        }
        Poll::Ready(Self::finish(framer))
    }

    fn finish<ReadError>(
        framer: &mut ArtifactFramer<'_>,
    ) -> Result<ListingSourceStep<'static, 'static, 'static>, ListingSourceError<ReadError>> {
        framer
            .finish()
            .map(|()| ListingSourceStep::Eof)
            .map_err(ListingSourceError::Framing)
    }

    fn record_step<'call, 'executable, 'scratch, ReadError>(
        framer: &'call mut ArtifactFramer<'_>,
        chunk: &'call [u8],
        executable: ExecutablePath<'executable, 'scratch>,
        filter: NameFilter<'_>,
    ) -> Result<
        (ListingSourceStep<'executable, 'scratch, 'call>, usize),
        ListingSourceError<ReadError>,
    > {
        let step = framer
            .next_record(chunk, parse_listing_line)
            .map_err(ListingSourceError::Record)?;
        Ok(match step {
            RecordStep::NeedMore { consumed_bytes } => {
                (ListingSourceStep::NeedMore, consumed_bytes)
            }
            RecordStep::Parsed {
                parsed: ListingLine::Test(name),
                consumed_bytes,
            } => (
                match filter.disposition(name) {
                    Disposition::Launch => {
                        ListingSourceStep::Launch(ExactTestIntent::new(executable, name))
                    }
                    Disposition::Filtered => ListingSourceStep::Filtered,
                    Disposition::Ignored => ListingSourceStep::IgnoredTest(name),
                },
                consumed_bytes,
            ),
            RecordStep::Parsed {
                parsed: ListingLine::Ignored,
                consumed_bytes,
            } => (ListingSourceStep::Ignored, consumed_bytes),
        })
    }
}

#[cfg(test)]
mod doc_report_tests {
    use std::string::{String, ToString};
    use std::vec::Vec;

    use super::{DocEvent, DocOutcome, DocParser};

    const REAL_REPORT: &str = include_str!("../tests/fixtures/doc-output/cargo-test-doc.stdout");

    #[derive(Debug, PartialEq, Eq)]
    enum Owned {
        Running(usize),
        Result(String, DocOutcome),
        Failure(String),
        Finished,
    }

    fn events(chunks: &[&[u8]]) -> Vec<Owned> {
        let mut parser = DocParser::new();
        let mut seen = Vec::new();
        for chunk in chunks {
            parser
                .feed::<()>(chunk, |event| {
                    seen.push(match event {
                        DocEvent::Running { count } => Owned::Running(count),
                        DocEvent::Result { name, outcome } => {
                            Owned::Result(String::from_utf8_lossy(name).to_string(), outcome)
                        }
                        DocEvent::FailureLine(line) => {
                            Owned::Failure(String::from_utf8_lossy(line).to_string())
                        }
                        DocEvent::Finished => Owned::Finished,
                    });
                    Ok(())
                })
                .expect("collecting events cannot fail");
        }
        seen
    }

    #[test]
    fn c68_real_cargo_test_doc_report_yields_one_pass_one_fail_one_ignored() {
        let seen = events(&[REAL_REPORT.as_bytes()]);

        let results = seen
            .iter()
            .filter(|event| matches!(event, Owned::Result(..)))
            .collect::<Vec<_>>();
        assert_eq!(seen.first(), Some(&Owned::Running(3)));
        assert_eq!(results.len(), 3);
        assert!(seen.contains(&Owned::Result(
            "src/lib.rs - add (line 3)".to_string(),
            DocOutcome::Passed
        )));
        assert!(seen.contains(&Owned::Result(
            "src/lib.rs - subtract (line 12)".to_string(),
            DocOutcome::Failed
        )));
        assert!(seen.contains(&Owned::Result(
            "src/lib.rs - needs_device (line 21)".to_string(),
            DocOutcome::Ignored
        )));
        assert_eq!(seen.last(), Some(&Owned::Finished));
    }

    #[test]
    fn c68_failure_output_is_the_failing_section_only_and_includes_the_panic_message() {
        let seen = events(&[REAL_REPORT.as_bytes()]);

        let failure_lines = seen
            .iter()
            .filter_map(|event| match event {
                Owned::Failure(line) => Some(line.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            failure_lines.first(),
            Some(&"---- src/lib.rs - subtract (line 12) stdout ----")
        );
        assert!(failure_lines.iter().any(|line| line.contains("left: 2")));
        assert!(failure_lines.iter().any(|line| line.contains("right: 3")));
        assert!(
            !failure_lines
                .iter()
                .any(|line| line.starts_with("failures:"))
        );
        assert!(
            !failure_lines
                .iter()
                .any(|line| line.starts_with("test result:"))
        );
    }

    #[test]
    fn c68_split_chunks_produce_the_same_events_as_one_chunk() {
        let whole = events(&[REAL_REPORT.as_bytes()]);
        let bytes = REAL_REPORT.as_bytes();
        let chunks = bytes.chunks(7).collect::<Vec<_>>();

        assert_eq!(events(&chunks), whole);
    }

    fn lf_report() -> String {
        REAL_REPORT.replace("\r\n", "\n")
    }

    fn crlf_report() -> String {
        lf_report().replace('\n', "\r\n")
    }

    #[test]
    fn c68_crlf_report_yields_the_same_events_as_lf() {
        let crlf = crlf_report();

        assert_eq!(
            events(&[crlf.as_bytes()]),
            events(&[lf_report().as_bytes()])
        );
    }

    #[test]
    fn c68_crlf_report_split_at_every_boundary_yields_the_same_events_as_lf() {
        let crlf = crlf_report();
        let bytes = crlf.as_bytes();
        let expected = events(&[lf_report().as_bytes()]);

        for split in 0..=bytes.len() {
            let (head, tail) = bytes.split_at(split);
            assert_eq!(events(&[head, tail]), expected, "split at byte {split}");
        }
    }

    #[test]
    fn c68_crlf_report_split_into_single_bytes_yields_the_same_events_as_lf() {
        let crlf = crlf_report();
        let chunks = crlf.as_bytes().chunks(1).collect::<Vec<_>>();

        assert_eq!(events(&chunks), events(&[lf_report().as_bytes()]));
    }

    #[test]
    fn c68_zero_doctest_report_is_a_running_zero_block() {
        let seen = events(&[b"\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n"]);

        assert_eq!(seen, [Owned::Running(0), Owned::Finished]);
    }

    #[test]
    fn c68_singular_running_header_and_unknown_verdicts_are_handled() {
        let seen = events(&[b"running 1 test\ntest src/lib.rs - a (line 1) ... bench: 3 ns/iter\ntest src/lib.rs - b (line 2) ... ok\n"]);

        assert_eq!(
            seen,
            [
                Owned::Running(1),
                Owned::Result("src/lib.rs - b (line 2)".to_string(), DocOutcome::Passed)
            ]
        );
    }

    #[test]
    fn c68_overlong_lines_are_truncated_without_corrupting_the_next_line() {
        let mut report = std::vec![b'x'; 5000];
        report.extend_from_slice(b"\nrunning 0 tests\n");

        assert_eq!(events(&[&report]), [Owned::Running(0)]);
    }
}

#[cfg(test)]
mod listing_source_tests {
    use core::cell::Cell;
    use core::convert::Infallible;
    use core::future::Future;
    use core::hint::black_box;
    use core::marker::PhantomData;
    use core::pin::{Pin, pin};
    use core::task::{Context, Poll, Waker};

    use proxima_core::io::AsyncRead;
    use proxima_primitives::pipe::Pipe;

    use super::{ListingSource, ListingSourceError, ListingSourceStep};
    use crate::ExecutablePath;
    use crate::artifact_stream::{ArtifactFramer, ArtifactStreamError, RecordFault};
    use crate::discover_listing::ListingError;
    use crate::schedule::SessionSlots;
    use crate::selection::NameFilter;
    use crate::test_intent::ExactTestIntent;

    const ALPHA: &[u8] = b"alpha: test\n";
    const BETA: &[u8] = b"beta: test\n";
    const EXECUTABLE: ExecutablePath<'static, 'static> =
        ExecutablePath::Borrowed("/fixture/test-binary");
    const EVERYTHING: NameFilter<'static> = NameFilter::Substring("");

    struct HeldListingReader {
        chunks: [&'static [u8]; 2],
        next_chunk: usize,
        read_calls: usize,
        release_eof: bool,
        eof_seen: bool,
    }

    impl HeldListingReader {
        fn new() -> Self {
            Self {
                chunks: [ALPHA, BETA],
                next_chunk: 0,
                read_calls: 0,
                release_eof: false,
                eof_seen: false,
            }
        }
    }

    impl AsyncRead for HeldListingReader {
        type Error = Infallible;

        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<Result<usize, Self::Error>> {
            let reader = self.get_mut();
            reader.read_calls += 1;
            if let Some(chunk) = reader.chunks.get(reader.next_chunk) {
                reader.next_chunk += 1;
                buffer[..chunk.len()].copy_from_slice(chunk);
                return Poll::Ready(Ok(chunk.len()));
            }
            if reader.release_eof {
                reader.eof_seen = true;
                Poll::Ready(Ok(0))
            } else {
                Poll::Pending
            }
        }
    }

    struct MockLaunch<'count, 'executable, 'scratch, 'name> {
        count: &'count Cell<usize>,
        intent: PhantomData<ExactTestIntent<'executable, 'scratch, 'name>>,
    }

    impl<'executable, 'scratch, 'name> Pipe for MockLaunch<'_, 'executable, 'scratch, 'name> {
        type In = ExactTestIntent<'executable, 'scratch, 'name>;
        type Out = ();
        type Err = Infallible;

        fn call(&self, _input: Self::In) -> impl Future<Output = Result<Self::Out, Self::Err>> {
            self.count.set(self.count.get() + 1);
            core::future::ready(Ok(()))
        }
    }

    fn launch_mock(
        intent: ExactTestIntent<'_, '_, '_>,
        count: &Cell<usize>,
        context: &mut Context<'_>,
    ) -> *const u8 {
        let name_pointer = intent.test_name().as_ptr();
        let launch = MockLaunch {
            count,
            intent: PhantomData,
        };
        let future = launch.call(intent);
        let mut future = pin!(future);
        assert!(matches!(
            Future::poll(future.as_mut(), context),
            Poll::Ready(Ok(()))
        ));
        name_pointer
    }

    #[derive(Clone, Copy)]
    struct TimingObservation {
        intent_emitted: bool,
        eof_seen: bool,
    }

    fn intent_before_eof(observation: TimingObservation) -> bool {
        observation.intent_emitted && !observation.eof_seen
    }

    fn launch_count_matches(observed: usize, expected: usize) -> bool {
        observed == expected
    }

    fn eof_gated_control_launch(intent_available: bool, listing_eof_seen: bool) -> bool {
        intent_available && listing_eof_seen
    }

    type LentOutcome<'call> = Poll<
        Result<(ListingSourceStep<'static, 'static, 'call>, usize), ListingSourceError<Infallible>>,
    >;

    fn lend<'call, T, const CAPACITY: usize>(
        framer: &'call mut ArtifactFramer<'_>,
        sessions: &SessionSlots<T, CAPACITY>,
        chunk: &'call [u8],
    ) -> LentOutcome<'call> {
        let mut context = Context::from_waker(Waker::noop());
        ListingSource::poll_lent(
            framer,
            sessions,
            chunk,
            EXECUTABLE,
            EVERYTHING,
            &mut context,
        )
    }

    fn launched_name(outcome: LentOutcome<'_>) -> (std::string::String, usize) {
        match outcome {
            Poll::Ready(Ok((ListingSourceStep::Launch(intent), consumed_bytes))) => (
                std::string::String::from(intent.test_name()),
                consumed_bytes,
            ),
            other => panic!("listing line did not launch: {other:?}"),
        }
    }

    fn launch_next(
        source: &mut ListingSource<'_>,
        reader: &mut HeldListingReader,
        framer: &mut ArtifactFramer<'_>,
        sessions: &SessionSlots<(), 1>,
        context: &mut Context<'_>,
        launch_count: &Cell<usize>,
        expected_name: &str,
    ) -> bool {
        let source_pointer = source.buffer.as_ptr();
        loop {
            match source.poll_next(
                Pin::new(&mut *reader),
                framer,
                sessions,
                EXECUTABLE,
                EVERYTHING,
                context,
            ) {
                Poll::Ready(Ok(ListingSourceStep::Launch(intent))) => {
                    assert_eq!(intent.test_name(), expected_name);
                    assert!(intent_before_eof(TimingObservation {
                        intent_emitted: true,
                        eof_seen: reader.eof_seen,
                    }));
                    return launch_mock(intent, launch_count, context) == source_pointer;
                }
                Poll::Ready(Ok(
                    ListingSourceStep::Read { .. }
                    | ListingSourceStep::NeedMore
                    | ListingSourceStep::Ignored
                    | ListingSourceStep::Filtered
                    | ListingSourceStep::IgnoredTest(_),
                )) => {}
                Poll::Ready(Ok(ListingSourceStep::Full | ListingSourceStep::Eof)) => {
                    panic!("{expected_name} was withheld")
                }
                Poll::Ready(Err(error)) => panic!("listing source failed: {error:?}"),
                Poll::Pending => panic!("{expected_name} listing chunk is already available"),
            }
        }
    }

    #[test]
    fn c50_launches_one_borrowed_intent_then_pauses_at_full_capacity() {
        let (observation, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut source_storage = [0_u8; 64];
            let mut frame_storage = [0_u8; 65];
            let mut source = ListingSource::new(&mut source_storage).expect("source has storage");
            let mut framer =
                ArtifactFramer::new(&mut frame_storage, 64).expect("framer accepts a listing line");
            let mut sessions = SessionSlots::<(), 1>::new();
            let mut reader = HeldListingReader::new();
            let mut context = Context::from_waker(Waker::noop());
            let launch_count = Cell::new(0);

            let first_pointer_identity = launch_next(
                &mut source,
                &mut reader,
                &mut framer,
                &sessions,
                &mut context,
                &launch_count,
                "alpha",
            );
            assert!(framer.is_empty());
            let active = sessions
                .insert(())
                .expect("first child takes the only slot");
            let calls_at_full = reader.read_calls;
            let cursor_at_full = source.consumed;
            assert!(matches!(
                source.poll_next(
                    Pin::new(&mut reader),
                    &mut framer,
                    &sessions,
                    EXECUTABLE,
                    EVERYTHING,
                    &mut context,
                ),
                Poll::Ready(Ok(ListingSourceStep::Full))
            ));
            let no_further_reads_while_full = reader.read_calls == calls_at_full;
            let no_bytes_consumed_while_full = source.consumed == cursor_at_full;
            assert_eq!(sessions.remove(active), Some(()));

            let second_pointer_identity = launch_next(
                &mut source,
                &mut reader,
                &mut framer,
                &sessions,
                &mut context,
                &launch_count,
                "beta",
            );
            (
                launch_count.get(),
                first_pointer_identity,
                second_pointer_identity,
                no_further_reads_while_full,
                no_bytes_consumed_while_full,
                !reader.eof_seen,
                reader.read_calls,
            )
        });
        assert_eq!(observation, (2, true, true, true, true, true, 2));
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"mock_launches\":2,\"first_intent_before_eof\":1,\"borrowed_name_pointer_identities\":2,\"full_slot_read_delta\":0,\"full_slot_consumed_delta\":0,\"resumed_after_retirement\":1,\"source_allocations\":{allocations}}}"
        );
    }

    #[test]
    fn c50_collect_all_control_emits_first_intent_only_after_eof() {
        let mut reader = HeldListingReader::new();
        let mut context = Context::from_waker(Waker::noop());
        let mut collected = [0_u8; 64];
        let mut collected_length = 0;
        for expected in [ALPHA, BETA] {
            let read =
                Pin::new(&mut reader).poll_read(&mut context, &mut collected[collected_length..]);
            assert!(matches!(read, Poll::Ready(Ok(length)) if length == expected.len()));
            collected_length += expected.len();
        }
        let pending =
            Pin::new(&mut reader).poll_read(&mut context, &mut collected[collected_length..]);
        assert!(matches!(pending, Poll::Pending));
        assert!(!intent_before_eof(TimingObservation {
            intent_emitted: false,
            eof_seen: reader.eof_seen,
        }));

        reader.release_eof = true;
        assert!(matches!(
            Pin::new(&mut reader).poll_read(&mut context, &mut collected[collected_length..]),
            Poll::Ready(Ok(0))
        ));
        let mut frame_storage = [0_u8; 65];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 64).expect("listing framer has storage");
        let sessions = SessionSlots::<(), 1>::new();
        let (name, _) = launched_name(lend(&mut framer, &sessions, &collected[..collected_length]));
        assert_eq!(name, "alpha");
        assert!(!intent_before_eof(TimingObservation {
            intent_emitted: true,
            eof_seen: reader.eof_seen,
        }));
        std::println!(
            "{{\"collector_pending_before_eof\":1,\"collector_first_intent_after_eof\":1,\"same_comparator_rejected\":1}}"
        );
    }

    #[test]
    fn c50_direct_listing_replays_tail_without_copying_complete_lines() {
        let combined = b"alpha: test\nbeta: test\n";
        let mut frame_storage = [0_u8; 65];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 64).expect("listing frame storage exists");
        let sessions = SessionSlots::<(), 1>::new();
        assert!(framer.is_empty());
        let consumed = match lend(&mut framer, &sessions, combined) {
            Poll::Ready(Ok((ListingSourceStep::Launch(intent), consumed_bytes))) => {
                assert_eq!(intent.test_name(), "alpha");
                assert_eq!(intent.test_name().as_ptr(), combined.as_ptr());
                assert_eq!(consumed_bytes, ALPHA.len());
                consumed_bytes
            }
            other => panic!("first test line was not selected: {other:?}"),
        };
        assert!(framer.is_empty());
        match lend(&mut framer, &sessions, &combined[consumed..]) {
            Poll::Ready(Ok((ListingSourceStep::Launch(intent), consumed_bytes))) => {
                assert_eq!(intent.test_name(), "beta");
                assert_eq!(intent.test_name().as_ptr(), combined[consumed..].as_ptr());
                assert_eq!(consumed_bytes, BETA.len());
            }
            other => panic!("second test line was not selected: {other:?}"),
        }
        assert!(framer.is_empty());
        framer
            .finish()
            .expect("complete direct lines leave no pending EOF data");
        std::println!(
            "{{\"direct_borrowed_name_pointers\":2,\"unread_tail_replayed\":1,\"complete_direct_lines_pending_bytes\":0,\"direct_eof_drain\":1}}"
        );
    }

    fn rejection(outcome: LentOutcome<'_>) -> RecordFault<ListingError> {
        match outcome {
            Poll::Ready(Err(ListingSourceError::Record(fault))) => fault,
            other => panic!("line was not rejected as a record fault: {other:?}"),
        }
    }

    #[test]
    fn c50_direct_and_buffered_errors_keep_framing_and_poison_contracts() {
        let sessions = SessionSlots::<(), 1>::new();
        let invalid = b"\xff: test\n";
        let mut direct_storage = [0_u8; 65];
        let mut direct_framer =
            ArtifactFramer::new(&mut direct_storage, 64).expect("direct frame storage exists");
        let direct_error = rejection(lend(&mut direct_framer, &sessions, invalid));
        assert_eq!(
            direct_error,
            RecordFault::Parse {
                consumed_bytes: invalid.len(),
                error: ListingError::InvalidUtf8
            }
        );
        let poisoned = RecordFault::Framing {
            consumed_bytes: 0,
            error: ArtifactStreamError::Poisoned,
        };
        assert_eq!(
            rejection(lend(&mut direct_framer, &sessions, ALPHA)),
            poisoned
        );

        let mut buffered_storage = [0_u8; 65];
        let mut buffered_framer =
            ArtifactFramer::new(&mut buffered_storage, 64).expect("buffered frame storage exists");
        assert!(matches!(
            lend(&mut buffered_framer, &sessions, b"\xff: te"),
            Poll::Ready(Ok((ListingSourceStep::NeedMore, 5)))
        ));
        assert!(!buffered_framer.is_empty());
        let buffered_error = rejection(lend(&mut buffered_framer, &sessions, b"st\n"));
        assert_eq!(
            buffered_error,
            RecordFault::Parse {
                consumed_bytes: 3,
                error: ListingError::InvalidUtf8
            }
        );
        assert_eq!(
            rejection(lend(&mut buffered_framer, &sessions, ALPHA)),
            poisoned
        );

        let oversized_line = b"abcdefghi\n";
        let oversize = RecordFault::Framing {
            consumed_bytes: 0,
            error: ArtifactStreamError::Framing(proxima_codec::FrameError::FrameTooLarge {
                len: 9,
            }),
        };
        let mut direct_limit_storage = [0_u8; 10];
        let mut direct_limit_framer =
            ArtifactFramer::new(&mut direct_limit_storage, 8).expect("direct limit storage exists");
        assert_eq!(
            rejection(lend(&mut direct_limit_framer, &sessions, oversized_line)),
            oversize
        );
        assert_eq!(
            rejection(lend(&mut direct_limit_framer, &sessions, oversized_line)),
            oversize
        );
        let mut buffered_limit_storage = [0_u8; 10];
        let mut buffered_limit_framer = ArtifactFramer::new(&mut buffered_limit_storage, 8)
            .expect("buffered limit storage exists");
        assert!(matches!(
            lend(&mut buffered_limit_framer, &sessions, b"abcdefgh"),
            Poll::Ready(Ok((ListingSourceStep::NeedMore, 8)))
        ));
        let buffered_limit_error = rejection(lend(&mut buffered_limit_framer, &sessions, b"i\n"));
        assert!(matches!(
            buffered_limit_error,
            RecordFault::Framing {
                error: ArtifactStreamError::Framing(proxima_codec::FrameError::FrameTooLarge {
                    len: 9
                }),
                ..
            }
        ));
        assert!(matches!(
            lend(&mut buffered_limit_framer, &sessions, b""),
            Poll::Ready(Err(ListingSourceError::Framing(
                ArtifactStreamError::Framing(proxima_codec::FrameError::FrameTooLarge { len: 9 })
            )))
        ));
        let mut eof_storage = [0_u8; 10];
        let mut eof_framer =
            ArtifactFramer::new(&mut eof_storage, 8).expect("EOF frame storage exists");
        assert!(matches!(
            lend(&mut eof_framer, &sessions, b"partial"),
            Poll::Ready(Ok((ListingSourceStep::NeedMore, 7)))
        ));
        assert_eq!(
            eof_framer.finish(),
            Err(ArtifactStreamError::UnterminatedRecord { length: 7 })
        );
        std::println!(
            "{{\"direct_utf8_error\":\"{direct_error:?}\",\"buffered_utf8_error\":\"{buffered_error:?}\",\"direct_poisoned\":1,\"buffered_poisoned\":1,\"oversize_error\":\"{oversize:?}\",\"direct_oversize_retry_same\":1,\"buffered_oversize_retry_same\":1,\"fragmented_line_staged\":1,\"partial_eof_rejected\":1}}"
        );
    }

    #[test]
    fn stops_consuming_listing_when_live_child_slots_are_full() {
        let mut buffer = [0; 65];
        let mut framer = ArtifactFramer::new(&mut buffer, 64).expect("fixed listing capacity");
        let mut sessions = SessionSlots::<&str, 1>::new();
        assert_eq!(
            launched_name(lend(&mut framer, &sessions, b"alpha: test\n")),
            (std::string::String::from("alpha"), 12)
        );
        let active = sessions.insert("alpha child").expect("one child slot");
        assert!(matches!(
            lend(&mut framer, &sessions, b"beta: test\n"),
            Poll::Ready(Ok((ListingSourceStep::Full, 0)))
        ));
        assert!(!framer.frame_ready().expect("listing state remains valid"));
        assert_eq!(sessions.get(active), Some(&"alpha child"));
        std::println!(
            "{{\"active_child\":\"alpha child\",\"consumed_bytes\":0,\"event\":\"pull_while_full\",\"listing_record\":\"beta: test\\n\",\"result\":\"Full\"}}"
        );
    }

    #[test]
    fn resumes_listing_after_child_slot_is_retired() {
        let mut buffer = [0; 65];
        let mut framer = ArtifactFramer::new(&mut buffer, 64).expect("fixed listing capacity");
        let mut sessions = SessionSlots::<&str, 1>::new();
        let active = sessions.insert("alpha child").expect("one child slot");
        assert!(matches!(
            lend(&mut framer, &sessions, b"beta: test\n"),
            Poll::Ready(Ok((ListingSourceStep::Full, 0)))
        ));
        assert_eq!(sessions.remove(active), Some("alpha child"));
        assert_eq!(
            launched_name(lend(&mut framer, &sessions, b"beta: test\n")),
            (std::string::String::from("beta"), 11)
        );
        std::println!(
            "{{\"borrowed_name\":\"beta\",\"consumed_bytes\":11,\"event\":\"pull_after_retire\",\"listing_record\":\"beta: test\\n\",\"result\":\"Launch\"}}"
        );
    }

    #[test]
    fn dispatch_pipe_does_not_allocate_and_probe_detects_allocation() {
        let mut buffer = [0; 65];
        let mut framer = ArtifactFramer::new(&mut buffer, 64).expect("fixed listing capacity");
        let sessions = SessionSlots::<&str, 1>::new();
        let (output, dispatch_allocations) = crate::test_alloc::measure_allocations(|| {
            matches!(
                lend(&mut framer, &sessions, b"beta: test\n"),
                Poll::Ready(Ok((ListingSourceStep::Launch(intent), 11)))
                    if intent.test_name() == "beta"
            )
        });

        let (allocation_control, positive_control_allocations) =
            crate::test_alloc::measure_allocations(|| std::vec::Vec::<u8>::with_capacity(1));
        black_box(allocation_control);

        assert!(output);
        assert_eq!(dispatch_allocations, 0);
        assert_eq!(positive_control_allocations, 1);
        std::println!(
            "{{\"dispatch_allocations\":{dispatch_allocations},\"event\":\"allocation_probe\",\"positive_control_allocations\":{positive_control_allocations}}}"
        );
    }

    #[test]
    fn launches_before_eof_and_only_pulls_again_after_slot_retirement() {
        let listing = b"alpha: test\nbeta: test\n";
        let (result, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut storage = [0; 65];
            let mut framer = ArtifactFramer::new(&mut storage, 64).expect("listing storage fits");
            let mut sessions = SessionSlots::<(), 1>::new();
            let first = lend(&mut framer, &sessions, &listing[..12]);
            let first_launch = matches!(
                &first,
                Poll::Ready(Ok((ListingSourceStep::Launch(intent), 12)))
                    if intent.test_name() == "alpha"
            );
            let first_slot = sessions.insert(()).expect("child slot available");
            let blocked = lend(&mut framer, &sessions, &listing[12..]);
            let blocked_without_consuming =
                matches!(blocked, Poll::Ready(Ok((ListingSourceStep::Full, 0))));
            sessions
                .remove(first_slot)
                .expect("retired child releases its live slot");
            let resumed = lend(&mut framer, &sessions, &listing[12..]);
            let second_launch = matches!(
                &resumed,
                Poll::Ready(Ok((ListingSourceStep::Launch(intent), 11)))
                    if intent.test_name() == "beta"
            );
            (
                first_launch,
                usize::from(first_launch),
                blocked_without_consuming,
                second_launch,
                usize::from(second_launch),
            )
        });
        let (
            first_launch,
            first_launch_count,
            blocked_without_consuming,
            second_launch,
            second_launch_count,
        ) = result;
        assert!(first_launch);
        assert!(blocked_without_consuming);
        assert!(second_launch);
        assert!(launch_count_matches(first_launch_count, 1));
        assert_eq!(second_launch_count, 1);
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"first_launch_before_eof\":{first_launch_count},\"listing_bytes_consumed_while_full\":0,\"launch_after_exit\":{second_launch_count},\"allocations\":{allocations}}}"
        );
    }

    #[test]
    fn rejects_delaying_first_launch_until_listing_eof() {
        let mut storage = [0; 65];
        let mut framer = ArtifactFramer::new(&mut storage, 64).expect("listing storage fits");
        let sessions = SessionSlots::<(), 1>::new();
        let actual = lend(&mut framer, &sessions, b"alpha: test\n");
        let actual_intent_available =
            matches!(actual, Poll::Ready(Ok((ListingSourceStep::Launch(_), _))));
        let listing_eof_seen = false;
        let delayed_control_launch_before_eof =
            eof_gated_control_launch(actual_intent_available, listing_eof_seen);
        let delayed_control_launch_after_eof =
            eof_gated_control_launch(actual_intent_available, true);

        assert!(launch_count_matches(
            usize::from(actual_intent_available),
            1
        ));
        assert!(!launch_count_matches(
            usize::from(delayed_control_launch_before_eof),
            1
        ));
        assert!(delayed_control_launch_after_eof);
        std::println!(
            "{{\"actual_intent_before_eof\":{},\"delayed_control_before_eof\":{},\"delayed_control_after_eof\":{},\"late_launch_control_rejected\":1}}",
            usize::from(actual_intent_available),
            usize::from(delayed_control_launch_before_eof),
            usize::from(delayed_control_launch_after_eof)
        );
    }

    const TERSE_LISTING: &[u8] = b"cargo_source::tests::alpha: test\nlisting_source::tests::beta: test\nbench_gamma: benchmark\nselection::tests::delta: test\n\n3 tests, 1 benchmarks\n";

    struct WholeListingReader {
        bytes: Option<&'static [u8]>,
    }

    impl AsyncRead for WholeListingReader {
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

    fn names_via_async_read() -> std::vec::Vec<std::string::String> {
        let mut source_storage = [0_u8; 1024];
        let mut frame_storage = [0_u8; 129];
        let mut source = ListingSource::new(&mut source_storage).expect("source has storage");
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 128).expect("framer accepts a listing line");
        let sessions = SessionSlots::<(), 4>::new();
        let mut reader = WholeListingReader {
            bytes: Some(TERSE_LISTING),
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut names = std::vec::Vec::new();
        loop {
            match source.poll_next(
                Pin::new(&mut reader),
                &mut framer,
                &sessions,
                EXECUTABLE,
                EVERYTHING,
                &mut context,
            ) {
                Poll::Ready(Ok(ListingSourceStep::Launch(intent))) => {
                    names.push(std::string::String::from(intent.test_name()));
                }
                Poll::Ready(Ok(
                    ListingSourceStep::Read { .. }
                    | ListingSourceStep::NeedMore
                    | ListingSourceStep::Ignored
                    | ListingSourceStep::Filtered,
                )) => {}
                Poll::Ready(Ok(ListingSourceStep::Eof)) => break names,
                other => panic!("async read path failed: {other:?}"),
            }
        }
    }

    fn feed_lent(
        chunk: &[u8],
        framer: &mut ArtifactFramer<'_>,
        sessions: &SessionSlots<(), 4>,
        names: &mut std::vec::Vec<std::string::String>,
    ) -> usize {
        let mut consumed = 0;
        while consumed < chunk.len() || framer.frame_ready().expect("framer state is readable") {
            match lend(framer, sessions, &chunk[consumed..]) {
                Poll::Ready(Ok((step, consumed_bytes))) => {
                    if let ListingSourceStep::Launch(intent) = step {
                        names.push(std::string::String::from(intent.test_name()));
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
        sessions: &SessionSlots<(), 4>,
        names: &mut std::vec::Vec<std::string::String>,
    ) {
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match ListingSource::poll_lent_eof(
                framer,
                sessions,
                EXECUTABLE,
                EVERYTHING,
                &mut context,
            ) {
                Poll::Ready(Ok(ListingSourceStep::Eof)) => break,
                Poll::Ready(Ok(ListingSourceStep::Launch(intent))) => {
                    names.push(std::string::String::from(intent.test_name()));
                }
                Poll::Ready(Ok(
                    ListingSourceStep::Read { .. }
                    | ListingSourceStep::NeedMore
                    | ListingSourceStep::Ignored
                    | ListingSourceStep::Filtered,
                )) => {}
                other => panic!("lent eof failed: {other:?}"),
            }
        }
    }

    #[test]
    fn lent_chunk_every_split_matches_async_read_for_the_libtest_terse_listing() {
        let reference = names_via_async_read();
        assert_eq!(reference.len(), 3);
        let sessions = SessionSlots::<(), 4>::new();
        let mut split_cases = 0;
        let mut intents_matched = 0;
        for split in 1..TERSE_LISTING.len() {
            let mut frame_storage = [0_u8; 129];
            let mut framer =
                ArtifactFramer::new(&mut frame_storage, 128).expect("framer accepts a line");
            let mut names = std::vec::Vec::new();
            for chunk in [&TERSE_LISTING[..split], &TERSE_LISTING[split..]] {
                let consumed = feed_lent(chunk, &mut framer, &sessions, &mut names);
                assert_eq!(consumed, chunk.len(), "split {split}");
            }
            drain_lent_eof(&mut framer, &sessions, &mut names);
            assert_eq!(names, reference, "split {split}");
            split_cases += 1;
            intents_matched += names.len();
        }
        assert_eq!(split_cases, TERSE_LISTING.len() - 1);
        std::println!(
            "{{\"split_cases_checked\":{split_cases},\"launch_intents_matched\":{intents_matched}}}"
        );
    }

    #[test]
    fn lent_chunk_with_sessions_full_returns_full_and_leaves_consumed_unchanged() {
        let mut frame_storage = [0_u8; 129];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 128).expect("framer accepts a line");
        let mut sessions = SessionSlots::<(), 1>::new();
        sessions.insert(()).expect("the only slot is free");
        let outcome = lend(&mut framer, &sessions, TERSE_LISTING);
        assert!(matches!(
            outcome,
            Poll::Ready(Ok((ListingSourceStep::Full, 0)))
        ));
        assert!(framer.is_empty());
        std::println!("{{\"full_cases_checked\":1,\"full_consumed\":0}}");
    }

    #[test]
    fn lent_eof_with_unterminated_line_returns_the_listing_framing_error() {
        let mut frame_storage = [0_u8; 129];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 128).expect("framer accepts a line");
        let sessions = SessionSlots::<(), 4>::new();
        let mut names = std::vec::Vec::new();
        let partial = &TERSE_LISTING[..10];
        assert_eq!(
            feed_lent(partial, &mut framer, &sessions, &mut names),
            partial.len()
        );
        let mut context = Context::from_waker(Waker::noop());
        let outcome = ListingSource::poll_lent_eof(
            &mut framer,
            &sessions,
            EXECUTABLE,
            EVERYTHING,
            &mut context,
        );
        std::println!("lent_eof_unterminated={outcome:?}");
        assert!(matches!(
            outcome,
            Poll::Ready(Err(ListingSourceError::Framing(
                ArtifactStreamError::UnterminatedRecord { length: 10 }
            )))
        ));
    }

    #[test]
    fn lent_zero_length_chunk_is_zero_consumed_need_more_and_leaves_the_framer_untouched() {
        let mut frame_storage = [0_u8; 129];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 128).expect("framer accepts a line");
        let sessions = SessionSlots::<(), 4>::new();
        let outcome = lend(&mut framer, &sessions, &[]);
        assert!(matches!(
            outcome,
            Poll::Ready(Ok((ListingSourceStep::NeedMore, 0)))
        ));
        assert!(framer.is_empty());
    }

    #[test]
    fn exact_filter_filters_a_superstring_name_and_launches_the_equal_one() {
        let mut frame_storage = [0_u8; 65];
        let mut framer =
            ArtifactFramer::new(&mut frame_storage, 64).expect("framer accepts a line");
        let sessions = SessionSlots::<(), 1>::new();
        let mut context = Context::from_waker(Waker::noop());
        let filter = NameFilter::Exact("alpha");
        let filtered = ListingSource::poll_lent(
            &mut framer,
            &sessions,
            b"alphabet: test\n",
            EXECUTABLE,
            filter,
            &mut context,
        );
        assert!(matches!(
            filtered,
            Poll::Ready(Ok((ListingSourceStep::Filtered, 15)))
        ));
        let launched = ListingSource::poll_lent(
            &mut framer,
            &sessions,
            ALPHA,
            EXECUTABLE,
            filter,
            &mut context,
        );
        assert!(matches!(
            launched,
            Poll::Ready(Ok((ListingSourceStep::Launch(intent), 12))) if intent.test_name() == "alpha"
        ));
        let summary = ListingSource::poll_lent(
            &mut framer,
            &sessions,
            b"2 tests, 0 benchmarks\n",
            EXECUTABLE,
            filter,
            &mut context,
        );
        assert!(matches!(
            summary,
            Poll::Ready(Ok((ListingSourceStep::Ignored, 22)))
        ));
    }
}
