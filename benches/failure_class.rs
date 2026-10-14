#[path = "../tests/common/group.rs"]
mod group;
#[path = "../tests/common/output.rs"]
mod output;
#[path = "../tests/common/failure_fixture.rs"]
mod failure_fixture;
#[path = "../tests/common/failure_fixture_sync.rs"]
mod failure_fixture_sync;

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use cargo_val::failure_class::{Exit, classify};
use failure_fixture::Captured;
use failure_fixture_sync::{build, run};

const ROUNDS: u32 = 5;
const PARSES_PER_ROUND: u32 = 20_000;
const LINE_BYTES: usize = 41;

struct CountingAllocator;

static ALLOCATIONS: AtomicU32 = AtomicU32::new(0);

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}

struct Arm {
    label: String,
    capture: Vec<u8>,
    exit: Exit,
}

fn capture_of(executable: &Path, test: &str, target_bytes: usize) -> Captured {
    let lines = (target_bytes / LINE_BYTES).to_string();
    run(executable, &format!("failing::{test}"), &[("CLASSIFY_NOISE_LINES", &lines)])
}

fn arm_of(label: String, captured: &Captured, keep: usize) -> Arm {
    let capture = captured.stdout[..keep.min(captured.stdout.len())].to_vec();
    Arm { label, capture, exit: captured.exit }
}

fn arms(executable: &Path) -> Vec<Arm> {
    let mut arms = vec![arm_of("16 B head of a real capture (rejected)".into(), &capture_of(executable, "assert_eq_mismatch", 0), 16)];
    for (name, size) in [("1 KB", 1024_usize), ("8 KB", 8 * 1024), ("64 KB", 64 * 1024)] {
        for test in ["assert_eq_mismatch", "registry_panic_under_user_call"] {
            let base = capture_of(executable, test, 0).stdout.len();
            let full = capture_of(executable, test, size.saturating_sub(base));
            arms.push(arm_of(format!("{name} {test}"), &full, size));
        }
    }
    arms
}

fn nanos_per_parse(arm: &Arm) -> f64 {
    let started = Instant::now();
    for _ in 0..PARSES_PER_ROUND {
        let _ = black_box(classify(black_box(&arm.capture), arm.exit, 0));
    }
    started.elapsed().as_secs_f64() * 1e9 / f64::from(PARSES_PER_ROUND)
}

fn main() {
    let executable = build("failure_class");
    println!("{:<52} {:>9} {:>12} {:>7} {:>13}", "arm", "bytes", "ns/parse", "CoV %", "allocs/parse");
    for arm in arms(&executable) {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let mut samples = [0.0_f64; ROUNDS as usize];
        for sample in &mut samples {
            *sample = nanos_per_parse(&arm);
        }
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        let mean = samples.iter().sum::<f64>() / f64::from(ROUNDS);
        let variance = samples.iter().map(|sample| (sample - mean).powi(2)).sum::<f64>() / f64::from(ROUNDS);
        let per_parse = f64::from(allocations) / f64::from(ROUNDS * PARSES_PER_ROUND);
        println!(
            "{:<52} {:>9} {:>12.1} {:>7.2} {:>13.3}",
            arm.label,
            arm.capture.len(),
            mean,
            variance.sqrt() / mean * 100.0,
            per_parse
        );
    }
}
