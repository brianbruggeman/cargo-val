#![cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]

use core::cell::Cell;
use core::slice;
use core::task::Poll;
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicUsize, Ordering};

use cargo_val::artifact_stream::{ArtifactFramer, ArtifactStreamError, RecordFault, RecordStep};
use cargo_val::{ArtifactRecord, ExecutablePath, ParseError, parse_artifact};
use proxima_codec::FrameError;

const FIRST_PART: &[u8] = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/first\"";
const FIRST_END_SECOND_PART: &[u8] = b"}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/second\"";
const SECOND_END: &[u8] = b"}\n";

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
}

fn tracking_enabled() -> bool {
    TRACKING.try_with(Cell::get).unwrap_or(false)
}

fn set_tracking(enabled: bool) {
    TRACKING.with(|tracking| tracking.set(enabled));
}

static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);

struct AllocationCounter;

#[global_allocator]
static GLOBAL_ALLOCATOR: AllocationCounter = AllocationCounter;

unsafe impl GlobalAlloc for AllocationCounter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if tracking_enabled() {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if tracking_enabled() {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[derive(Clone, Copy)]
enum ProducerState {
    First,
    Second,
    AwaitingAcknowledgement,
    Third,
    AwaitingEof,
    Eof,
}

struct HeldOpenProducer {
    state: ProducerState,
}

impl HeldOpenProducer {
    fn poll_chunk(&mut self) -> Poll<Option<&'static [u8]>> {
        match self.state {
            ProducerState::First => {
                self.state = ProducerState::Second;
                Poll::Ready(Some(FIRST_PART))
            }
            ProducerState::Second => {
                self.state = ProducerState::AwaitingAcknowledgement;
                Poll::Ready(Some(FIRST_END_SECOND_PART))
            }
            ProducerState::AwaitingAcknowledgement => Poll::Pending,
            ProducerState::Third => {
                self.state = ProducerState::AwaitingEof;
                Poll::Ready(Some(SECOND_END))
            }
            ProducerState::AwaitingEof => {
                self.state = ProducerState::Eof;
                Poll::Ready(None)
            }
            ProducerState::Eof => Poll::Ready(None),
        }
    }

    fn acknowledge_first(&mut self) {
        assert!(matches!(self.state, ProducerState::AwaitingAcknowledgement));
        self.state = ProducerState::Third;
    }

    fn is_open(&self) -> bool {
        !matches!(self.state, ProducerState::Eof)
    }
}

fn select<'call>(
    record: &'call [u8],
    scratch: &'call mut [u8],
    max_record_bytes: usize,
) -> Result<Option<&'call str>, ParseError> {
    Ok(
        match parse_artifact(record, scratch, max_record_bytes, 4)? {
            ArtifactRecord::Ignored => None,
            ArtifactRecord::TestExecutable(
                ExecutablePath::Borrowed(path) | ExecutablePath::Scratch(path),
            ) => Some(path),
        },
    )
}

fn next_selection<'call>(
    framer: &'call mut ArtifactFramer<'_>,
    chunk: &'call [u8],
    scratch: &'call mut [u8],
) -> Result<RecordStep<Option<&'call str>>, RecordFault<ParseError>> {
    let max_record_bytes = framer.max_record_bytes();
    framer.next_record(chunk, |record| select(record, scratch, max_record_bytes))
}

#[proxima::test]
async fn two_artifacts_emit_before_source_eof() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    set_tracking(false);
    let mut buffer = [0; 256];
    let mut scratch = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 255).expect("buffer capacity");
    let mut producer = HeldOpenProducer {
        state: ProducerState::First,
    };
    let first_chunk = match producer.poll_chunk() {
        Poll::Ready(Some(chunk)) => chunk,
        other => panic!("producer did not yield the first chunk: {other:?}"),
    };
    {
        set_tracking(true);
        let first_step = next_selection(&mut framer, first_chunk, &mut scratch);
        set_tracking(false);
        assert_eq!(
            first_step.expect("first chunk stages"),
            RecordStep::NeedMore {
                consumed_bytes: first_chunk.len()
            }
        );
    }

    let second_chunk = match producer.poll_chunk() {
        Poll::Ready(Some(chunk)) => chunk,
        other => panic!("producer did not yield the completing chunk: {other:?}"),
    };
    {
        set_tracking(true);
        let second_step = next_selection(&mut framer, second_chunk, &mut scratch);
        set_tracking(false);
        let RecordStep::Parsed {
            parsed,
            consumed_bytes,
        } = second_step.expect("second chunk completes the first record")
        else {
            panic!("second chunk produced no record");
        };
        assert_eq!(consumed_bytes, second_chunk.len());
        assert!(producer.is_open());
        assert_eq!(
            parsed.expect("first artifact emitted while producer is open"),
            "/fixture/first"
        );
        assert!(matches!(producer.poll_chunk(), Poll::Pending));
    }

    producer.acknowledge_first();
    let third_chunk = match producer.poll_chunk() {
        Poll::Ready(Some(chunk)) => chunk,
        other => panic!("producer did not resume after acknowledgement: {other:?}"),
    };
    {
        set_tracking(true);
        let third_step = next_selection(&mut framer, third_chunk, &mut scratch);
        set_tracking(false);
        let RecordStep::Parsed {
            parsed,
            consumed_bytes,
        } = third_step.expect("third chunk completes the second record")
        else {
            panic!("third chunk produced no record");
        };
        assert_eq!(consumed_bytes, third_chunk.len());
        assert_eq!(parsed.expect("second artifact emitted"), "/fixture/second");
    }
    assert_eq!(producer.poll_chunk(), Poll::Ready(None));
    assert_eq!(framer.finish(), Ok(()));
    assert_eq!(
        framer.push(b"late bytes"),
        Err(ArtifactStreamError::InputAfterEof)
    );
    {
        let after_eof = next_selection(&mut framer, b"", &mut scratch);
        assert_eq!(
            after_eof.expect("empty chunk after EOF is idle"),
            RecordStep::NeedMore { consumed_bytes: 0 }
        );
    }
    set_tracking(false);
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 0);

    set_tracking(true);
    let allocation_control = Vec::<u8>::with_capacity(1);
    set_tracking(false);
    let positive_control_allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    assert_eq!(positive_control_allocations, 1);
    drop(allocation_control);
    println!(
        "C3_ALLOCATION_TRACE os={} records=2 path_allocations=0 positive_control_allocations={positive_control_allocations}",
        env::consts::OS
    );
}

#[proxima::test]
async fn record_without_newline_is_rejected_at_eof() {
    let mut buffer = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 63).expect("buffer capacity");
    let partial_record = b"{\"reason\":\"compiler-artifact\"";
    let mut scratch = [0; 64];
    assert_eq!(
        next_selection(&mut framer, partial_record, &mut scratch),
        Ok(RecordStep::NeedMore {
            consumed_bytes: partial_record.len()
        })
    );
    assert_eq!(
        framer.finish(),
        Err(ArtifactStreamError::UnterminatedRecord {
            length: partial_record.len()
        })
    );
    assert_eq!(
        framer.push(b"late bytes"),
        Err(ArtifactStreamError::InputAfterEof)
    );
    let mut complete_buffer = [0; 64];
    let mut complete_framer =
        ArtifactFramer::new(&mut complete_buffer, 63).expect("complete frame buffer");
    let complete_record = b"{\"reason\":\"build-finished\",\"success\":true}\n";
    assert_eq!(
        complete_framer
            .push(complete_record)
            .expect("complete record input"),
        complete_record.len()
    );
    assert_eq!(
        complete_framer.finish(),
        Err(ArtifactStreamError::UndrainedFrame {
            length: complete_record.len()
        })
    );
    assert_eq!(
        next_selection(&mut complete_framer, b"late bytes", &mut scratch),
        Err(RecordFault::Framing {
            consumed_bytes: 0,
            error: ArtifactStreamError::InputAfterEof
        })
    );
}

#[proxima::test]
async fn parse_failure_poisoning_prevents_clean_eof_after_discard() {
    let mut buffer = [0; 8];
    let mut scratch = [0; 8];
    let mut framer = ArtifactFramer::new(&mut buffer, 7).expect("buffer capacity");
    assert_eq!(
        next_selection(&mut framer, b"{\n", &mut scratch),
        Err(RecordFault::Parse {
            consumed_bytes: 2,
            error: ParseError::InvalidJson
        })
    );
    assert_eq!(framer.finish(), Err(ArtifactStreamError::Poisoned));
    assert_eq!(framer.push(b"\n"), Err(ArtifactStreamError::Poisoned));
}

#[proxima::test]
async fn fixed_buffer_applies_backpressure_to_an_oversized_record() {
    let mut buffer = [0; 4];
    let mut framer = ArtifactFramer::new(&mut buffer, 3).expect("buffer capacity");
    let mut scratch = [0; 8];
    assert_eq!(
        next_selection(&mut framer, b"123", &mut scratch),
        Ok(RecordStep::NeedMore { consumed_bytes: 3 })
    );
    assert_eq!(
        next_selection(&mut framer, b"45", &mut scratch),
        Err(RecordFault::Framing {
            consumed_bytes: 1,
            error: ArtifactStreamError::Framing(FrameError::FrameTooLarge { len: 4 })
        })
    );
}

#[proxima::test]
async fn next_record_reports_consumed_ignored_records_before_draining_the_next_record() {
    let ignored_then_test = b"{\"reason\":\"build-finished\",\"success\":true}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/next\"}\n";
    let first_record_length = newline_end(ignored_then_test);
    let mut buffer = [0; 256];
    let mut scratch = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 255).expect("buffer capacity");
    let first_step = next_selection(&mut framer, ignored_then_test, &mut scratch)
        .expect("ignored record frames");
    assert_eq!(
        first_step,
        RecordStep::Parsed {
            parsed: None,
            consumed_bytes: first_record_length
        }
    );

    let second_step = next_selection(
        &mut framer,
        &ignored_then_test[first_record_length..],
        &mut scratch,
    )
    .expect("test record frames");
    let RecordStep::Parsed { parsed, .. } = second_step else {
        panic!("test record produced no parsed step");
    };
    assert_eq!(parsed.expect("test artifact selected"), "/fixture/next");
}

#[proxima::test]
async fn next_record_replays_unconsumed_chunk_tail_before_selecting_the_next_record() {
    let records = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/a\"}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/b\"}\n";
    let first_record_length = newline_end(records);
    let mut buffer = [0; 128];
    let mut scratch = [0; 32];
    let mut framer = ArtifactFramer::new(&mut buffer, 127).expect("buffer capacity");
    let first_step =
        next_selection(&mut framer, records, &mut scratch).expect("first record frames");
    assert_eq!(
        first_step,
        RecordStep::Parsed {
            parsed: Some("/fixture/a"),
            consumed_bytes: first_record_length
        }
    );
    let second_step = next_selection(&mut framer, &records[first_record_length..], &mut scratch)
        .expect("replayed tail frames");
    assert_eq!(
        second_step,
        RecordStep::Parsed {
            parsed: Some("/fixture/b"),
            consumed_bytes: records.len() - first_record_length
        }
    );
}

#[proxima::test]
async fn one_byte_chunks_preserve_incremental_delimiter_state() {
    let record = b"record split across one-byte chunks\n";
    let mut buffer = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 63).expect("buffer capacity");
    for (index, byte) in record.iter().enumerate() {
        let step = framer
            .next_record(slice::from_ref(byte), |frame| Ok::<_, ()>(frame.len()))
            .expect("byte input");
        if index + 1 == record.len() {
            assert_eq!(
                step,
                RecordStep::Parsed {
                    parsed: record.len() - 1,
                    consumed_bytes: 1
                }
            );
        } else {
            assert_eq!(step, RecordStep::NeedMore { consumed_bytes: 1 });
        }
    }
}
fn newline_end(records: &[u8]) -> usize {
    records
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("fixture record ends with a newline")
        + 1
}
