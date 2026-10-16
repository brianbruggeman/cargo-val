//! A test-side deadline must fire before the harness timeout of the test that waits on it, or the lock-holder section in its failure message is never printed.

use std::fs;
use std::path::Path;
use std::time::Duration;

mod common;

use common::group;

const TEST_ATTRIBUTE: &str = "#[proxima::test(timeout = \"";

// each setting with the identifiers a test body uses to wait on it, directly or through a common helper
const WAITS: [(&str, &[&str]); 4] = [
    ("FIXTURE_DEADLINE", &["FIXTURE_DEADLINE", "fixture_deadline()", "fixture_output("]),
    ("SYNC_DEADLINE", &["SYNC_DEADLINE", "wait_within("]),
    ("FIFO_DEADLINE", &["FIFO_DEADLINE", "fifo_deadline()"]),
    ("WAIT_LINE_DEADLINE", &["WAIT_LINE_DEADLINE"]),
];

// a test that forces its own short deadline never waits on the configured one
const PINNED: [&str; 2] = ["Duration::from_millis(", "with_one_second_fifo_deadline()"];

fn harness_timeout(attribute_tail: &str) -> Option<Duration> {
    let seconds = attribute_tail.split_once("s\"")?.0.parse().ok()?;
    Some(Duration::from_secs(seconds))
}

fn test_sources() -> Vec<(String, String)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut sources: Vec<(String, String)> = fs::read_dir(directory)
        .expect("the tests directory is readable")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .map(|path| (path.display().to_string(), fs::read_to_string(&path).expect("a test source is readable")))
        .collect();
    sources.sort();
    sources
}

// (file, harness timeout, setting named in the test body) for every test that waits on one of the long deadlines
fn waiting_tests() -> Vec<(String, Duration, &'static str)> {
    let mut found = Vec::new();
    for (file, source) in test_sources() {
        for chunk in source.split(TEST_ATTRIBUTE).skip(1) {
            let Some(timeout) = harness_timeout(chunk) else { continue };
            let body = chunk.split("\n}\n").next().unwrap_or(chunk);
            if PINNED.iter().any(|pinned| body.contains(pinned)) {
                continue;
            }
            for (setting, identifiers) in WAITS {
                if identifiers.iter().any(|identifier| body.contains(identifier)) {
                    found.push((file.clone(), timeout, setting));
                }
            }
        }
    }
    found
}

#[test]
fn every_test_deadline_plus_its_kill_grace_fires_before_the_harness_timeout_of_the_tests_that_use_it() {
    let waiting = waiting_tests();
    let grace = group::settings::bound("INTERRUPT_GRACE");

    let late: Vec<String> = waiting
        .iter()
        .filter(|(_, timeout, setting)| group::settings::bound(setting) + grace >= *timeout)
        .map(|(file, timeout, setting)| format!("{file}: {setting} {:?} + grace {grace:?} >= harness timeout {timeout:?}", group::settings::bound(setting)))
        .collect();

    assert!(waiting.len() >= 20, "the scan found only {} waiting tests; the attribute or identifier patterns no longer match", waiting.len());
    assert!(late.is_empty(), "a harness timeout fires before these deadlines, so their lock-holder sections are never printed:\n{}", late.join("\n"));
}
