mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/failure_fixture.rs"]
mod failure_fixture;
#[path = "common/failure_fixture_async.rs"]
mod failure_fixture_async;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use cargo_val::failure_class::{Class, Exit, classify};
use failure_fixture_async::{build, run};

const PARSES: usize = 100_000;

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record() {
    if COUNTING.try_with(Cell::get).ok() == Some(true) {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[proxima::test(timeout = "310s")]
async fn a_hundred_thousand_parses_of_real_captures_allocate_nothing() {
    let executable = build("failure_class").await;
    let names = ["assert_eq_mismatch", "plain_panic", "registry_panic_under_user_call"];
    let mut runs = Vec::with_capacity(names.len());
    for name in names {
        runs.push(run(&executable, &format!("failing::{name}"), &[]).await);
    }
    assert!(runs.iter().all(|captured| captured.exit != Exit::Code(0)), "fixture tests must fail");
    let captures: Vec<Vec<u8>> = runs.into_iter().map(|captured| captured.stdout).collect();

    COUNTING.with(|flag| flag.set(true));
    ALLOCATIONS.with(|count| count.set(0));
    let mut classified = 0_usize;
    for index in 0..PARSES {
        let capture = &captures[index % captures.len()];
        let record = classify(capture, Exit::Code(101), 0).expect("fixture captures classify");
        classified += usize::from(matches!(record.class, Class::Assert | Class::Panic) && record.location.is_ok());
    }
    COUNTING.with(|flag| flag.set(false));

    assert_eq!(classified, PARSES, "every parse must have produced a located record");
    assert_eq!(ALLOCATIONS.with(Cell::get), 0, "allocations over {PARSES} parses");
}
