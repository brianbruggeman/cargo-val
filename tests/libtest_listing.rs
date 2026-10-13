#![cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::ptr;
use std::alloc::System;
use std::env;
use std::sync::atomic::{AtomicUsize, Ordering};

use cargo_val::artifact_stream::{ArtifactFramer, ArtifactStreamError, RecordFault, RecordStep};
use cargo_val::discover_listing::{ListingError, ListingLine, parse_listing_line};
use proxima_codec::FrameError;

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

#[proxima::test]
async fn parse_one_listing_line() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    set_tracking(true);
    let name_input = b"suite::alpha: test";
    let name = parse_listing_line(name_input);
    set_tracking(false);
    assert_eq!(name, Ok(ListingLine::Test("suite::alpha")));
    let Ok(ListingLine::Test(name)) = name else {
        panic!("test name was not returned");
    };
    assert!(ptr::eq(name.as_ptr(), name_input.as_ptr()));

    set_tracking(true);
    let summary = parse_listing_line(b"1 test, 0 benchmarks");
    set_tracking(false);
    assert_eq!(summary, Ok(ListingLine::Ignored));

    set_tracking(true);
    let mutated_suffix = parse_listing_line(b"suite::alpha: benchmark");
    set_tracking(false);
    assert_eq!(mutated_suffix, Ok(ListingLine::Ignored));

    set_tracking(true);
    let invalid_utf8 = parse_listing_line(b"suite::alpha\xff: test");
    set_tracking(false);
    assert_eq!(invalid_utf8, Err(ListingError::InvalidUtf8));

    set_tracking(true);
    let empty_name = parse_listing_line(b": test");
    set_tracking(false);
    assert_eq!(empty_name, Ok(ListingLine::Ignored));

    set_tracking(true);
    let crlf_name = parse_listing_line(b"suite::alpha: test\r");
    set_tracking(false);
    assert_eq!(crlf_name, Ok(ListingLine::Test("suite::alpha")));
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 0);

    set_tracking(true);
    let allocation_control = Vec::<u8>::with_capacity(1);
    set_tracking(false);
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 1);
    drop(allocation_control);

    println!(
        "C4_ALLOCATION_TRACE os={} parser_calls=6 parser_allocations=0 positive_control_allocations=1",
        env::consts::OS
    );
}

#[proxima::test]
async fn framer_rejects_a_listing_line_over_the_record_limit() {
    let mut buffer = [0; 9];
    let mut framer =
        ArtifactFramer::new(&mut buffer, 8).expect("storage holds a record and newline");
    let oversized_line = b"suite::alpha: test\n";
    let direct = framer.next_record(oversized_line, parse_listing_line);
    assert_eq!(
        direct,
        Err(RecordFault::Framing {
            consumed_bytes: 0,
            error: ArtifactStreamError::Framing(FrameError::FrameTooLarge { len: 18 })
        })
    );

    let mut staged_buffer = [0; 9];
    let mut staged_framer =
        ArtifactFramer::new(&mut staged_buffer, 8).expect("storage holds a record and newline");
    let staged = staged_framer.next_record(b"suite::a", parse_listing_line);
    assert_eq!(staged, Ok(RecordStep::NeedMore { consumed_bytes: 8 }));
    let overflow = staged_framer.next_record(b"lpha: test\n", parse_listing_line);
    assert!(matches!(
        overflow,
        Err(RecordFault::Framing {
            error: ArtifactStreamError::Framing(FrameError::FrameTooLarge { .. }),
            ..
        })
    ));
}

#[proxima::test]
async fn framer_rejects_a_listing_line_missing_its_newline_at_eof() {
    let mut buffer = [0; 33];
    let mut framer =
        ArtifactFramer::new(&mut buffer, 32).expect("storage holds a record and newline");
    let unterminated = b"suite::alpha: test";
    let staged = framer.next_record(unterminated, parse_listing_line);
    assert_eq!(
        staged,
        Ok(RecordStep::NeedMore {
            consumed_bytes: unterminated.len()
        })
    );
    assert_eq!(
        framer.finish(),
        Err(ArtifactStreamError::UnterminatedRecord {
            length: unterminated.len()
        })
    );
}
