//! The fixed-width tail of a ledger record in the name arena: what a failure's retry and rerun concluded.
//!
//! A record is `binary NUL name NUL` followed by `TAIL` bytes, so the ledger stays one `[u32]` of offsets and the arena
//! stays the only storage. Every field is little-endian at a named offset; the state reads and writes through these
//! functions and nowhere else.

use super::review::{Rerun, Verdict};

pub(super) const TAIL: usize = 38;
const VERDICT: usize = 0;
const BUDGET: usize = 1;
const RERUN: usize = 5;
const FIRST: usize = 6;
const SECOND: usize = 14;
const CLEARED: usize = 22;
const LINES: usize = 30;

fn get(tail: &[u8], at: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&tail[at..at + 8]);
    u64::from_le_bytes(word)
}

fn put(tail: &mut [u8], at: usize, value: u64) {
    tail[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub(super) fn lines(tail: &[u8]) -> usize {
    usize::try_from(get(tail, LINES)).unwrap_or(usize::MAX)
}

pub(super) fn set_lines(tail: &mut [u8], lines: usize) {
    put(tail, LINES, u64::try_from(lines).unwrap_or(u64::MAX));
}

pub(super) fn cleared_at(tail: &[u8]) -> u64 {
    get(tail, CLEARED)
}

pub(super) fn verdict(tail: &[u8]) -> Option<Verdict> {
    match tail[VERDICT] {
        1 => Some(Verdict::Real),
        2 => Some(Verdict::Flaky),
        3 => Some(Verdict::OrderDependent),
        4 => {
            let mut word = [0_u8; 4];
            word.copy_from_slice(&tail[BUDGET..BUDGET + 4]);
            Some(Verdict::NotRetried { budget_secs: u32::from_le_bytes(word) })
        }
        _ => None,
    }
}

pub(super) fn set_verdict(tail: &mut [u8], verdict: Verdict) {
    let (code, budget) = match verdict {
        Verdict::Real => (1, 0),
        Verdict::Flaky => (2, 0),
        Verdict::OrderDependent => (3, 0),
        Verdict::NotRetried { budget_secs } => (4, budget_secs),
    };
    tail[VERDICT] = code;
    tail[BUDGET..BUDGET + 4].copy_from_slice(&budget.to_le_bytes());
}

pub(super) fn rerun(tail: &[u8]) -> Rerun {
    match tail[RERUN] {
        1 => Rerun::Running { elapsed_ms: get(tail, FIRST), expected_ms: get(tail, SECOND).checked_sub(1) },
        2 => Rerun::Passed { millis: get(tail, FIRST) },
        _ => Rerun::Idle,
    }
}

pub(super) fn set_rerun(tail: &mut [u8], rerun: Rerun, now_ms: u64) {
    match rerun {
        Rerun::Idle => tail[RERUN] = 0,
        Rerun::Running { elapsed_ms, expected_ms } => {
            tail[RERUN] = 1;
            put(tail, FIRST, elapsed_ms);
            put(tail, SECOND, expected_ms.map_or(0, |expected| expected.saturating_add(1)));
        }
        Rerun::Passed { millis } => {
            tail[RERUN] = 2;
            put(tail, FIRST, millis);
            put(tail, CLEARED, now_ms);
        }
    }
}
