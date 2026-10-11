use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

struct CountingAllocator;

std::thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCATIONS.try_with(Cell::get).ok() == Some(true) {
            let _ = ALLOCATION_COUNT.try_with(|count| {
                count.set(count.get().saturating_add(1));
            });
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNT_ALLOCATIONS.try_with(Cell::get).ok() == Some(true) {
            let _ = ALLOCATION_COUNT.try_with(|count| {
                count.set(count.get().saturating_add(1));
            });
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

pub(crate) fn measure_allocations<T>(action: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATION_COUNT.with(|count| count.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    let output = action();
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    let allocations = ALLOCATION_COUNT.with(Cell::get);
    (output, allocations)
}
