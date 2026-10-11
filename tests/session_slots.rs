use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::hint::black_box;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::System;

use cargo_val::schedule::SessionSlots;

struct CountingAllocator;

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

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
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

#[test]
fn fixed_slots_hold_and_reuse_child_session_values() {
    let mut sessions = SessionSlots::<&str, 2>::new();
    assert_eq!(sessions.capacity(), 2);
    assert!(sessions.is_empty());

    let alpha = sessions.insert("suite::alpha").expect("first slot is free");
    let beta = sessions.insert("suite::beta").expect("second slot is free");
    assert_eq!(sessions.len(), 2);
    assert!(sessions.is_full());
    assert_eq!(sessions.insert("suite::gamma"), Err("suite::gamma"));

    assert_eq!(sessions.get(alpha), Some(&"suite::alpha"));
    assert_eq!(sessions.remove(alpha), Some("suite::alpha"));
    assert_eq!(sessions.get(alpha), None);

    let gamma = sessions
        .insert("suite::gamma")
        .expect("retired slot is free");
    assert_eq!(gamma.index(), alpha.index());
    assert_ne!(gamma, alpha);
    assert_eq!(sessions.get(alpha), None);
    assert_eq!(sessions.remove(alpha), None);
    assert_eq!(sessions.get(beta), Some(&"suite::beta"));
    assert_eq!(sessions.get(gamma), Some(&"suite::gamma"));
    assert_eq!(sessions.remove(beta), Some("suite::beta"));
    assert_eq!(sessions.remove(gamma), Some("suite::gamma"));
    assert_eq!(sessions.remove(gamma), None);
    assert!(sessions.is_empty());
}

#[test]
fn zero_capacity_never_admits_a_session() {
    let mut sessions = SessionSlots::<u8, 0>::new();
    assert_eq!(sessions.insert(7), Err(7));
    assert!(sessions.is_full());
    assert_eq!(sessions.len(), 0);
}

#[test]
fn slot_operations_allocate_nothing_and_probe_counts_allocations() {
    let mut sessions = SessionSlots::<&str, 2>::new();
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    set_tracking(true);
    let alpha = sessions.insert("suite::alpha");
    let beta = sessions.insert("suite::beta");
    let alpha_present = alpha
        .as_ref()
        .is_ok_and(|slot| sessions.get(*slot).is_some());
    let alpha_removed = alpha.is_ok_and(|slot| sessions.remove(slot).is_some());
    let beta_present = beta.is_ok_and(|slot| sessions.get(slot).is_some());
    set_tracking(false);
    let slot_allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);

    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    set_tracking(true);
    let allocation_control = Vec::<u8>::with_capacity(1);
    set_tracking(false);
    let positive_control_allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    black_box(allocation_control);

    assert!(alpha_present);
    assert!(alpha_removed);
    assert!(beta_present);
    assert_eq!(slot_allocations, 0);
    assert_eq!(positive_control_allocations, 1);
    println!(
        "{{\"event\":\"slot_allocation_probe\",\"slot_allocations\":{slot_allocations},\"positive_control_allocations\":{positive_control_allocations}}}"
    );
}
