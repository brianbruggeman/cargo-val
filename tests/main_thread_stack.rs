#![cfg(feature = "edge-os")]

use std::path::Path;

use cargo_val::run::drive;
use proxima_runtime::CoreId;

const WINDOWS_MAIN_THREAD_STACK: usize = 1024 * 1024;
const DRIVE_FUTURE_BOUND: usize = 16 * 1024;

fn fixture_arguments(target: &Path) -> Vec<Vec<u8>> {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cli_run/Cargo.toml");
    ["--manifest-path", manifest, "--target-dir", target.to_str().expect("target path is UTF-8")]
        .into_iter()
        .map(|argument| argument.as_bytes().to_vec())
        .collect()
}

#[test]
fn drive_completes_on_a_one_mebibyte_stack() {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("main-thread-stack");
    let arguments = fixture_arguments(&target);

    let summary = std::thread::Builder::new()
        .stack_size(WINDOWS_MAIN_THREAD_STACK)
        .spawn(move || {
            prime::os::core_shard::run_on_this_thread(CoreId(0), drive(arguments))
                .expect("prime root runs")
                .expect("fixture run completes")
        })
        .expect("thread spawns")
        .join()
        .expect("drive does not overflow or panic");

    let _ = std::fs::remove_dir_all(&target);
    assert_eq!((summary.passed, summary.failed), (2, 0));
}

#[test]
fn drive_future_stays_small() {
    let future = drive(Vec::new());

    let size = core::mem::size_of_val(&future);

    assert!(size < DRIVE_FUTURE_BOUND, "drive future is {size} bytes");
}
