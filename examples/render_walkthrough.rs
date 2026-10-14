//! Drives every legal transition of the render core and prints the plain projection of each event, then the review keys.
//!
//! The state is `cargo_val::render::State`; each event goes through `apply` (the sans-IO transition) and `project`
//! (state to bytes in a caller-provided `Line`). The driver does the same and hands the bytes to `write_fd`.

use std::io::Write;

use cargo_val::line::Line;
use cargo_val::failure_class::{Class, Enforcer, Location};
use cargo_val::render::review::{Detail, Key, Rerun, Verdict};
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

const REVIEW_KEYS: [Key; 17] = [
    Key::Char(b'f'),
    Key::Char(b'j'),
    Key::Enter,
    Key::Char(b'j'),
    Key::Enter,
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

fn detail(id: Identity<'_>) -> Detail<'static> {
    let location = Location { file: b"tests/sched_records.rs", line: 55, col: 1 };
    let class = if id == FAILING { Class::Timeout { enforced_by: Enforcer::Val } } else { Class::Panic };
    (class, Some(location), b"proxima::test: body did not complete within the timeout\nsecond line\nthird line\n")
}

fn show_review(state: &mut State<'_, RERUN_BYTES>, key: Key) -> Result<(), String> {
    let before = state.mode();
    let todo = std::format!("{:?}", state.action(key, detail));
    state.apply(&Event::Key(key));
    println!("-- key {key:?}: {before:?} -> {:?}  action {todo}", state.mode());
    let mut frame = Line::<16_384>::new();
    state.screen(detail, 80, false, &mut frame).map_err(|lost| format!("frame cut {lost} bytes"))?;
    std::io::stdout().write_all(frame.bytes()).map_err(|error| error.to_string())
}

fn fail(state: &mut State<'_, RERUN_BYTES>, id: Identity<'static>, millis: u64) {
    let output: &'static [u8] = b"proxima::test: body did not complete within the timeout\nsecond line\nthird line\n";
    state.apply(&Event::Settled { id, outcome: Outcome::Failed, millis: Some(millis), solo: false });
    state.apply(&Event::Captured { id, stdout: (output, false), stderr: (b"", false) });
    state.apply(&Event::Retried { id, verdict: Verdict::NotRetried { budget_secs: 30 } });
}

fn run_review() -> Result<(), String> {
    let mut arena = [0_u8; NAME_BYTES];
    let mut ledger = [0_u32; FAILURES];
    let mut state = State::<RERUN_BYTES>::new(&mut arena, &mut ledger);
    fail(&mut state, FAILING, 71_660);
    fail(&mut state, PASSING, 3_123);
    state.apply(&Event::Tick { now_ms: 138_205 });
    state.apply(&Event::Reran { id: PASSING, rerun: Rerun::Running { elapsed_ms: 1_200, expected_ms: Some(8_400) } });
    for key in REVIEW_KEYS {
        show_review(&mut state, key)?;
    }
    assert_eq!(state.mode(), Mode::Receipt, "q q ends review and a final ctrl-c does not reopen it");
    let mut other_arena = [0_u8; NAME_BYTES];
    let mut other_ledger = [0_u32; FAILURES];
    let mut live = State::<RERUN_BYTES>::new(&mut other_arena, &mut other_ledger);
    fail(&mut live, FAILING, 71_660);
    show_review(&mut live, Key::Char(b'f'))?;
    live.apply(&Event::Reran { id: FAILING, rerun: Rerun::Passed { millis: 1_840 } });
    live.apply(&Event::Tick { now_ms: 1_500 });
    show_review(&mut live, Key::Char(b'j'))?;
    show_review(&mut live, Key::CtrlC)?;
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
