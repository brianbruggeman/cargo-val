use std::fs;
use std::path::Path;
use std::string::String;
use std::vec::Vec;

use super::{Action, Back, Key, Rerun, Review, Row, Verdict, action};
use crate::failure_class::{Class, Enforcer, Location};
use crate::line::Line;
use crate::render::{Event, Mode, Outcome, State};
use crate::test_alloc::measure_allocations;

const SCHED: (&[u8], &[u8]) = (b"sched_records", b"the_host_binary_format_yields_every_declared_record_with_flags_and_group");
const SHARD: (&[u8], &[u8]) = (b"shard_process", b"a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly");
const SHARD_OUTPUT: &[u8] = b"thread 'a_per_test_interrupt_ends_a_blocked_child_and_cargo_val_exits_promptly' panicked at tests/shard_process.rs:575:31:\nthe shard child reported its pid before the deadline\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
const WIDTHS: [usize; 3] = [80, 60, 40];

fn sched<'a>(rerun: Rerun) -> Row<'a> {
    Row {
        id: SCHED,
        class: Class::Timeout { enforced_by: Enforcer::Val },
        millis: 71_660,
        location: Some(Location { file: b"tests/sched_records.rs", line: 55, col: 1 }),
        output: b"proxima::test: body did not complete within the timeout\n",
        verdict: Some(Verdict::NotRetried { budget_secs: 30 }),
        rerun,
    }
}

fn shard<'a>() -> Row<'a> {
    Row {
        id: SHARD,
        class: Class::Panic,
        millis: 120_030,
        location: Some(Location { file: b"tests/shard_process.rs", line: 575, col: 31 }),
        output: SHARD_OUTPUT,
        verdict: Some(Verdict::NotRetried { budget_secs: 30 }),
        rerun: Rerun::Idle,
    }
}

fn review<'a>(rows: &'a [Row<'a>]) -> Review<'a> {
    Review { rows, passed: 381, skipped: 0, warnings: 0, clock_ms: 138_205 }
}

fn frame_of(review: &Review<'_>, mode: Mode, width: usize, color: bool) -> String {
    let mut frame = Line::<8192>::new();
    review.project(mode, width, color, &mut frame).expect("the frame holds one review screen");
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
    from.next(&Event::Key(key), open)
}

#[test]
fn golden_frames_at_every_width_with_colour_and_without() {
    let running_row = sched(Rerun::Running { elapsed_ms: 1_200, expected_ms: Some(8_400) });
    let both = [sched(Rerun::Idle), shard()];
    let running = [running_row, shard()];
    let passed = [sched(Rerun::Passed { millis: 8_400 }), shard()];
    let cases: [(&str, &[Row<'_>], u16); 5] =
        [("list", &both, 0), ("detail", &both, 1), ("rerun_running", &running, 0), ("rerun_passed", &passed, 0), ("empty", &[], 0)];
    let mut frames = 0;
    for (name, rows, selected) in cases {
        for width in WIDTHS {
            for (color, tag) in [(true, "color"), (false, "plain")] {
                let text = frame_of(&review(rows), Mode::Review { selected, armed: false }, width, color);
                golden(&std::format!("{name}_{width}_{tag}.txt"), &text);
                frames += 1;
            }
        }
    }
    assert_eq!(frames, 30, "5 frames x 3 widths x 2 colour settings were compared");
}

#[test]
fn no_color_frames_carry_no_escape_and_no_frame_exceeds_its_width() {
    let rows = [sched(Rerun::Running { elapsed_ms: 900, expected_ms: None }), shard()];
    let mut checked = 0;
    for width in WIDTHS {
        for selected in 0..2 {
            for armed in [false, true] {
                let plain = frame_of(&review(&rows), Mode::Review { selected, armed }, width, false);
                let colored = frame_of(&review(&rows), Mode::Review { selected, armed }, width, true);
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
    let rows = [shard()];

    let text = frame_of(&review(&rows), Mode::Review { selected: 0, armed: false }, 80, false);

    let lines: Vec<&str> = text.lines().collect();
    let hint = lines.iter().position(|line| line.starts_with("find the commit")).expect("a bisect hint");
    let reason = lines.iter().position(|line| line.starts_with("NO-RETRY")).expect("a no-retry reason");
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
        ("not retried", Some(Verdict::NotRetried { budget_secs: 30 })),
    ];

    for (name, verdict) in verdicts {
        let rows = [Row { verdict, ..shard() }];
        let text = frame_of(&review(&rows), Mode::Review { selected: 0, armed: false }, 80, false);
        if text.contains("find the commit") {
            shown.push(name);
        }
        assert!(!text.contains("B "), "{name}: the keys line has no B key");
    }

    assert_eq!(shown, ["real", "not retried"]);
}

#[test]
fn a_list_name_that_does_not_fit_keeps_its_head_and_its_tail() {
    let rows = [sched(Rerun::Idle)];

    let text = frame_of(&review(&rows), Mode::Review { selected: 0, armed: false }, 60, false);

    let row = text.lines().find(|line| line.starts_with('\u{203a}')).expect("the selected row");
    assert!(row.contains("sched_records::the_"), "{row}");
    assert!(row.ends_with("flags_and_group"), "{row}");
    assert!(row.contains('\u{2026}'), "{row}");
}

#[test]
fn q_once_arms_and_does_not_quit_while_q_twice_quits_and_esc_stays_in_review() {
    let review_mode = Mode::Review { selected: 0, armed: false };
    let rows = [sched(Rerun::Idle)];

    let armed = key_mode(review_mode, Key::Char(b'q'), 1);
    let quit = key_mode(armed, Key::Char(b'q'), 1);

    assert_eq!(armed, Mode::Review { selected: 0, armed: true });
    assert_eq!(action(review_mode, Key::Char(b'q'), &rows), Action::None, "one q writes nothing");
    assert_eq!(key_mode(armed, Key::Esc, 1), review_mode, "esc stays in review and disarms");
    assert_eq!(key_mode(review_mode, Key::Esc, 1), review_mode);
    assert_eq!(quit, Mode::Receipt);
    assert_eq!(action(armed, Key::Char(b'q'), &rows), Action::Quit);
    assert!(frame_of(&review(&rows), armed, 80, false).contains("q again to confirm, any key cancels"));
}

#[test]
fn every_transition_of_the_spec_table_lands_where_it_says() {
    let review_at = |selected, armed| Mode::Review { selected, armed };
    let (running, back_review) = (Back::Running, Back::Review { selected: 1 });
    let interrupted = Mode::Interrupted { signal: 2 };
    let rows: [(Mode, Key, usize, Mode); 40] = [
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
        (review_at(1, false), Key::Enter, 2, review_at(1, false)),
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
fn the_run_events_move_the_screens_the_way_the_spec_table_says() {
    let finished = |failed| Event::Finished { passed: 1, failed, ignored: 0, not_run: 0, wall_ms: 5, manifest: None };
    let build = Event::BuildFailed { diagnostic: b"E0308" };
    let signal = Event::Interrupted { signal: 15 };
    let review_mode = Mode::Review { selected: 1, armed: false };
    let rows = [
        (Mode::Running, finished(0), Mode::Done { wall_ms: 5 }),
        (Mode::Running, finished(2), Mode::Running),
        (review_mode, finished(0), review_mode),
        (Mode::Running, build, Mode::BuildError),
        (review_mode, build, review_mode),
        (review_mode, signal, Mode::Interrupted { signal: 15 }),
        (Mode::Timeline { back: Back::Running }, signal, Mode::Interrupted { signal: 15 }),
        (Mode::QuitArmed, signal, Mode::Interrupted { signal: 15 }),
        (Mode::Receipt, signal, Mode::Receipt),
        (Mode::Done { wall_ms: 5 }, signal, Mode::Done { wall_ms: 5 }),
    ];

    for (from, event, to) in rows {
        assert_eq!(from.next(&event, 2), to, "{from:?} on {event:?}");
    }
    assert_eq!(review_mode.next(&finished(1), 1), Mode::Review { selected: 0, armed: false }, "a shrunk list clamps the selection");
}

#[test]
fn keys_become_actions_the_driver_executes_later() {
    let rows = [sched(Rerun::Idle), shard()];
    let second = Mode::Review { selected: 1, armed: false };
    let first = Mode::Review { selected: 0, armed: false };
    let busy = [sched(Rerun::Running { elapsed_ms: 1, expected_ms: None })];

    assert_eq!(action(second, Key::Char(b'r'), &rows), Action::RerunOne { id: SHARD });
    assert_eq!(action(second, Key::Char(b'R'), &rows), Action::RerunFailed);
    assert_eq!(action(second, Key::Char(b'e'), &rows), Action::OpenEditor { file: b"tests/shard_process.rs", line: 575 });
    assert_eq!(action(first, Key::Char(b'r'), &busy), Action::None, "a running rerun is not started twice");
    assert_eq!(action(first, Key::Char(b'R'), &busy), Action::None);
    assert_eq!(action(Mode::Running, Key::CtrlC, &rows), Action::Interrupt);
    assert_eq!(action(second, Key::Char(b'c'), &rows), Action::Interrupt);
    assert_eq!(action(Mode::QuitArmed, Key::Char(b'q'), &rows), Action::Quit);
    assert_eq!(action(Mode::Receipt, Key::Char(b'q'), &rows), Action::None);
}

#[test]
fn the_quit_receipt_is_the_plain_closing_block_whatever_the_width_or_colour() {
    let mut arena = [0_u8; 512];
    let mut ledger = [0_u32; 4];
    let mut state = State::<400>::new(&mut arena, &mut ledger);
    for id in [SCHED, SHARD] {
        state.apply(&Event::Settled { id, outcome: Outcome::Failed, millis: Some(1), solo: false });
    }
    let finished = Event::Finished { passed: 381, failed: 2, ignored: 0, not_run: 0, wall_ms: 138_205, manifest: Some(b"/w/Cargo.toml") };
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
fn ten_thousand_review_frames_and_ten_thousand_key_events_allocate_nothing() {
    let rows = [sched(Rerun::Running { elapsed_ms: 1_200, expected_ms: Some(8_400) }), shard(), sched(Rerun::Idle), shard()];
    let screen = review(&rows);
    let mut frame = Line::<8192>::new();
    let keys = [Key::Char(b'j'), Key::Char(b'k'), Key::Char(b'q'), Key::Esc, Key::Down, Key::Up, Key::Char(b's'), Key::Char(b's'), Key::Char(b'r'), Key::Enter];
    let (mut frames, mut events) = (0_usize, 0_usize);
    let mut mode = Mode::Review { selected: 0, armed: false };

    let ((), allocations) = measure_allocations(|| {
        for round in 0..10_000_usize {
            frame.clear();
            let shown = Mode::Review { selected: u16::try_from(round % 4).unwrap_or(0), armed: round % 7 == 0 };
            screen.project(shown, WIDTHS[round % 3], round % 2 == 0, &mut frame).expect("the frame holds a review screen");
            frames += 1;
            let key = keys[round % keys.len()];
            _ = action(mode, key, &rows);
            mode = mode.next(&Event::Key(key), rows.len());
            events += 1;
        }
    });

    assert_eq!((frames, events), (10_000, 10_000), "the loops ran, so a zero is not an empty loop");
    assert_eq!(allocations, 0);
    assert!(frame.bytes().len() > 200, "the last frame is a drawn screen");
}
