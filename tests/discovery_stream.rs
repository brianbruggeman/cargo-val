#![cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]

use core::slice;
use core::task::Poll;

use cargo_val::artifact_stream::{ArtifactFramer, ArtifactStreamError, RecordFault, RecordStep};
use cargo_val::discover_listing::{ListingError, ListingLine, parse_listing_line};

type Discovery<'call> = Result<RecordStep<ListingLine<'call>>, RecordFault<ListingError>>;

fn discover<'call>(framer: &'call mut ArtifactFramer<'_>, chunk: &'call [u8]) -> Discovery<'call> {
    framer.next_record(chunk, parse_listing_line)
}

fn parsed(step: Discovery<'_>) -> (ListingLine<'_>, usize) {
    match step {
        Ok(RecordStep::Parsed {
            parsed,
            consumed_bytes,
        }) => (parsed, consumed_bytes),
        other => panic!("listing line was not parsed: {other:?}"),
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
    fn dispatch(&mut self, line: ListingLine<'_>) {
        assert!(matches!(line, ListingLine::Test(_)));
        self.dispatched_names += 1;
    }
}

#[proxima::test]
async fn dispatches_first_name_before_listing_eof() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let mut source = GatedListingSource { state: 0 };
    let mut dispatcher = CountingDispatcher::default();
    let mut observed_names = 0;

    let Poll::Ready(Some(first_chunk)) = source.poll_next_chunk(dispatcher.dispatched_names) else {
        panic!("first listing chunk was not ready")
    };
    let (first_line, first_consumed) = parsed(discover(&mut framer, first_chunk));
    assert_eq!(first_consumed, first_chunk.len());
    assert_eq!(first_line, ListingLine::Test("alpha"));
    observed_names += 1;
    assert!(matches!(
        source.poll_next_chunk(dispatcher.dispatched_names),
        Poll::Pending
    ));
    dispatcher.dispatch(first_line);

    for expected_name in ["beta", "gamma", "delta"] {
        let Poll::Ready(Some(next_chunk)) = source.poll_next_chunk(dispatcher.dispatched_names)
        else {
            panic!("gated listing chunk was not released after dispatch")
        };
        let (next_line, _) = parsed(discover(&mut framer, next_chunk));
        assert_eq!(next_line, ListingLine::Test(expected_name));
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

#[proxima::test]
async fn ignores_summary_without_stalling_later_name() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let summary = b"1 test, 0 benchmarks\n";
    let (first, first_consumed) = parsed(discover(&mut framer, summary));
    assert_eq!(first, ListingLine::Ignored);
    assert_eq!(first_consumed, summary.len());

    let (second, second_consumed) = parsed(discover(&mut framer, b"alpha: test\n"));
    assert_eq!(second, ListingLine::Test("alpha"));
    assert_eq!(second_consumed, 12);
}

#[proxima::test]
async fn reports_framing_failure_without_emitting_name() {
    let mut buffer = [0; 4];
    let mut framer = ArtifactFramer::new(&mut buffer, 3).expect("small fixed listing capacity");
    let result = discover(&mut framer, b"longer: test\n");
    assert!(matches!(
        result,
        Err(RecordFault::Framing {
            error: ArtifactStreamError::Framing(proxima_codec::FrameError::FrameTooLarge { .. }),
            consumed_bytes: 0,
        })
    ));
}

#[proxima::test]
async fn withholds_split_line_until_newline_arrives() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let line = b"alpha: test\n";
    for (index, byte) in line.iter().enumerate() {
        let step = discover(&mut framer, slice::from_ref(byte)).expect("split input");
        if index + 1 == line.len() {
            assert_eq!(
                step,
                RecordStep::Parsed {
                    parsed: ListingLine::Test("alpha"),
                    consumed_bytes: 1
                }
            );
        } else {
            assert_eq!(step, RecordStep::NeedMore { consumed_bytes: 1 });
        }
    }
}

#[proxima::test]
async fn drains_buffered_line_before_accepting_new_chunk() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let fragment = b"al";
    assert_eq!(
        discover(&mut framer, fragment),
        Ok(RecordStep::NeedMore { consumed_bytes: 2 })
    );
    let rest = b"pha: test\nbeta: test\n";
    let (first, first_consumed) = parsed(discover(&mut framer, rest));
    assert_eq!(first, ListingLine::Test("alpha"));
    assert_eq!(first_consumed, rest.len());

    let replay_chunk = b"gamma: test\n";
    let (second, second_consumed) = parsed(discover(&mut framer, replay_chunk));
    assert_eq!(second, ListingLine::Test("beta"));
    assert_eq!(second_consumed, 0);

    let (third, third_consumed) = parsed(discover(&mut framer, replay_chunk));
    assert_eq!(third, ListingLine::Test("gamma"));
    assert_eq!(third_consumed, replay_chunk.len());
    assert_eq!(framer.finish(), Ok(()));
}

#[proxima::test]
async fn lent_chunk_consumes_one_line_and_leaves_the_tail_with_the_caller() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let chunk = b"alpha: test\nbeta: test\n";
    let (first, first_consumed) = parsed(discover(&mut framer, chunk));
    assert_eq!(first, ListingLine::Test("alpha"));
    assert_eq!(first_consumed, 12);

    let tail = &chunk[first_consumed..];
    let (second, second_consumed) = parsed(discover(&mut framer, tail));
    assert_eq!(second, ListingLine::Test("beta"));
    assert_eq!(second_consumed, tail.len());
    assert_eq!(framer.finish(), Ok(()));
}

#[proxima::test]
async fn replays_short_copy_tail_before_reading_another_chunk() {
    let mut buffer = [0; 12];
    let mut framer = ArtifactFramer::new(&mut buffer, 11).expect("fixed line capacity");
    let chunk = b"alpha: test\nbeta: test\n";
    let (first, first_consumed) = parsed(discover(&mut framer, chunk));
    assert_eq!(first, ListingLine::Test("alpha"));
    assert_eq!(first_consumed, 12);
    assert!(first_consumed < chunk.len());

    let tail = &chunk[first_consumed..];
    let (second, second_consumed) = parsed(discover(&mut framer, tail));
    assert_eq!(second, ListingLine::Test("beta"));
    assert_eq!(second_consumed, tail.len());
    assert_eq!(framer.finish(), Ok(()));
}

#[proxima::test]
async fn parser_failure_reports_progress_and_poisons_stream() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    let malformed_chunk = b"\xff: test\nok: test\n";
    let result = discover(&mut framer, malformed_chunk);
    assert!(matches!(
        result,
        Err(RecordFault::Parse {
            consumed_bytes,
            error: ListingError::InvalidUtf8,
        }) if consumed_bytes == b"\xff: test\n".len()
    ));
    assert!(matches!(
        discover(&mut framer, b""),
        Err(RecordFault::Framing {
            consumed_bytes: 0,
            error: ArtifactStreamError::Poisoned,
        })
    ));
}

#[proxima::test]
async fn empty_poll_after_eof_returns_no_line() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    assert_eq!(framer.finish(), Ok(()));
    assert_eq!(
        discover(&mut framer, b""),
        Ok(RecordStep::NeedMore { consumed_bytes: 0 })
    );
}

#[proxima::test]
async fn nonempty_input_after_eof_is_rejected_even_with_buffered_line() {
    let mut buffer = [0; 33];
    let mut framer = ArtifactFramer::new(&mut buffer, 32).expect("fixed listing capacity");
    assert_eq!(
        discover(&mut framer, b"al"),
        Ok(RecordStep::NeedMore { consumed_bytes: 2 })
    );
    let (first, _) = parsed(discover(&mut framer, b"pha: test\nbeta: test\n"));
    assert_eq!(first, ListingLine::Test("alpha"));
    assert!(matches!(
        framer.finish(),
        Err(ArtifactStreamError::UndrainedFrame { .. })
    ));
    assert!(matches!(
        discover(&mut framer, b"gamma: test\n"),
        Err(RecordFault::Framing {
            error: ArtifactStreamError::InputAfterEof,
            consumed_bytes: 0,
        })
    ));
}
