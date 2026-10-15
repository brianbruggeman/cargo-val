mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/failure_fixture.rs"]
mod failure_fixture;
#[path = "common/failure_fixture_async.rs"]
mod failure_fixture_async;

use cargo_val::failure_class::{Absent, Class, Enforcer, Exit, Location, Reject, classify};
use cargo_val::isolated_target::LockKind;
use failure_fixture::Captured;
use failure_fixture_async::{build, run};

const MODULE: &str = "failing::";
const FRAME_COLUMN: u32 = if cfg!(windows) { 0 } else { 25 };

async fn failing(test: &str) -> Captured {
    let executable = build("failure_class").await;
    run(&executable, &format!("{MODULE}{test}"), &[]).await
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("fixture output is UTF-8")
}

#[proxima::test(timeout = "310s")]
async fn assert_eq_yields_class_location_message_and_both_sides() {
    let captured = failing("assert_eq_mismatch").await;

    let record = classify(&captured.stdout, captured.exit, 0).expect("a failed assert classifies");

    assert_eq!(record.class, Class::Assert);
    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!((location.line, location.col), (15, 9));
    assert!(text(record.message).starts_with("assertion `left == right` failed: segment checksum differs"));
    assert_eq!(record.sides, Some((&b"[1, 2, 3]"[..], &b"[1, 2, 4]"[..])));
}

#[proxima::test(timeout = "310s")]
async fn plain_panic_yields_message_and_no_sides() {
    let captured = failing("plain_panic").await;

    let record = classify(&captured.stdout, captured.exit, 0).expect("a panic classifies");

    assert_eq!(record.class, Class::Panic);
    assert_eq!(text(record.message), "queue drained before head arrived: None");
    assert_eq!(record.sides, None);
    assert_eq!(record.location.map(|site| (site.line, site.col)), Ok((23, 9)));
}

#[proxima::test(timeout = "310s")]
async fn registry_panic_resolves_to_the_user_frame_that_called_it() {
    let captured = failing("registry_panic_under_user_call").await;
    assert!(text(&captured.stdout).contains("registry"), "the panic site must be in a registry crate");

    let record = classify(&captured.stdout, captured.exit, 0).expect("a registry panic classifies");

    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!((location.line, location.col), (30, FRAME_COLUMN));
}

#[proxima::test(timeout = "310s")]
async fn harness_timeout_is_enforced_by_harness_and_points_at_the_test_fn() {
    let harness = build("failure_harness").await;
    let captured = run(&harness, &format!("{MODULE}body_never_completes"), &[("PROXIMA_TEST_TIMEOUT_MS", "200")]).await;
    assert!(text(&captured.stdout).contains("harness.rs:174"), "the panic site must be the harness");

    let record = classify(&captured.stdout, captured.exit, 0).expect("a harness timeout classifies");

    assert_eq!(record.class, Class::Timeout { enforced_by: Enforcer::Harness });
    assert_eq!(text(record.message), "proxima::test: body did not complete within the timeout");
    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!(location.line, 3);
}

#[proxima::test(timeout = "310s")]
async fn harness_relayed_panic_location_beats_the_harness_site() {
    let harness = build("failure_harness").await;
    let captured = run(&harness, &format!("{MODULE}body_panics_after_await"), &[]).await;

    let record = classify(&captured.stdout, captured.exit, 0).expect("a relayed panic classifies");

    assert_eq!(record.class, Class::Panic);
    let location = user_site(record.location, &captured.stdout);
    assert!(in_fixture_lib(location.file), "{}", text(location.file));
    assert_eq!((location.line, location.col), (11, 9));
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn segfault_and_abort_are_crashes_carrying_the_signal() {
    for (test, signal) in [("segfault_crash", 11), ("abort_crash", 6)] {
        let captured = failing(test).await;

        let record = classify(&captured.stdout, captured.exit, 0).expect("a crash classifies");

        assert_eq!(record.class, Class::Crash { signal }, "{test}");
        assert_eq!(record.location, Err(Absent::NoPanicSite), "{test}");
    }
}

#[proxima::test(timeout = "310s")]
async fn truncated_capture_reports_dropped_bytes_and_keeps_what_survived() {
    let executable = build("failure_class").await;
    let captured = run(&executable, "failing::assert_eq_mismatch", &[("CLASSIFY_NOISE_LINES", "40")]).await;
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

#[proxima::test(timeout = "310s")]
async fn capture_cut_before_the_panic_has_no_evidence() {
    let executable = build("failure_class").await;
    let captured = run(&executable, "failing::assert_eq_mismatch", &[("CLASSIFY_NOISE_LINES", "40")]).await;
    let cut = text(&captured.stdout).find("thread '").expect("the panic header exists") - 1;

    let verdict = classify(&captured.stdout[..cut], Exit::Code(101), (captured.stdout.len() - cut) as u64);

    assert_eq!(verdict, Err(Reject::NoEvidence));
}

#[test]
fn val_deadline_kill_is_val_enforced_timeout_or_hang_when_silent() {
    let loud = classify(b"running 1 test\n", Exit::DeadlineKilled { waiting: None }, 0).expect("deadline kills classify");
    let silent = classify(b"", Exit::DeadlineKilled { waiting: None }, 0).expect("deadline kills classify");

    assert_eq!(loud.class, Class::Timeout { enforced_by: Enforcer::Val });
    assert_eq!(silent.class, Class::Hang);
}

#[proxima::test(timeout = "310s")]
async fn a_passing_exit_is_not_a_failure() {
    let captured = failing("assert_eq_mismatch").await;

    assert_eq!(classify(&captured.stdout, Exit::Code(0), 0), Err(Reject::Succeeded));
}

fn in_fixture_lib(file: &[u8]) -> bool {
    text(file).replace('\\', "/").ends_with("src/lib.rs")
}

fn user_site<'a>(location: Result<Location<'a>, Absent<'a>>, capture: &[u8]) -> Location<'a> {
    location.unwrap_or_else(|absent| panic!("no user frame ({absent:?}) in capture:\n{}", String::from_utf8_lossy(capture)))
}

#[test]
fn a_deadline_kill_behind_a_held_lock_is_a_lock_wait_whatever_the_capture_holds() {
    let waiting = Exit::DeadlineKilled { waiting: Some((LockKind::BuildDirectory, 4242)) };

    let silent = classify(b"", waiting, 0).expect("deadline kills classify");
    let loud = classify(b"    Blocking waiting for file lock on build directory\n", waiting, 0).expect("deadline kills classify");

    assert_eq!(silent.class, Class::LockWait { lock: LockKind::BuildDirectory, holder: 4242 });
    assert_eq!(loud.class, silent.class);
    assert_eq!(silent.location, Err(Absent::NoPanicSite));
}
