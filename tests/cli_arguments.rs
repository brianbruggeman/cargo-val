use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::hint::black_box;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::System;
use std::vec::Vec;

use cargo_val::cli::{CliAction, CliParser};

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

#[proxima::test]
async fn help_and_version_select_top_level_actions() {
    let mut help_parser = CliParser::new();
    help_parser.push(b"--help");
    assert_eq!(help_parser.action(), CliAction::Help);

    let mut version_parser = CliParser::new();
    version_parser.push(b"-V");
    assert_eq!(version_parser.action(), CliAction::Version);
}

#[proxima::test]
async fn test_arguments_after_separator_are_not_cargo_val_options() {
    let mut parser = CliParser::new();
    parser.push(b"--manifest-path");
    parser.push(b"tests/fixtures/workspace/Cargo.toml");
    parser.push(b"--package");
    parser.push(b"cargo_val_fixture");
    parser.push(b"--test");
    parser.push(b"integration_fixture");
    parser.push(b"fixture::passes");
    parser.push(b"--exact");
    parser.push(b"--");
    parser.push(b"--help");
    parser.push(b"--version");
    assert_eq!(parser.action(), CliAction::Run);
    assert_eq!(
        parser.manifest_path(),
        Some(&b"tests/fixtures/workspace/Cargo.toml"[..])
    );
    assert_eq!(parser.filter(), Some(&b"fixture::passes"[..]));
    assert!(parser.exact());
    assert!(!parser.has_invalid_manifest_path());
    assert!(!parser.has_invalid_cargo_option_value());
}

#[proxima::test]
async fn missing_or_empty_manifest_path_is_rejected() {
    let mut missing_parser = CliParser::new();
    missing_parser.push(b"--manifest-path");
    assert!(missing_parser.has_invalid_manifest_path());

    let mut empty_parser = CliParser::new();
    empty_parser.push(b"--manifest-path=");
    assert!(empty_parser.has_invalid_manifest_path());

    let mut missing_target_parser = CliParser::new();
    missing_target_parser.push(b"--test");
    assert!(missing_target_parser.has_invalid_cargo_option_value());
}

#[proxima::test]
async fn argv_parsing_allocates_nothing_and_probe_detects_allocation() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    set_tracking(true);
    let mut parser = CliParser::new();
    parser.push(b"--manifest-path");
    parser.push(b"tests/fixtures/workspace/Cargo.toml");
    parser.push(b"fixture::passes");
    parser.push(b"--exact");
    let parsed = parser.manifest_path().is_some() && parser.filter().is_some() && parser.exact();
    set_tracking(false);
    let parser_allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);

    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    set_tracking(true);
    let allocation_control = Vec::<u8>::with_capacity(1);
    set_tracking(false);
    let positive_control_allocations = ALLOCATION_COUNT.load(Ordering::Relaxed);
    black_box(allocation_control);

    assert!(parsed);
    assert_eq!(parser_allocations, 0);
    assert_eq!(positive_control_allocations, 1);
    println!(
        "{{\"event\":\"cli_allocation_probe\",\"parser_allocations\":{parser_allocations},\"positive_control_allocations\":{positive_control_allocations}}}"
    );
}
