//! Drives `failure_class::classify` through every class on real captured output.
//!
//! Each case runs a failing fixture test (libtest, a registry crate, proxima-test) and feeds the
//! bytes it printed, plus how the process ended, to the classifier. `classify` is a transform:
//! `In = (capture, Exit, dropped)`, `Out = Result<Record, Reject>`; nothing here is wired to the
//! driver yet.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/block_on.rs"]
mod block_on;
#[path = "../tests/common/isolation.rs"]
mod isolation;
#[path = "../tests/common/failure_fixture.rs"]
mod failure_fixture;
#[path = "../tests/common/failure_fixture_sync.rs"]
mod failure_fixture_sync;

use cargo_val::failure_class::{Exit, Record, classify};
use failure_fixture_sync::{build, run};

fn show(case: &str, record: &Record<'_>) {
    let location = match record.location {
        Ok(site) => format!("{}:{}:{}", String::from_utf8_lossy(site.file), site.line, site.col),
        Err(absent) => format!("absent ({absent:?})"),
    };
    let message = String::from_utf8_lossy(record.message);
    println!("{case:<22} class={:?}\n  location={location}\n  message={:?}", record.class, message.lines().next());
    if let Some((left, right)) = record.sides {
        println!("  left={} right={}", String::from_utf8_lossy(left), String::from_utf8_lossy(right));
    }
    println!("  dropped={}", record.dropped);
}

fn main() {
    let plain = build("failure_class");
    let harness = build("failure_harness");
    for test in ["assert_eq_mismatch", "plain_panic", "registry_panic_under_user_call", "segfault_crash", "abort_crash"] {
        let captured = run(&plain, &format!("failing::{test}"), &[]);
        let record = classify(&captured.stdout, captured.exit, 0).expect("fixture failures classify");
        show(test, &record);
    }

    let timeout = run(&harness, "failing::body_never_completes", &[("PROXIMA_TEST_TIMEOUT_MS", "200")]);
    show("harness_timeout", &classify(&timeout.stdout, timeout.exit, 0).expect("timeout classifies"));

    let noisy = run(&plain, "failing::assert_eq_mismatch", &[("CLASSIFY_NOISE_LINES", "40")]);
    let cut = String::from_utf8_lossy(&noisy.stdout).find("left: ").expect("the assert prints its sides") + 12;
    let kept = &noisy.stdout[..cut];
    let dropped = (noisy.stdout.len() - kept.len()) as u64;
    show("truncated_capture", &classify(kept, noisy.exit, dropped).expect("head classifies"));

    show("val_deadline_silent", &classify(b"", Exit::DeadlineKilled { waiting: None }, 0).expect("deadline classifies"));
    show("val_deadline_output", &classify(b"running 1 test\n", Exit::DeadlineKilled { waiting: None }, 0).expect("deadline classifies"));
}
