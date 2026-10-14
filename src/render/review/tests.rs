use std::fs;
use std::path::Path;
use std::string::String;
use std::vec;
use std::vec::Vec;

use super::{Action, Detail, Key, Rerun, Verdict};
use crate::failure_class::{Class, Enforcer, Exit, Location};
use crate::line::Line;
use crate::render::{Event, Identity, Mode, Outcome, State, UNREAD};
use crate::test_alloc::measure_allocations;

const SCHED: (&[u8], &[u8]) = (b"sched_records", b"the_host_binary_format_yields_every_declared_record_with_flags_and_group");
const SHARD: (&[u8], &[u8]) = (b"shard_process", b"a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly");
const SCHED_OUTPUT: &[u8] = b"proxima::test: body did not complete within the timeout\n";
const SHARD_OUTPUT: &[u8] = b"thread 'a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly' panicked at tests/shard_process.rs:575:31:\nthe shard child reported its pid before the deadline\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
const WIDTHS: [usize; 3] = [80, 60, 40];
const CLOCK_MS: u64 = 138_205;
const NOT_RETRIED: Verdict = Verdict::NotRetried { budget_secs: 30 };

type Review<'storage> = State<'storage, 400>;

fn tracked<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32]) -> Review<'storage> {
    let inflight: &'static mut [u8] = std::boxed::Box::leak(vec![0_u8; 512].into_boxed_slice());
    State::new(arena, ledger).with_inflight(inflight)
}

fn detail(id: Identity<'_>) -> Detail<'static> {
    if id == SCHED {
        let location = Location { file: b"tests/sched_records.rs", line: 55, col: 1 };
        (Class::Timeout { enforced_by: Enforcer::Val }, Some(location), (b"", None), SCHED_OUTPUT)
    } else {
        let location = Location { file: b"tests/shard_process.rs", line: 575, col: 31 };
        (Class::Panic, Some(location), (b"the shard child reported its pid before the deadline", None), SHARD_OUTPUT)
    }
}

fn settle(state: &mut Review<'_>, id: Identity<'_>, outcome: Outcome, millis: u64) {
    state.apply(&Event::Started { id, solo: false, at_ms: 0, limit_ms: None });
    state.apply(&Event::Settled { id, outcome, at_ms: Some(millis), solo: false });
}

fn run<'storage>(arena: &'storage mut [u8], ledger: &'storage mut [u32], passed: usize) -> Review<'storage> {
    let mut state = tracked(arena, ledger);
    for _ in 0..passed {
        settle(&mut state, (b"c68_run_policy", b"a_passing_test"), Outcome::Passed, 3_123);
    }
    for (id, millis, output) in [(SCHED, 71_660, SCHED_OUTPUT), (SHARD, 120_030, SHARD_OUTPUT)] {
        settle(&mut state, id, Outcome::Failed, millis);
        state.apply(&Event::Captured { id, stdout: (output, 0), stderr: (b"", 0), exit: if id == SCHED { Exit::DeadlineKilled } else { Exit::Code(101) }, excerpt: UNREAD });
        state.apply(&Event::Retried { id, verdict: NOT_RETRIED });
    }
    state.apply(&Event::Tick { now_ms: CLOCK_MS });
    state
}

fn keys(state: &mut Review<'_>, pressed: &[Key]) {
    for key in pressed {
        state.apply(&Event::Key(*key));
    }
}

fn scenario<'storage>(name: &str, arena: &'storage mut [u8], ledger: &'storage mut [u32]) -> Review<'storage> {
    let mut state = run(arena, ledger, if name == "empty" { 379 } else { 381 });
    keys(&mut state, &[Key::Char(b'f')]);
    match name {
        "detail" => keys(&mut state, &[Key::Char(b'j')]),
        "rerun_running" => state.apply(&Event::Reran { id: SCHED, rerun: Rerun::Running { elapsed_ms: 1_200, expected_ms: Some(8_400) } }),
        "rerun_passed" => state.apply(&Event::Reran { id: SCHED, rerun: Rerun::Passed { millis: 8_400 } }),
        "empty" => {
            for id in [SCHED, SHARD] {
                state.apply(&Event::Reran { id, rerun: Rerun::Passed { millis: 8_400 } });
            }
            state.apply(&Event::Tick { now_ms: CLOCK_MS + 1_500 });
        }
        _ => {}
    }
    state
}

fn frame_of(state: &Review<'_>, width: usize, color: bool) -> String {
    let mut frame = Line::<8192>::new();
    state.screen(detail, width, color, &mut frame).expect("the frame holds one review screen");
    String::from_utf8(frame.bytes().to_vec()).expect("a review frame is utf-8")
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/review").join(name);
    if std::env::var_os("CARGO_VAL_BLESS_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().expect("a golden path has a parent")).expect("the golden directory is creatable");
        fs::write(&path, actual).expect("the golden file is writable");
        return;
    }
    let expected = fs::read_to_string(&path).expect("the golden file exists; bless it with CARGO_VAL_BLESS_GOLDEN=1");
    assert_eq!(actual, expected, "golden {name}");
}

fn columns(line: &str) -> usize {
    let mut inside_escape = false;
    line.chars()
        .filter(|character| match (inside_escape, *character) {
            (_, '\x1b') => {
                inside_escape = true;
                false
            }
            (true, 'm') => {
                inside_escape = false;
                false
            }
            (inside, _) => !inside,
        })
        .count()
}

fn key_mode(from: Mode, key: Key, open: usize) -> Mode {
    from.next(&Event::Key(key), open, 0)
}

fn list_rows(frame: &str) -> Vec<String> {
    frame
        .lines()
        .skip_while(|line| !line.contains("passed"))
        .skip(1)
        .take_while(|line| !line.starts_with(" \u{2500}"))
        .filter(|line| !line.trim().is_empty() && !line.contains(" failures"))
        .map(String::from)
        .collect()
}

#[test]
fn golden_frames_at_every_width_with_colour_and_without() {
    let mut frames = 0;
    for name in ["list", "detail", "rerun_running", "rerun_passed", "empty"] {
        for width in WIDTHS {
            for (color, tag) in [(true, "color"), (false, "plain")] {
                let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
                let state = scenario(name, &mut arena, &mut ledger);
                golden(&std::format!("{name}_{width}_{tag}.txt"), &frame_of(&state, width, color));
                frames += 1;
            }
        }
    }
    assert_eq!(frames, 30, "5 frames x 3 widths x 2 colour settings were compared");
}

#[test]
fn no_color_frames_carry_no_escape_and_no_frame_exceeds_its_width() {
    let mut checked = 0;
    for width in WIDTHS {
        for selected in 0..2 {
            for armed in [false, true] {
                let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
                let mut state = scenario("list", &mut arena, &mut ledger);
                state.apply(&Event::Reran { id: SCHED, rerun: Rerun::Running { elapsed_ms: 900, expected_ms: None } });
                keys(&mut state, &vec![Key::Char(b'j'); selected]);
                keys(&mut state, &vec![Key::Char(b'q'); usize::from(armed)]);
                let plain = frame_of(&state, width, false);
                let colored = frame_of(&state, width, true);
                assert!(!plain.contains('\x1b'), "NO_COLOR frame at {width} carries an escape");
                assert!(colored.contains('\x1b'), "the colour frame at {width} carries colour");
                for line in plain.lines().chain(colored.lines()) {
                    assert!(columns(line) <= width, "{width}: {line:?} is {} columns", columns(line));
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 100, "{checked} lines were measured");
}

#[test]
fn the_bisect_hint_and_the_no_retry_reason_wrap_at_80_columns_without_an_ellipsis() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("list", &mut arena, &mut ledger);
    keys(&mut state, &[Key::Char(b'j')]);

    let text = frame_of(&state, 80, false);

    let lines: Vec<&str> = text.lines().collect();
    let hint = lines.iter().position(|line| line.starts_with("find the commit")).expect("a bisect hint");
    let reason = lines.iter().position(|line| line.starts_with("NO-RETRY not")).expect("a no-retry reason");
    assert_eq!(lines[hint], "find the commit  cargo val bisect shard_process");
    assert_eq!(lines[hint + 1], "  a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly");
    assert_eq!(lines[reason], "NO-RETRY not retried alone: it failed after 120.030s, over the 30s retry");
    assert_eq!(lines[reason + 1], "  budget. r retries it now");
    for line in [lines[hint], lines[hint + 1], lines[reason], lines[reason + 1]] {
        assert!(!line.contains('\u{2026}'), "{line}");
    }
}

#[test]
fn the_bisect_hint_appears_on_real_and_not_retried_rows_only() {
    let mut shown = Vec::new();
    let verdicts = [
        ("real", Some(Verdict::Real)),
        ("flaky", Some(Verdict::Flaky)),
        ("order", Some(Verdict::OrderDependent)),
        ("undecided", None),
        ("not retried", Some(NOT_RETRIED)),
    ];

    for (name, verdict) in verdicts {
        let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
        let mut state = tracked(&mut arena, &mut ledger);
        settle(&mut state, SHARD, Outcome::Failed, 120_030);
        if let Some(verdict) = verdict {
            state.apply(&Event::Retried { id: SHARD, verdict });
        }
        keys(&mut state, &[Key::Char(b'f')]);
        let text = frame_of(&state, 80, false);
        if text.contains("find the commit") {
            shown.push(name);
        }
        assert!(!text.contains("B "), "{name}: the keys line has no B key");
    }

    assert_eq!(shown, ["real", "not retried"]);
}

#[test]
fn a_list_name_that_does_not_fit_keeps_its_head_and_its_tail() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let state = scenario("list", &mut arena, &mut ledger);

    let text = frame_of(&state, 60, false);

    let row = text.lines().find(|line| line.starts_with('\u{203a}')).expect("the selected row");
    assert!(row.contains("sched_records:"), "{row}");
    assert!(row.ends_with("lags_and_group"), "{row}");
    assert!(row.contains('\u{2026}'), "{row}");
}

#[test]
fn q_once_arms_and_does_not_quit_while_q_twice_quits_and_esc_stays_in_review() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("list", &mut arena, &mut ledger);
    let review_mode = state.mode();

    assert_eq!(state.action(Key::Char(b'q'), detail), Action::None, "one q writes nothing");
    keys(&mut state, &[Key::Char(b'q')]);
    let armed = state.mode();
    assert_eq!(state.action(Key::Char(b'q'), detail), Action::Quit);
    let armed_frame = frame_of(&state, 80, false);
    keys(&mut state, &[Key::Esc]);
    let after_esc = state.mode();
    keys(&mut state, &[Key::Char(b'q'), Key::Char(b'q')]);

    assert_eq!(review_mode, Mode::Review { selected: 0, armed: false, top: None });
    assert_eq!(armed, Mode::Review { selected: 0, armed: true, top: None });
    assert!(armed_frame.contains("q again to confirm, any key cancels"));
    assert_eq!(after_esc, review_mode, "esc stays in review and disarms");
    assert_eq!(state.mode(), Mode::Receipt);
}

#[test]
fn every_transition_of_the_spec_table_lands_where_it_says() {
    let review_at = |selected, armed| Mode::Review { selected, armed, top: None };
    let (running, back_review) = (None, Some(1));
    let interrupted = Mode::Interrupted { signal: 2 };
    let rows: [(Mode, Key, usize, Mode); 39] = [
        (Mode::Running, Key::Char(b'f'), 2, review_at(0, false)),
        (Mode::Running, Key::Char(b'f'), 0, Mode::Running),
        (Mode::Running, Key::Char(b's'), 2, Mode::Timeline { back: running }),
        (Mode::Running, Key::Char(b'/'), 2, Mode::Find { back: running }),
        (Mode::Running, Key::Char(b'i'), 2, Mode::Skipped { back: running }),
        (Mode::Running, Key::Char(b'w'), 2, Mode::Warnings { back: running }),
        (Mode::Running, Key::Char(b'q'), 2, Mode::QuitArmed),
        (Mode::QuitArmed, Key::Char(b'q'), 2, Mode::Receipt),
        (Mode::QuitArmed, Key::Char(b'j'), 2, Mode::Running),
        (Mode::Running, Key::Char(b'c'), 2, interrupted),
        (Mode::Running, Key::CtrlC, 2, interrupted),
        (review_at(1, false), Key::Char(b'c'), 2, interrupted),
        (review_at(1, false), Key::CtrlC, 2, interrupted),
        (Mode::Timeline { back: running }, Key::CtrlC, 2, interrupted),
        (Mode::Find { back: running }, Key::CtrlC, 2, interrupted),
        (Mode::Skipped { back: running }, Key::CtrlC, 2, interrupted),
        (Mode::Warnings { back: running }, Key::CtrlC, 2, interrupted),
        (review_at(0, false), Key::Char(b'j'), 2, review_at(1, false)),
        (review_at(1, false), Key::Char(b'j'), 2, review_at(1, false)),
        (review_at(0, false), Key::Down, 2, review_at(1, false)),
        (review_at(1, false), Key::Char(b'k'), 2, review_at(0, false)),
        (review_at(0, false), Key::Up, 2, review_at(0, false)),
        (review_at(1, false), Key::Esc, 2, review_at(1, false)),
        (review_at(1, false), Key::Char(b'r'), 2, review_at(1, false)),
        (review_at(1, false), Key::Char(b'R'), 2, review_at(1, false)),
        (review_at(1, false), Key::Char(b'e'), 2, review_at(1, false)),
        (review_at(1, false), Key::Char(b's'), 2, Mode::Timeline { back: back_review }),
        (review_at(1, false), Key::Char(b'/'), 2, Mode::Find { back: back_review }),
        (review_at(1, false), Key::Char(b'i'), 2, Mode::Skipped { back: back_review }),
        (review_at(1, false), Key::Char(b'w'), 2, Mode::Warnings { back: back_review }),
        (review_at(1, false), Key::Char(b'q'), 2, review_at(1, true)),
        (review_at(1, true), Key::Char(b'q'), 2, Mode::Receipt),
        (review_at(1, true), Key::Char(b'j'), 2, review_at(1, false)),
        (Mode::Timeline { back: back_review }, Key::Esc, 2, review_at(1, false)),
        (Mode::Timeline { back: back_review }, Key::Char(b's'), 2, review_at(1, false)),
        (Mode::Find { back: running }, Key::Esc, 2, Mode::Running),
        (Mode::Skipped { back: running }, Key::Char(b'i'), 2, Mode::Running),
        (Mode::Warnings { back: back_review }, Key::Char(b'w'), 2, review_at(1, false)),
        (Mode::Receipt, Key::Char(b'j'), 2, Mode::Receipt),
    ];

    for (from, key, open, to) in rows {
        assert_eq!(key_mode(from, key, open), to, "{from:?} on {key:?} with {open} open");
    }
}

#[test]
fn enter_toggles_between_list_focus_and_detail_scroll_and_j_k_scroll_the_detail_by_line() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("detail", &mut arena, &mut ledger);
    let list_focus = state.mode();
    let list_frame = frame_of(&state, 80, false);

    keys(&mut state, &[Key::Enter]);
    let scrolling = state.mode();
    let scrolling_frame = frame_of(&state, 80, false);
    keys(&mut state, &[Key::Char(b'j'), Key::Char(b'j'), Key::Char(b'j'), Key::Down]);
    let past_the_end = state.mode();
    keys(&mut state, &[Key::Char(b'k')]);
    let back_one = state.mode();
    let scrolled_frame = frame_of(&state, 80, false);
    keys(&mut state, &[Key::Enter, Key::Char(b'k')]);

    assert_eq!(list_focus, Mode::Review { selected: 1, armed: false, top: None });
    assert_eq!(scrolling, Mode::Review { selected: 1, armed: false, top: Some(0) });
    assert_eq!(past_the_end, Mode::Review { selected: 1, armed: false, top: Some(2) }, "3 output lines bound the scroll at the last line");
    assert_eq!(back_one, Mode::Review { selected: 1, armed: false, top: Some(1) });
    assert_eq!(state.mode(), Mode::Review { selected: 0, armed: false, top: None }, "enter returns focus to the list, so k moves the cursor");
    assert!(list_frame.contains("j/k move") && !list_frame.contains("j/k scroll"));
    assert!(scrolling_frame.contains("j/k scroll  enter list"), "{scrolling_frame}");
    assert!(scrolled_frame.contains("+1 lines above"), "{scrolled_frame}");
    assert!(scrolled_frame.contains("the shard child reported its pid") && !scrolled_frame.contains("thread 'a_per_test"), "{scrolled_frame}");
}

#[test]
fn the_run_events_move_the_screens_the_way_the_spec_table_says() {
    let finished = |failed| Event::Finished { passed: 1, failed, ignored: 0, not_run: 0, wall_ms: 5, manifest: None, discovered: 0, filtered: 0, exit: 0 };
    let build = Event::BuildFailed { diagnostic: b"E0308", exit: 3 };
    let signal = Event::Interrupted { signal: 15, groups: 0 };
    let tick = Event::Tick { now_ms: 9 };
    let review_mode = Mode::Review { selected: 1, armed: false, top: None };
    let rows = [
        (Mode::Running, finished(0), Mode::Done { wall_ms: 5 }),
        (Mode::Running, finished(2), Mode::Done { wall_ms: 5 }),
        (review_mode, finished(0), review_mode),
        (Mode::Running, build, Mode::BuildError),
        (review_mode, build, review_mode),
        (review_mode, signal, Mode::Interrupted { signal: 15 }),
        (Mode::Timeline { back: None }, signal, Mode::Interrupted { signal: 15 }),
        (Mode::QuitArmed, signal, Mode::Interrupted { signal: 15 }),
        (Mode::Receipt, signal, Mode::Receipt),
        (Mode::Done { wall_ms: 5 }, signal, Mode::Done { wall_ms: 5 }),
        (review_mode, tick, review_mode),
        (Mode::Find { back: None }, tick, Mode::Find { back: None }),
    ];

    for (from, event, to) in rows {
        assert_eq!(from.next(&event, 2, 0), to, "{from:?} on {event:?}");
    }
    assert_eq!(review_mode.next(&finished(1), 1, 0), Mode::Review { selected: 0, armed: false, top: None }, "a shrunk list clamps the selection");
}

#[test]
fn a_rerun_that_passes_leaves_the_list_and_the_cursor_clamps_to_the_rows_that_remain() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("detail", &mut arena, &mut ledger);
    let before = frame_of(&state, 80, false);

    state.apply(&Event::Reran { id: SHARD, rerun: Rerun::Passed { millis: 1_840 } });
    let showing_pass = frame_of(&state, 80, false);
    state.apply(&Event::Tick { now_ms: CLOCK_MS + 1_499 });
    let still_listed = state.open();
    state.apply(&Event::Tick { now_ms: CLOCK_MS + 1_500 });
    let after = frame_of(&state, 80, false);

    assert_eq!(list_rows(&before).len(), 2, "{before}");
    assert!(showing_pass.contains("PASS") && showing_pass.contains("1 failed  382 passed"), "{showing_pass}");
    assert_eq!(still_listed, 2, "the PASS row stays 1.5 s");
    assert_eq!(state.open(), 1);
    assert_eq!(state.mode(), Mode::Review { selected: 0, armed: false, top: None }, "the cursor was on the row that left");
    assert_eq!(list_rows(&after).len(), 1, "{after}");
    assert!(list_rows(&after)[0].starts_with('\u{203a}') && list_rows(&after)[0].contains("sched_records"), "{after}");
    assert!(after.contains("1 failed  382 passed"), "{after}");
}

#[test]
fn a_cleared_ledger_row_cannot_be_moved_past_the_end_of_the_list() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("list", &mut arena, &mut ledger);
    state.apply(&Event::Reran { id: SHARD, rerun: Rerun::Passed { millis: 1_840 } });
    state.apply(&Event::Tick { now_ms: CLOCK_MS + 1_500 });

    keys(&mut state, &[Key::Char(b'j'), Key::Char(b'j'), Key::Down]);

    assert_eq!(state.mode(), Mode::Review { selected: 0, armed: false, top: None });
}

#[test]
fn an_armed_q_expires_two_seconds_after_the_press_and_the_core_reads_no_clock() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut running = tracked(&mut arena, &mut ledger);
    running.apply(&Event::Tick { now_ms: 10_000 });
    keys(&mut running, &[Key::Char(b'q')]);
    let armed = running.mode();
    running.apply(&Event::Tick { now_ms: 11_999 });
    let inside_window = running.mode();
    running.apply(&Event::Tick { now_ms: 12_000 });
    let expired = running.mode();
    keys(&mut running, &[Key::Char(b'q')]);
    let rearmed = running.mode();

    let (mut review_arena, mut review_ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut review = scenario("list", &mut review_arena, &mut review_ledger);
    keys(&mut review, &[Key::Char(b'q')]);
    review.apply(&Event::Tick { now_ms: CLOCK_MS + 2_000 });
    let review_expired = review.mode();
    keys(&mut review, &[Key::Char(b'q')]);

    assert_eq!(armed, Mode::QuitArmed);
    assert_eq!(inside_window, Mode::QuitArmed, "1.999 s after the press is still inside the window");
    assert_eq!(expired, Mode::Running, "2.000 s after the press the arm is gone");
    assert_eq!(rearmed, Mode::QuitArmed, "a q after expiry arms again instead of quitting");
    assert_eq!(review_expired, Mode::Review { selected: 0, armed: false, top: None });
    assert_eq!(review.mode(), Mode::Review { selected: 0, armed: true, top: None });
}

#[test]
fn the_list_carries_a_verdict_column_with_the_settled_words() {
    let cases = [(Verdict::Real, "REAL"), (Verdict::Flaky, "FLAKY"), (Verdict::OrderDependent, "ORDER-DEPENDENT"), (NOT_RETRIED, "NO-RETRY")];

    for (verdict, word) in cases {
        let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
        let mut state = scenario("list", &mut arena, &mut ledger);
        state.apply(&Event::Retried { id: SCHED, verdict });
        state.apply(&Event::Retried { id: SHARD, verdict: Verdict::Real });

        let rows = list_rows(&frame_of(&state, 80, false));

        assert!(rows[0].contains(&std::format!("   {word} ")), "{word}: {}", rows[0]);
        assert!(rows[1].contains("REAL "), "{}", rows[1]);
    }
}

#[test]
fn a_row_with_no_verdict_yet_leaves_the_verdict_column_blank() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = tracked(&mut arena, &mut ledger);
    settle(&mut state, SCHED, Outcome::Failed, 71_660);
    keys(&mut state, &[Key::Char(b'f')]);

    let rows = list_rows(&frame_of(&state, 80, false));

    assert_eq!(rows.len(), 1);
    assert!(rows[0].starts_with("\u{203a}TIMEOUT    71.66s   sched_records::the_host_binar"), "{}", rows[0]);
}

#[test]
fn keys_become_actions_the_driver_executes_later() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("list", &mut arena, &mut ledger);
    keys(&mut state, &[Key::Char(b'j')]);
    assert_eq!(state.action(Key::Char(b'r'), detail), Action::RerunOne { id: SHARD });
    assert_eq!(state.action(Key::Char(b'R'), detail), Action::RerunFailed);
    assert_eq!(state.action(Key::Char(b'e'), detail), Action::OpenEditor { file: b"tests/shard_process.rs", line: 575 });
    assert_eq!(state.action(Key::Char(b'c'), detail), Action::Interrupt);

    state.apply(&Event::Reran { id: SCHED, rerun: Rerun::Running { elapsed_ms: 1, expected_ms: None } });
    state.apply(&Event::Reran { id: SHARD, rerun: Rerun::Running { elapsed_ms: 1, expected_ms: None } });

    assert_eq!(state.action(Key::Char(b'r'), detail), Action::None, "a running rerun is not started twice");
    assert_eq!(state.action(Key::Char(b'R'), detail), Action::None);
}

#[test]
fn the_quit_receipt_is_the_plain_closing_block_whatever_the_width_or_colour() {
    let mut arena = [0_u8; 1024];
    let mut ledger = [0_u32; 4];
    let mut state = State::<400>::new(&mut arena, &mut ledger);
    for id in [SCHED, SHARD] {
        state.apply(&Event::Settled { id, outcome: Outcome::Failed, at_ms: Some(1), solo: false });
    }
    let finished = Event::Finished { passed: 381, failed: 2, ignored: 0, not_run: 0, wall_ms: 138_205, manifest: Some(b"/w/Cargo.toml"), discovered: 0, filtered: 0, exit: 1 };
    for key in *b"fqq" {
        state.apply(&Event::Key(Key::Char(key)));
    }
    let expected = "SUMMARY  passed=381 failed=2 ignored=0 not_run=0  wall=138.205s\n\
        FAILED   sched_records the_host_binary_format_yields_every_declared_record_with_flags_and_group\n\
        FAILED   shard_process a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly\n\
        RERUN    cargo val --manifest-path '/w/Cargo.toml' --no-fail-fast -E '(binary(=sched_records) & test(=the_host_binary_format_yields_every_declared_record_with_flags_and_group)) | (binary(=shard_process) & test(=a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly))'\n";
    let mut frame = Line::<8192>::new();

    state.project(&finished, &mut frame).expect("the closing block fits");

    assert_eq!(state.mode(), Mode::Receipt);
    assert_eq!(String::from_utf8_lossy(frame.bytes()), expected);
    assert!(!frame.bytes().contains(&0x1b), "the receipt is plain bytes in every mode");
}

#[test]
fn ten_thousand_decodes_lane_chunks_projections_and_details_allocate_nothing() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = scenario("rerun_running", &mut arena, &mut ledger);
    let mut frame = Line::<8192>::new();
    let pressed = [Key::Char(b'j'), Key::Char(b'k'), Key::Char(b'q'), Key::Esc, Key::Down, Key::Up, Key::Enter, Key::Char(b'j'), Key::Char(b'r'), Key::Enter];
    let reads: [&[u8]; 5] = [b"\x1b", b"[A", b"\x1b", b"q", b"j\x1b[B"];
    let (mut frames, mut events, mut decodes, mut keys_seen, mut projections, mut details, mut detail_bytes) = (0_usize, 0_usize, 0_usize, 0_usize, 0_usize, 0_usize, 0_usize);
    let mut carry = 0_u8;

    let ((), allocations) = measure_allocations(|| {
        for round in 0..10_000_usize {
            frame.clear();
            state.screen(detail, WIDTHS[round % 3], round % 2 == 0, &mut frame).expect("the frame holds a review screen");
            frames += 1;
            let key = pressed[round % pressed.len()];
            _ = state.action(key, detail);
            state.apply(&Event::Key(key));
            state.apply(&Event::Tick { now_ms: CLOCK_MS + round as u64 });
            events += 1;

            let outcome = Key::decode(&mut carry, reads[round % reads.len()], |key| {
                keys_seen += 1;
                _ = state.action(key, detail);
                state.apply(&Event::Key(key));
                Ok::<(), ()>(())
            });
            assert_eq!(outcome, Ok(()));
            decodes += 1;

            frame.clear();
            state.project_live(&Event::Key(key), &mut frame).expect("the frame holds a review projection");
            projections += 1;

            let (_, _, _, output) = state.detail(if round % 2 == 0 { SCHED } else { SHARD });
            detail_bytes += output.len();
            details += 1;
        }
    });

    assert_eq!((frames, events, decodes, projections, details), (10_000, 10_000, 10_000, 10_000, 10_000), "every loop ran, so a zero is not an empty loop");
    assert_eq!(keys_seen, 10_000, "five split reads yield five keys, so 10k rounds yield 10k keys through the carry");
    assert_eq!(detail_bytes, 5_000 * SCHED_OUTPUT.len() + 5_000 * SHARD_OUTPUT.len(), "the real detail returned the retained capture each time");
    assert_eq!(allocations, 0);
    assert!(frame.bytes().len() > 200, "the last projection is a drawn screen");
}

fn decoded(carry: &mut u8, bytes: &[u8], keys: &mut Vec<Key>) {
    Key::decode(carry, bytes, |key| {
        keys.push(key);
        Ok::<(), ()>(())
    })
    .expect("collecting keys cannot fail");
}

fn read_in_two(bytes: &[u8], at: usize) -> Vec<Key> {
    let (mut carry, mut keys) = (0_u8, Vec::new());
    decoded(&mut carry, &bytes[..at], &mut keys);
    decoded(&mut carry, &bytes[at..], &mut keys);
    Key::flush(&mut carry, |key| {
        keys.push(key);
        Ok::<(), ()>(())
    })
    .expect("collecting keys cannot fail");
    keys
}

const KEY_TABLE: [(&[u8], &[Key]); 9] = [
    (b"j", &[Key::Char(b'j')]),
    (b"qq", &[Key::Char(b'q'), Key::Char(b'q')]),
    (b"\r", &[Key::Enter]),
    (b"\x1b", &[Key::Esc]),
    (b"\x1b[A\x1b[B", &[Key::Up, Key::Down]),
    (b"\x1bOB", &[Key::Down]),
    (b"\x03", &[Key::CtrlC]),
    (b"\x1b[1;5C\xc3\xa9\x00", &[]),
    (b"R/\x1b[Br\n", &[Key::Char(b'R'), Key::Char(b'/'), Key::Down, Key::Char(b'r'), Key::Enter]),
];

#[test]
fn terminal_reads_decode_to_the_keys_the_review_table_names() {
    for (bytes, expected) in KEY_TABLE {
        assert_eq!(read_in_two(bytes, bytes.len()), expected, "read {bytes:?}");
    }
}

#[test]
fn every_key_of_the_spec_table_decodes_the_same_split_at_every_byte_boundary() {
    let mut splits = 0;
    for (bytes, expected) in KEY_TABLE {
        for at in 0..=bytes.len() {
            assert_eq!(read_in_two(bytes, at), expected, "read {bytes:?} split at {at}");
            splits += 1;
        }
    }
    assert_eq!(splits, KEY_TABLE.iter().map(|(bytes, _)| bytes.len() + 1).sum::<usize>(), "every boundary of every row ran");
    assert_eq!(splits, 40, "the table has 40 split points");
}

#[test]
fn an_escape_alone_then_bracket_a_in_the_next_read_is_one_up_and_no_esc() {
    let (mut carry, mut keys) = (0_u8, Vec::new());
    decoded(&mut carry, b"\x1b", &mut keys);
    assert_eq!((keys.len(), carry), (0, 0x1b), "a lone escape is held, not decided");
    decoded(&mut carry, b"[A", &mut keys);
    assert_eq!((keys, carry), (vec![Key::Up], 0));
}

#[test]
fn an_escape_followed_by_q_is_esc_then_q_and_a_held_escape_is_esc_on_the_next_tick() {
    let (mut carry, mut keys) = (0_u8, Vec::new());
    decoded(&mut carry, b"\x1b", &mut keys);
    decoded(&mut carry, b"q", &mut keys);
    assert_eq!(keys, vec![Key::Esc, Key::Char(b'q')]);

    let (mut carry, mut keys) = (0_u8, Vec::new());
    decoded(&mut carry, b"\x1b", &mut keys);
    Key::flush(&mut carry, |key| {
        keys.push(key);
        Ok::<(), ()>(())
    })
    .expect("collecting keys cannot fail");
    assert_eq!((keys, carry), (vec![Key::Esc], 0), "the tick resolves the held escape");
}

#[test]
fn a_three_byte_utf8_character_split_across_reads_decodes_to_no_keys() {
    let euro = "\u{20ac}".as_bytes();
    assert_eq!(euro.len(), 3);
    for at in 0..=euro.len() {
        assert_eq!(read_in_two(euro, at), Vec::<Key>::new(), "euro split at {at}");
    }
    let (mut carry, mut keys) = (0_u8, Vec::new());
    decoded(&mut carry, &euro[..2], &mut keys);
    decoded(&mut carry, &[&euro[2..], b"j"].concat(), &mut keys);
    assert_eq!(keys, vec![Key::Char(b'j')], "the key after the split character still decodes");
}

#[test]
fn a_sink_error_stops_the_decode_and_is_returned() {
    let mut carry = 0_u8;
    let mut seen = 0;
    let outcome = Key::decode(&mut carry, b"jkl", |_| {
        seen += 1;
        if seen == 2 { Err("stop") } else { Ok(()) }
    });
    assert_eq!((outcome, seen), (Err("stop"), 2));
}

#[test]
fn the_state_keeps_each_failures_output_and_classifies_it_for_the_detail_view() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let state = run(&mut arena, &mut ledger, 1);

    assert_eq!(state.detail(SHARD), detail(SHARD), "a panic keeps its class, its line and its bytes");
    let (class, _, _, output) = state.detail(SCHED);
    assert_eq!((class, output), (Class::Timeout { enforced_by: Enforcer::Val }, SCHED_OUTPUT), "a val deadline keeps its class and its bytes");
}

const LONG_OUTPUT: &[u8] = b"\nrunning 1 test\ntest tests::settles_failing ... FAILED\n\nfailures:\n\n---- tests::settles_failing stdout ----\n\nthread 'tests::settles_failing' panicked at src/lib.rs:11:9:\nassertion `left == right` failed: expected three listeners, found [8080, 8443]\n  left: 2\n right: 3\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    tests::settles_failing\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

#[test]
fn a_long_capture_shows_the_assertion_first_then_the_lines_around_the_panic_with_both_elisions_counted() {
    let (mut arena, mut ledger) = ([0_u8; 1024], [0_u32; 8]);
    let mut state = run(&mut arena, &mut ledger, 1);
    keys(&mut state, &[Key::Char(b'f')]);
    let long = |_: Identity<'_>| -> Detail<'static> {
        let location = Location { file: b"src/lib.rs", line: 11, col: 9 };
        let message: &[u8] = b"assertion `left == right` failed: expected three listeners, found [8080, 8443]";
        (Class::Assert, Some(location), (message, Some((&b"2"[..], &b"3"[..]))), LONG_OUTPUT)
    };
    let mut frame = Line::<8192>::new();
    state.screen(long, 100, false, &mut frame).expect("the frame holds one review screen");
    let text = String::from_utf8(frame.bytes().to_vec()).expect("a review frame is utf-8");

    let reason = text.find("   left   2").expect("the left value is shown");
    let output = text.find("test output  verbatim").expect("the output section is shown");
    assert!(text.find("assertion `left == right` failed").is_some_and(|message| message < reason && reason < output), "{text}");
    assert!(text.contains("   right  3"), "{text}");
    assert!(text.contains("panicked at src/lib.rs:11:9:") && !text.contains("running 1 test"), "{text}");
    assert!(text.contains("above") && text.contains("below"), "{text}");
}
