#[path = "common/failure_fixture.rs"]
mod failure_fixture;

use std::path::PathBuf;
use std::sync::OnceLock;

use cargo_val::failure_class::{Absent, Class, Enforcer, Exit, Location, Reject, classify};
use failure_fixture::{Captured, build, run};

const MODULE: &str = "failing::";
const FRAME_COLUMN: u32 = if cfg!(windows) { 0 } else { 25 };

fn class_fixture() -> &'static PathBuf {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE.get_or_init(|| build("failure_class"))
}

fn harness_fixture() -> &'static PathBuf {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE.get_or_init(|| build("failure_harness"))
}

fn failing(test: &str) -> Captured {
    run(class_fixture(), &format!("{MODULE}{test}"), &[])
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("fixture output is UTF-8")
}

#[test]
fn assert_eq_yields_class_location_message_and_both_sides() {
    let captured = failing("assert_eq_mismatch");

    let record = classify(&captured.stdout, captured.exit, 0).expect("a failed assert classifies");

    assert_eq!(record.class, Class::Assert);
    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!((location.line, location.col), (15, 9));
    assert!(text(record.message).starts_with("assertion `left == right` failed: segment checksum differs"));
    assert_eq!(record.sides, Some((&b"[1, 2, 3]"[..], &b"[1, 2, 4]"[..])));
}

#[test]
fn plain_panic_yields_message_and_no_sides() {
    let captured = failing("plain_panic");

    let record = classify(&captured.stdout, captured.exit, 0).expect("a panic classifies");

    assert_eq!(record.class, Class::Panic);
    assert_eq!(text(record.message), "queue drained before head arrived: None");
    assert_eq!(record.sides, None);
    assert_eq!(record.location.map(|site| (site.line, site.col)), Ok((23, 9)));
}

#[test]
fn registry_panic_resolves_to_the_user_frame_that_called_it() {
    let captured = failing("registry_panic_under_user_call");
    assert!(text(&captured.stdout).contains("registry"), "the panic site must be in a registry crate");

    let record = classify(&captured.stdout, captured.exit, 0).expect("a registry panic classifies");

    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!((location.line, location.col), (30, FRAME_COLUMN));
}

#[test]
fn harness_timeout_is_enforced_by_harness_and_points_at_the_test_fn() {
    let captured =
        run(harness_fixture(), &format!("{MODULE}body_never_completes"), &[("PROXIMA_TEST_TIMEOUT_MS", "200")]);
    assert!(text(&captured.stdout).contains("harness.rs:174"), "the panic site must be the harness");

    let record = classify(&captured.stdout, captured.exit, 0).expect("a harness timeout classifies");

    assert_eq!(record.class, Class::Timeout { enforced_by: Enforcer::Harness });
    assert_eq!(text(record.message), "proxima::test: body did not complete within the timeout");
    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!(location.line, 3);
}

#[test]
fn harness_relayed_panic_location_beats_the_harness_site() {
    let captured = run(harness_fixture(), &format!("{MODULE}body_panics_after_await"), &[]);

    let record = classify(&captured.stdout, captured.exit, 0).expect("a relayed panic classifies");

    assert_eq!(record.class, Class::Panic);
    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!((location.line, location.col), (11, 9));
}

#[cfg(unix)]
#[test]
fn segfault_and_abort_are_crashes_carrying_the_signal() {
    for (test, signal) in [("segfault_crash", 11), ("abort_crash", 6)] {
        let captured = failing(test);

        let record = classify(&captured.stdout, captured.exit, 0).expect("a crash classifies");

        assert_eq!(record.class, Class::Crash { signal }, "{test}");
        assert_eq!(record.location, Err(Absent::NoPanicSite), "{test}");
    }
}

#[test]
fn truncated_capture_reports_dropped_bytes_and_keeps_what_survived() {
    let captured = run(class_fixture(), "failing::assert_eq_mismatch", &[("CLASSIFY_NOISE_LINES", "40")]);
    let full = captured.stdout.len();
    let marker = text(&captured.stdout).find("left: ").expect("the assert prints its sides");
    let kept = marker + 12;
    let dropped = (full - kept) as u64;

    let record = classify(&captured.stdout[..kept], captured.exit, dropped).expect("a head-only capture classifies");

    assert_eq!(record.dropped, dropped);
    assert_eq!(record.class, Class::Assert);
    assert_eq!(record.location.map(|site| site.line), Ok(15));
    assert!(record.sides.is_none(), "the right side was in the dropped tail");
}

#[test]
fn capture_cut_before_the_panic_has_no_evidence() {
    let captured = run(class_fixture(), "failing::assert_eq_mismatch", &[("CLASSIFY_NOISE_LINES", "40")]);
    let cut = text(&captured.stdout).find("thread '").expect("the panic header exists") - 1;

    let verdict = classify(&captured.stdout[..cut], Exit::Code(101), (captured.stdout.len() - cut) as u64);

    assert_eq!(verdict, Err(Reject::NoEvidence));
}

#[test]
fn val_deadline_kill_is_val_enforced_timeout_or_hang_when_silent() {
    let loud = classify(b"running 1 test\n", Exit::DeadlineKilled, 0).expect("deadline kills classify");
    let silent = classify(b"", Exit::DeadlineKilled, 0).expect("deadline kills classify");

    assert_eq!(loud.class, Class::Timeout { enforced_by: Enforcer::Val });
    assert_eq!(silent.class, Class::Hang);
}

#[test]
fn a_passing_exit_is_not_a_failure() {
    let captured = failing("assert_eq_mismatch");

    assert_eq!(classify(&captured.stdout, Exit::Code(0), 0), Err(Reject::Succeeded));
}

fn in_fixture_lib(file: &[u8]) -> bool {
    text(file).replace('\\', "/").ends_with("src/lib.rs")
}

fn user_site<'a>(location: Result<Location<'a>, Absent<'a>>, capture: &[u8]) -> Location<'a> {
    location.unwrap_or_else(|absent| panic!("no user frame ({absent:?}) in capture:\n{}", String::from_utf8_lossy(capture)))
}
