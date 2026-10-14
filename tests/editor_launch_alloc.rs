#![cfg(unix)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use cargo_val::editor_launch::{editor_argv, editor_command, spawn};

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

fn counted<Out>(work: impl FnOnce() -> Out) -> (Out, usize) {
    ALLOCATIONS.with(|count| count.set(0));
    COUNTING.with(|flag| flag.set(true));
    let out = work();
    COUNTING.with(|flag| flag.set(false));
    (out, ALLOCATIONS.with(Cell::get))
}

const EXPECTED_ALLOCATIONS: usize = 12;

#[test]
fn one_editor_launch_allocates_a_fixed_cold_path_count() {
    let (launched, allocations) = counted(|| {
        let command = editor_command(None, Some("true"));
        let argv = editor_argv(&command, b"/w/src/lib.rs", 28, 13);
        spawn(&argv, &[], b"/")
    });

    let (group, _exit) = launched.expect("the stand-in editor starts");
    println!("allocations for one launch (command, argv, spawn, exit source): {allocations}");
    assert!(group != 0, "a spawned editor leads a group");
    assert_eq!(allocations, EXPECTED_ALLOCATIONS, "the cold launch path allocates a fixed count");
}
