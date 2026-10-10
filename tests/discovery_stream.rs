#![cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]

use core::future::Future;
use core::pin::pin;
use core::slice;
use core::task::{Context, Poll, Waker};

use cargo_val::artifact_stream::{ArtifactFramer, ArtifactStreamError};
use cargo_val::discovery_stream::{
    ListingDiscoveryError, ListingDiscoveryErrorKind, ListingDiscoveryInput,
    ListingDiscoveryOutput, ListingDiscoveryPipe,
};
use cargo_val::libtest_listing::ListingLine;
use proxima_primitives::pipe::Pipe;

fn discover<'call, 'buffer>(
    framer: &'call mut ArtifactFramer<'buffer>,
    chunk: &'call [u8],
) -> Result<ListingDiscoveryOutput<'call>, ListingDiscoveryError> {
    discover_with_limit(framer, chunk, 32)
}

fn discover_with_limit<'call, 'buffer>(
    framer: &'call mut ArtifactFramer<'buffer>,
    chunk: &'call [u8],
    max_line_bytes: usize,
) -> Result<ListingDiscoveryOutput<'call>, ListingDiscoveryError> {
    let pipe = ListingDiscoveryPipe::new();
    let future = pipe.call(ListingDiscoveryInput {
        framer,
        chunk,
        max_line_bytes,
    });
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("discovery pipe unexpectedly yielded"),
    }
}

struct GatedListingSource {
    state: usize,
}

impl GatedListingSource {
    fn poll_next_chunk(&mut self, dispatched_names: usize) -> Poll<Option<&'static [u8]>> {
        match self.state {
            0 => {
                self.state = 1;
                Poll::Ready(Some(b"alpha: test\n"))
            }
            1 if dispatched_names == 0 => Poll::Pending,
            1 => {
                self.state = 2;
                Poll::Ready(Some(b"beta: test\n"))
            }
            2 => {
                self.state = 3;
                Poll::Ready(Some(b"gamma: test\n"))
            }
            3 => {
                self.state = 4;
                Poll::Ready(Some(b"delta: test\n"))
            }
            _ => Poll::Ready(None),
        }
    }
}

#[derive(Default)]
struct CountingDispatcher {
    dispatched_names: usize,
}

impl CountingDispatcher {
    fn dispatch(&mut self, line: Option<ListingLine<'_>>) {
        assert!(matches!(line, Some(ListingLine::Test(_))));
        self.dispatched_names += 1;
    }
}

#[test]
fn dispatches_first_name_before_listing_eof() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let mut source = GatedListingSource { state: 0 };
    let mut dispatcher = CountingDispatcher::default();
    let mut observed_names = 0;

    let first_chunk = match source.poll_next_chunk(dispatcher.dispatched_names) {
        Poll::Ready(Some(chunk)) => chunk,
        _ => panic!("first listing chunk was not ready"),
    };
    let first_line = {
        let first = discover(&mut framer, first_chunk).expect("first discovery");
        assert!(first.line_consumed);
        assert_eq!(first.line, Some(ListingLine::Test("alpha")));
        first.line
    };
    observed_names += 1;
    assert!(matches!(
        source.poll_next_chunk(dispatcher.dispatched_names),
        Poll::Pending
    ));
    dispatcher.dispatch(first_line);

    for expected_name in ["beta", "gamma", "delta"] {
        let next_chunk = match source.poll_next_chunk(dispatcher.dispatched_names) {
            Poll::Ready(Some(chunk)) => chunk,
            _ => panic!("gated listing chunk was not released after dispatch"),
        };
        let next_line = {
            let next = discover(&mut framer, next_chunk).expect("next discovery");
            assert_eq!(next.line, Some(ListingLine::Test(expected_name)));
            next.line
        };
        dispatcher.dispatch(next_line);
        observed_names += 1;
    }

    assert_eq!(dispatcher.dispatched_names, 4);
    assert_eq!(observed_names, 4);
    assert_eq!(
        source.poll_next_chunk(dispatcher.dispatched_names),
        Poll::Ready(None)
    );
    assert_eq!(framer.finish(), Ok(()));
}

#[test]
fn ignores_summary_without_stalling_later_name() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let first = discover(&mut framer, b"1 test, 0 benchmarks\n").expect("summary line");
    assert_eq!(first.line, Some(ListingLine::Ignored));
    assert!(first.line_consumed);

    let second = discover(&mut framer, b"alpha: test\n").expect("test line");
    assert_eq!(second.line, Some(ListingLine::Test("alpha")));
    assert!(second.line_consumed);
}

#[test]
fn reports_framing_failure_without_emitting_name() {
    let mut buffer = [0; 4];
    let mut framer = ArtifactFramer::new(&mut buffer, 3).expect("small fixed listing capacity");
    let result = discover(&mut framer, b"longer: test\n");
    assert!(matches!(
        result,
        Err(ListingDiscoveryError {
            kind: ListingDiscoveryErrorKind::Framing(ArtifactStreamError::Framing(
                proxima_codec::FrameError::FrameTooLarge { .. }
            )),
            consumed_bytes: 4,
        })
    ));
}

#[test]
fn withholds_split_line_until_newline_arrives() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let line = b"alpha: test\n";
    for (index, byte) in line.iter().enumerate() {
        let output = discover(&mut framer, slice::from_ref(byte)).expect("split input");
        if index + 1 == line.len() {
            assert_eq!(output.line, Some(ListingLine::Test("alpha")));
        } else {
            assert_eq!(output.line, None);
        }
    }
}

#[test]
fn drains_buffered_line_before_accepting_new_chunk() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let first_chunk = b"alpha: test\nbeta: test\n";
    let first = discover(&mut framer, first_chunk).expect("first buffered line");
    assert_eq!(first.line, Some(ListingLine::Test("alpha")));
    assert_eq!(first.consumed_bytes, first_chunk.len());

    let replay_chunk = b"gamma: test\n";
    let second = discover(&mut framer, replay_chunk).expect("second buffered line");
    assert_eq!(second.line, Some(ListingLine::Test("beta")));
    assert_eq!(second.consumed_bytes, 0);

    let third = discover(&mut framer, replay_chunk).expect("replayed tail");
    assert_eq!(third.line, Some(ListingLine::Test("gamma")));
    assert_eq!(third.consumed_bytes, replay_chunk.len());
    assert_eq!(framer.finish(), Ok(()));
}

#[test]
fn replays_short_copy_tail_before_reading_another_chunk() {
    let mut buffer = [0; 12];
    let mut framer = ArtifactFramer::new(&mut buffer, 11).expect("fixed line capacity");
    let chunk = b"alpha: test\nbeta: test\n";
    let first = discover_with_limit(&mut framer, chunk, 11).expect("first short-copy frame");
    assert_eq!(first.line, Some(ListingLine::Test("alpha")));
    assert_eq!(first.consumed_bytes, 12);
    assert!(first.consumed_bytes < chunk.len());

    let tail = &chunk[first.consumed_bytes..];
    let second = discover_with_limit(&mut framer, tail, 11).expect("replayed short-copy tail");
    assert_eq!(second.line, Some(ListingLine::Test("beta")));
    assert_eq!(second.consumed_bytes, tail.len());
    assert_eq!(framer.finish(), Ok(()));
}

#[test]
fn parser_failure_reports_progress_and_poisons_stream() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let malformed_chunk = b"\xff: test\nok: test\n";
    let result = discover(&mut framer, malformed_chunk);
    assert!(matches!(
        result,
        Err(ListingDiscoveryError {
            consumed_bytes,
            kind: ListingDiscoveryErrorKind::Listing(
                cargo_val::libtest_listing::ListingError::InvalidUtf8
            ),
        }) if consumed_bytes == malformed_chunk.len()
    ));
    assert!(matches!(
        discover(&mut framer, b""),
        Err(ListingDiscoveryError {
            consumed_bytes: 0,
            kind: ListingDiscoveryErrorKind::Framing(ArtifactStreamError::Poisoned),
        })
    ));
}

#[test]
fn empty_poll_after_eof_returns_no_line() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    assert_eq!(framer.finish(), Ok(()));
    let output = discover(&mut framer, b"").expect("empty eof poll");
    assert_eq!(output.line, None);
    assert_eq!(output.consumed_bytes, 0);
}

#[test]
fn nonempty_input_after_eof_is_rejected_even_with_buffered_line() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    framer.push(b"alpha: test\n").expect("buffer first line");
    assert!(matches!(
        framer.finish(),
        Err(ArtifactStreamError::UndrainedFrame { .. })
    ));
    assert!(matches!(
        discover(&mut framer, b"beta: test\n"),
        Err(ListingDiscoveryError {
            kind: ListingDiscoveryErrorKind::Framing(ArtifactStreamError::InputAfterEof),
            consumed_bytes: 0,
        })
    ));
}
