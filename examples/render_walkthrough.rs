//! Drives every legal transition of the render core and prints the plain projection of each event, then the review keys.
//!
//! The state is `cargo_val::render::State`; each event goes through `apply` (the sans-IO transition) and `project`
//! (state to bytes in a caller-provided `Line`). The driver does the same and hands the bytes to `write_fd`.

use std::io::Write;

use cargo_val::line::Line;
use cargo_val::failure_class::{Class, Enforcer, Location};
use cargo_val::render::review::{Key, Rerun, Review, Row, Verdict, action};
use cargo_val::render::{Event, Identity, Mode, Outcome, State};
use cargo_val::view_defaults::{FAILURES, NAME_BYTES, RERUN_BYTES};

const FAILING: Identity<'static> = (b"sched_records", b"the_host_binary_format_yields_every_declared_record");
const PASSING: Identity<'static> = (b"c68_run_policy", b"c68_max_fail_two_stops_after_the_second_of_three_failures");

fn show(state: &mut State<'_, RERUN_BYTES>, event: Event<'static>) -> Result<(), String> {
    let before = state.mode();
    state.apply(&event);
    let mut frame = Line::<16_384>::new();
    state.project(&event, &mut frame).map_err(|lost| format!("frame cut {lost} bytes"))?;
    std::io::stdout().write_all(frame.bytes()).map_err(|error| error.to_string())?;
    if before != state.mode() {
        println!("-- mode {before:?} -> {:?}", state.mode());
    }
    Ok(())
}

fn finished(failed: usize) -> Event<'static> {
    Event::Finished { passed: 2, failed, ignored: 0, not_run: 0, wall_ms: 258_235, manifest: Some(b"Cargo.toml") }
}

fn run_with_failure() -> Result<(), String> {
    let mut arena = [0_u8; NAME_BYTES];
    let mut ledger = [0_u32; FAILURES];
    let mut state = State::<RERUN_BYTES>::new(&mut arena, &mut ledger);
    show(&mut state, Event::Begin { version: b"0.1.0", shard: true, jobs: 8, github: true })?;
    show(&mut state, Event::Listed { binaries: 19, tests: 383 })?;
    show(&mut state, Event::Started { id: PASSING, solo: false })?;
    show(&mut state, Event::Settled { id: PASSING, outcome: Outcome::Passed, millis: Some(3123), solo: false })?;
    show(&mut state, Event::Settled { id: FAILING, outcome: Outcome::Failed, millis: Some(71_660), solo: false })?;
    show(&mut state, Event::Captured { id: FAILING, stdout: (b"", false), stderr: (b"body did not complete within the timeout", false) })?;
    show(&mut state, Event::Warned { id: FAILING, what: b"solo rerun passed" })?;
    show(&mut state, finished(1))
}

fn run_clean() -> Result<(), String> {
    let mut arena = [0_u8; NAME_BYTES];
    let mut ledger = [0_u32; FAILURES];
    let mut state = State::<RERUN_BYTES>::new(&mut arena, &mut ledger);
    show(&mut state, Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false })?;
    show(&mut state, Event::Settled { id: PASSING, outcome: Outcome::Ignored, millis: None, solo: false })?;
    show(&mut state, finished(0))
}

fn run_ended_early() -> Result<(), String> {
    let (mut build_arena, mut interrupted_arena) = ([0_u8; NAME_BYTES], [0_u8; NAME_BYTES]);
    let (mut build_ledger, mut interrupted_ledger) = ([0_u32; FAILURES], [0_u32; FAILURES]);
    let mut build = State::<RERUN_BYTES>::new(&mut build_arena, &mut build_ledger);
    show(&mut build, Event::BuildFailed { diagnostic: b"error[E0308]: mismatched types" })?;
    let mut interrupted = State::<RERUN_BYTES>::new(&mut interrupted_arena, &mut interrupted_ledger);
    show(&mut interrupted, Event::Interrupted { signal: 2 })?;
    assert!(matches!(interrupted.mode(), Mode::Interrupted { signal: 2 }));
    Ok(())
}

const REVIEW_KEYS: [Key; 14] = [
    Key::Char(b'f'),
    Key::Char(b'j'),
    Key::Char(b'e'),
    Key::Char(b'r'),
    Key::Char(b's'),
    Key::Esc,
    Key::Char(b'q'),
    Key::Esc,
    Key::Char(b'R'),
    Key::Char(b'i'),
    Key::Char(b'i'),
    Key::Char(b'q'),
    Key::Char(b'q'),
    Key::CtrlC,
];

fn failed_row(id: Identity<'static>, class: Class, millis: u64, rerun: Rerun) -> Row<'static> {
    Row {
        id,
        class,
        millis,
        location: Some(Location { file: b"tests/sched_records.rs", line: 55, col: 1 }),
        output: b"proxima::test: body did not complete within the timeout\n",
        verdict: Some(Verdict::NotRetried { budget_secs: 30 }),
        rerun,
    }
}

fn show_review(state: &mut State<'_, RERUN_BYTES>, rows: &[Row<'_>], key: Key) -> Result<(), String> {
    let before = state.mode();
    let todo = action(before, key, rows);
    state.apply(&Event::Key(key));
    println!("-- key {key:?}: {before:?} -> {:?}  action {todo:?}", state.mode());
    let review = Review { rows, passed: 381, skipped: 0, warnings: 0, clock_ms: 138_205 };
    let mut frame = Line::<16_384>::new();
    review.project(state.mode(), 80, false, &mut frame).map_err(|lost| format!("frame cut {lost} bytes"))?;
    std::io::stdout().write_all(frame.bytes()).map_err(|error| error.to_string())
}

fn run_review() -> Result<(), String> {
    let timeout = Class::Timeout { enforced_by: Enforcer::Val };
    let rows = [
        failed_row(FAILING, timeout, 71_660, Rerun::Idle),
        failed_row(PASSING, Class::Panic, 3_123, Rerun::Running { elapsed_ms: 1_200, expected_ms: Some(8_400) }),
    ];
    let mut arena = [0_u8; NAME_BYTES];
    let mut ledger = [0_u32; FAILURES];
    let mut state = State::<RERUN_BYTES>::new(&mut arena, &mut ledger);
    for (id, millis) in [(FAILING, 71_660), (PASSING, 3_123)] {
        state.apply(&Event::Settled { id, outcome: Outcome::Failed, millis: Some(millis), solo: false });
    }
    for key in REVIEW_KEYS {
        show_review(&mut state, &rows, key)?;
    }
    assert_eq!(state.mode(), Mode::Receipt, "q q ends review and a final ctrl-c does not reopen it");
    let mut other_arena = [0_u8; NAME_BYTES];
    let mut other_ledger = [0_u32; FAILURES];
    let mut live = State::<RERUN_BYTES>::new(&mut other_arena, &mut other_ledger);
    live.apply(&Event::Settled { id: FAILING, outcome: Outcome::Failed, millis: Some(71_660), solo: false });
    show_review(&mut live, &rows, Key::Char(b'f'))?;
    show_review(&mut live, &rows, Key::CtrlC)?;
    assert_eq!(live.mode(), Mode::Interrupted { signal: 2 });
    Ok(())
}

fn main() -> Result<(), String> {
    run_with_failure()?;
    println!("--");
    run_clean()?;
    println!("--");
    run_ended_early()?;
    println!("--");
    run_review()
}
