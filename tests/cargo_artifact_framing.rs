#![cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]

use core::pin::pin;
use core::slice;
use core::task::{Context, Poll, Waker};
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cargo_val::artifact_stream::{
    ArtifactFramer, ArtifactInput, ArtifactPipe, ArtifactStreamError, PathStorage,
};
use proxima_codec::FrameError;
use proxima_primitives::pipe::Pipe;

const FIRST_PART: &[u8] = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/first\"";
const FIRST_END_SECOND_PART: &[u8] = b"}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/second\"";
const SECOND_END: &[u8] = b"}\n";

static TRACK_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);

struct AllocationCounter;

#[global_allocator]
static GLOBAL_ALLOCATOR: AllocationCounter = AllocationCounter;

unsafe impl GlobalAlloc for AllocationCounter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if TRACK_ALLOCATIONS.load(Ordering::Relaxed) {
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

#[test]
fn two_artifacts_emit_before_source_eof() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
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
        let first_pipe = ArtifactPipe::new();
        let first_input = ArtifactInput {
            framer: &mut framer,
            chunk: first_chunk,
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
        let first_future = first_pipe.call(first_input);
        let mut first_future = pin!(first_future);
        let mut first_context = Context::from_waker(Waker::noop());
        let first_output = match first_future.as_mut().poll(&mut first_context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("first chunk pipe did not complete: {other:?}"),
        };
        TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
        assert_eq!(first_output.consumed_bytes, first_chunk.len());
        assert!(!first_output.record_consumed);
        assert_eq!(first_output.selection, None);
    }

    let second_chunk = match producer.poll_chunk() {
        Poll::Ready(Some(chunk)) => chunk,
        other => panic!("producer did not yield the completing chunk: {other:?}"),
    };
    {
        let second_pipe = ArtifactPipe::new();
        let first_input = ArtifactInput {
            framer: &mut framer,
            chunk: second_chunk,
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
        let second_future = second_pipe.call(first_input);
        let mut second_future = pin!(second_future);
        let first_waker = Waker::noop();
        let mut first_context = Context::from_waker(first_waker);
        let second_output = match second_future.as_mut().poll(&mut first_context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("second chunk pipe did not complete: {other:?}"),
        };
        TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
        assert_eq!(second_output.consumed_bytes, second_chunk.len());
        assert!(producer.is_open());
        let first_selection = second_output
            .selection
            .expect("first artifact emitted while producer is open");
        assert_eq!(first_selection.storage, PathStorage::Record);
        assert_eq!(first_selection.path, "/fixture/first");
        assert!(matches!(producer.poll_chunk(), Poll::Pending));
    }

    producer.acknowledge_first();
    let third_chunk = match producer.poll_chunk() {
        Poll::Ready(Some(chunk)) => chunk,
        other => panic!("producer did not resume after acknowledgement: {other:?}"),
    };
    {
        let third_pipe = ArtifactPipe::new();
        let second_input = ArtifactInput {
            framer: &mut framer,
            chunk: third_chunk,
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
        let third_future = third_pipe.call(second_input);
        let mut third_future = pin!(third_future);
        let mut second_context = Context::from_waker(Waker::noop());
        let third_output = match third_future.as_mut().poll(&mut second_context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("third chunk pipe did not complete: {other:?}"),
        };
        TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
        assert_eq!(third_output.consumed_bytes, third_chunk.len());
        let second_selection = third_output.selection.expect("second artifact emitted");
        assert_eq!(second_selection.storage, PathStorage::Record);
        assert_eq!(second_selection.path, "/fixture/second");
    }
    assert_eq!(producer.poll_chunk(), Poll::Ready(None));
    assert_eq!(framer.finish(), Ok(()));
    assert_eq!(
        framer.push(b"late bytes"),
        Err(ArtifactStreamError::InputAfterEof)
    );
    {
        let pipe = ArtifactPipe::new();
        let input = ArtifactInput {
            framer: &mut framer,
            chunk: b"",
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        let future = pipe.call(input);
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        let output = match future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("empty pipe call after EOF failed: {other:?}"),
        };
        assert_eq!(output.consumed_bytes, 0);
        assert!(!output.record_consumed);
        assert_eq!(output.selection, None);
    }
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 0);

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let allocation_control = Vec::<u8>::with_capacity(1);
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    let positive_control_allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    assert_eq!(positive_control_allocations, 1);
    drop(allocation_control);
    println!(
        "C3_ALLOCATION_TRACE os={} records=2 path_allocations=0 positive_control_allocations={positive_control_allocations}",
        env::consts::OS
    );
}

#[test]
fn record_without_newline_is_rejected_at_eof() {
    let mut buffer = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 63).expect("buffer capacity");
    let partial_record = b"{\"reason\":\"compiler-artifact\"";
    assert_eq!(
        framer.push(partial_record).expect("partial record"),
        partial_record.len()
    );
    assert_eq!(framer.next_frame().expect("partial is not an error"), None);
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
    let mut scratch = [0; 64];
    let pipe = ArtifactPipe::new();
    let input = ArtifactInput {
        framer: &mut complete_framer,
        chunk: b"late bytes",
        scratch: &mut scratch,
        max_json_depth: 4,
    };
    let future = pipe.call(input);
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(Err(ArtifactStreamError::InputAfterEof))
    ));
}

#[test]
fn parse_failure_poisoning_prevents_clean_eof_after_discard() {
    let mut buffer = [0; 8];
    let mut scratch = [0; 8];
    let mut framer = ArtifactFramer::new(&mut buffer, 7).expect("buffer capacity");
    {
        let pipe = ArtifactPipe::new();
        let input = ArtifactInput {
            framer: &mut framer,
            chunk: b"{\n",
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        let future = pipe.call(input);
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(Err(ArtifactStreamError::Parsing(
                cargo_val::ParseError::InvalidJson
            )))
        ));
    }
    assert_eq!(framer.finish(), Err(ArtifactStreamError::Poisoned));
    assert_eq!(framer.push(b"\n"), Err(ArtifactStreamError::Poisoned));
}

#[test]
fn fixed_buffer_applies_backpressure_to_an_oversized_record() {
    let mut buffer = [0; 4];
    let mut framer = ArtifactFramer::new(&mut buffer, 3).expect("buffer capacity");
    assert_eq!(framer.push(b"12345").expect("first partial chunk"), 4);
    assert_eq!(framer.push(b"12345").expect("backpressure"), 0);
    assert_eq!(
        framer.next_frame(),
        Err(ArtifactStreamError::Framing(FrameError::FrameTooLarge {
            len: 4
        }))
    );
}

#[test]
fn pipe_reports_consumed_ignored_records_before_draining_the_next_record() {
    let ignored_then_test = b"{\"reason\":\"build-finished\",\"success\":true}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/next\"}\n";
    let mut buffer = [0; 256];
    let mut scratch = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 255).expect("buffer capacity");
    let mut context = Context::from_waker(Waker::noop());
    {
        let first_pipe = ArtifactPipe::new();
        let first_input = ArtifactInput {
            framer: &mut framer,
            chunk: ignored_then_test,
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        let first_future = first_pipe.call(first_input);
        let mut first_future = pin!(first_future);
        let first_output = match first_future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("ignored record pipe call failed: {other:?}"),
        };
        assert!(first_output.record_consumed);
        assert_eq!(first_output.selection, None);
        assert_eq!(first_output.consumed_bytes, ignored_then_test.len());
    };

    {
        let second_pipe = ArtifactPipe::new();
        let second_input = ArtifactInput {
            framer: &mut framer,
            chunk: b"",
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        let second_future = second_pipe.call(second_input);
        let mut second_future = pin!(second_future);
        let second_output = match second_future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("buffered test record pipe call failed: {other:?}"),
        };
        assert!(second_output.record_consumed);
        assert_eq!(
            second_output
                .selection
                .expect("test artifact selected")
                .path,
            "/fixture/next"
        );
    }
}

#[test]
fn pipe_replays_unconsumed_chunk_tail_before_selecting_the_next_record() {
    let records = b"{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/a\"}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"test\":true},\"profile\":{\"test\":true},\"executable\":\"/fixture/b\"}\n";
    let mut buffer = [0; 128];
    let mut scratch = [0; 32];
    let mut framer = ArtifactFramer::new(&mut buffer, 127).expect("buffer capacity");
    let mut context = Context::from_waker(Waker::noop());
    let consumed_bytes = {
        let pipe = ArtifactPipe::new();
        let input = ArtifactInput {
            framer: &mut framer,
            chunk: records,
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        let future = pipe.call(input);
        let mut future = pin!(future);
        let output = match future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("first replay pipe call failed: {other:?}"),
        };
        assert!(output.record_consumed);
        assert_eq!(output.selection.expect("first artifact").path, "/fixture/a");
        assert_eq!(output.consumed_bytes, 128);
        output.consumed_bytes
    };
    {
        let pipe = ArtifactPipe::new();
        let input = ArtifactInput {
            framer: &mut framer,
            chunk: &records[consumed_bytes..],
            scratch: &mut scratch,
            max_json_depth: 4,
        };
        let future = pipe.call(input);
        let mut future = pin!(future);
        let output = match future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(output)) => output,
            other => panic!("replayed tail pipe call failed: {other:?}"),
        };
        assert!(output.record_consumed);
        assert_eq!(
            output.selection.expect("second artifact").path,
            "/fixture/b"
        );
        assert_eq!(output.consumed_bytes, records.len() - consumed_bytes);
    }
}

#[test]
fn one_byte_chunks_preserve_incremental_delimiter_state() {
    let record = b"record split across one-byte chunks\n";
    let mut buffer = [0; 64];
    let mut framer = ArtifactFramer::new(&mut buffer, 63).expect("buffer capacity");
    for (index, byte) in record.iter().enumerate() {
        assert_eq!(framer.push(slice::from_ref(byte)).expect("byte input"), 1);
        let frame = framer.next_frame().expect("frame scan");
        if index + 1 == record.len() {
            assert_eq!(frame, Some(&record[..record.len() - 1]));
        } else {
            assert_eq!(frame, None);
        }
    }
}
