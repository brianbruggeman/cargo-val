#![cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]

use core::alloc::{GlobalAlloc, Layout};
use core::future::Future;
use core::pin::pin;
use core::ptr;
use core::task::{Context, Poll, Waker};
use std::alloc::System;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cargo_val::libtest_listing::{LibtestListingPipe, ListingError, ListingLine};
use proxima_codec::FrameError;
use proxima_primitives::pipe::Pipe;

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

fn parse_line<'line>(line: &'line [u8]) -> Result<ListingLine<'line>, ListingError> {
    parse_line_with_limit(line, line.len())
}

fn parse_line_with_limit<'line>(
    line: &'line [u8],
    max_line_bytes: usize,
) -> Result<ListingLine<'line>, ListingError> {
    let pipe = LibtestListingPipe::new(max_line_bytes);
    let future = pipe.call(line);
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("listing parser pipe unexpectedly yielded"),
    }
}

#[test]
fn parse_one_listing_line() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let name_input = b"suite::alpha: test\n";
    let name = parse_line(name_input);
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(name, Ok(ListingLine::Test("suite::alpha")));
    let Ok(ListingLine::Test(name)) = name else {
        panic!("test name was not returned");
    };
    assert!(ptr::eq(name.as_ptr(), name_input.as_ptr()));

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let summary = parse_line(b"1 test, 0 benchmarks\n");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(summary, Ok(ListingLine::Ignored));

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let mutated_suffix = parse_line(b"suite::alpha: benchmark\n");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(mutated_suffix, Ok(ListingLine::Ignored));

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let missing_newline = parse_line(b"suite::alpha: test");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(
        missing_newline,
        Err(ListingError::Framing(FrameError::Incomplete))
    );

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let trailing_bytes = parse_line(b"suite::alpha: test\nextra\n");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(trailing_bytes, Err(ListingError::TrailingBytes));

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let invalid_utf8 = parse_line(b"suite::alpha\xff: test\n");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(invalid_utf8, Err(ListingError::InvalidUtf8));

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let empty_name = parse_line(b": test\n");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(empty_name, Ok(ListingLine::Ignored));

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let over_limit = parse_line_with_limit(b"suite::alpha: test\n", 8);
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(
        over_limit,
        Err(ListingError::LineTooLarge {
            limit: 8,
            actual: 19
        })
    );

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let crlf_name = parse_line(b"suite::alpha: test\r\n");
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(crlf_name, Ok(ListingLine::Test("suite::alpha")));
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 0);

    TRACK_ALLOCATIONS.store(true, Ordering::Relaxed);
    let allocation_control = Vec::<u8>::with_capacity(1);
    TRACK_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 1);
    drop(allocation_control);

    println!(
        "C4_ALLOCATION_TRACE os={} parser_calls=9 parser_allocations=0 positive_control_allocations=1",
        env::consts::OS
    );
}
