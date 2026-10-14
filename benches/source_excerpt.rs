use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cargo_val::source_excerpt::{Excerpt, read_source};

const ROUNDS: u32 = 5;
const FILES: [(&str, &[u8], &[u8]); 2] = [
    ("tests/shard_process.rs (600 lines)", b"tests/shard_process.rs", b"let full = format!("),
    ("tests/sched_records.rs (92 lines)", b"tests/sched_records.rs", b"let garbage = Path::new("),
];

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

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

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

fn line_of(source: &[u8], needle: &[u8]) -> u32 {
    let at = memchr::memmem::find(source, needle).expect("the needle is in the real file");
    1 + u32::try_from(memchr::memchr_iter(b'\n', &source[..at]).count()).expect("line count fits")
}

fn once(root: &Path, file: &[u8], needle: &[u8], colour: bool) -> usize {
    let source = read_source(root, file).expect("real file reads");
    let line = line_of(&source, needle);
    let mut out = Vec::new();
    let excerpt = Excerpt::new(&source, line, 5).expect("line is inside the file");
    let fallback = excerpt.write(&mut out, b"here", colour);
    assert_eq!(fallback, None);
    black_box(out.len())
}

fn measure(label: &str, iterations: u32, mut body: impl FnMut() -> usize) {
    let mut samples = [0.0_f64; ROUNDS as usize];
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut bytes = 0;
    for sample in &mut samples {
        let started = Instant::now();
        for _ in 0..iterations {
            bytes = body();
        }
        *sample = started.elapsed().as_secs_f64() * 1e9 / f64::from(iterations);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    let mean = samples.iter().sum::<f64>() / f64::from(ROUNDS);
    let variance = samples.iter().map(|sample| (sample - mean).powi(2)).sum::<f64>() / f64::from(ROUNDS);
    let per_use = f64::from(u32::try_from(allocations).unwrap_or(u32::MAX)) / f64::from(ROUNDS * iterations);
    println!("{label:<62} {bytes:>6} {mean:>12.0} {:>7.2} {per_use:>10.1}", variance.sqrt() / mean * 100.0);
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    println!("{:<62} {:>6} {:>12} {:>7} {:>10}", "arm (read + excerpt + write, cold path)", "bytes", "ns/use", "CoV %", "allocs/use");
    for (name, file, needle) in FILES {
        measure(&format!("plain {name}"), 2_000, || once(root, file, needle, false));
        #[cfg(feature = "highlight")]
        {
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let started = Instant::now();
            once(root, file, needle, true);
            println!("highlight FIRST use {name}: {} ns, {} allocs", started.elapsed().as_nanos(), ALLOCATIONS.load(Ordering::Relaxed) - before);
            measure(&format!("highlight steady {name}"), 200, || once(root, file, needle, true));
        }
    }
}
