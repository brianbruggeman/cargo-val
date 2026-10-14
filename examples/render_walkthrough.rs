//! Drives every legal transition of the render core and prints the plain projection of each event.
//!
//! The state is `cargo_val::render::State`; each event goes through `apply` (the sans-IO transition) and `project`
//! (state to bytes in a caller-provided `Line`). The driver does the same and hands the bytes to `write_fd`.

use std::io::Write;

use cargo_val::line::Line;
use cargo_val::render::{Event, Identity, Mode, Outcome, State};
use cargo_val::view_defaults::{FAILURES, INFLIGHT_BYTES, NAME_BYTES, RERUN_BYTES};

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
    let mut inflight = [0_u8; INFLIGHT_BYTES];
    let mut state = State::<RERUN_BYTES>::new(&mut arena, &mut ledger).with_inflight(&mut inflight);
    show(&mut state, Event::Begin { version: b"0.1.0", shard: true, jobs: 8, github: true })?;
    show(&mut state, Event::Listed { binaries: 19, of: Some(19), tests: 383 })?;
    show(&mut state, Event::Started { id: PASSING, solo: false, at_ms: 1_000, limit_ms: None })?;
    show(&mut state, Event::Started { id: FAILING, solo: false, at_ms: 1_200, limit_ms: Some(120_000) })?;
    show(&mut state, Event::Tick { now_ms: 11_000 })?;
    show(&mut state, Event::Settled { id: PASSING, outcome: Outcome::Passed, at_ms: Some(4_123), solo: false })?;
    show(&mut state, Event::Settled { id: FAILING, outcome: Outcome::Failed, at_ms: Some(72_860), solo: false })?;
    show(&mut state, Event::Captured { id: FAILING, stdout: (b"", false), stderr: (b"body did not complete within the timeout", false) })?;
    show(&mut state, Event::Warned { id: FAILING, what: b"solo rerun passed" })?;
    show(&mut state, finished(1))
}

fn run_clean() -> Result<(), String> {
    let mut arena = [0_u8; NAME_BYTES];
    let mut ledger = [0_u32; FAILURES];
    let mut state = State::<RERUN_BYTES>::new(&mut arena, &mut ledger);
    show(&mut state, Event::Begin { version: b"0.1.0", shard: false, jobs: 2, github: false })?;
    show(&mut state, Event::Settled { id: PASSING, outcome: Outcome::Ignored, at_ms: None, solo: false })?;
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

fn main() -> Result<(), String> {
    run_with_failure()?;
    println!("--");
    run_clean()?;
    println!("--");
    run_ended_early()
}
