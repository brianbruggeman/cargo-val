//! Estimates of the time a run has left, as pure arithmetic over milliseconds and counts so every rule is testable with exact numbers.
//!
//! The build phase counts compile units; the test phase counts settled tests. Both return `None` until the data supports an estimate and
//! never return zero while work remains.

/// Arrivals older than this, measured back from the newest one, no longer shape the build estimate.
pub const WINDOW_MS: u64 = 5_000;
/// Fewest arrivals inside the window before the build rate is trusted.
pub const MIN_WINDOW_ARRIVALS: usize = 3;
/// Share of the expected tests that must have settled before their own rate is trusted, in percent.
pub const SETTLED_PERCENT: usize = 10;
/// The smallest estimate shown while work remains.
pub const FLOOR_MS: u64 = 1_000;

fn floored(remaining_ms: u64) -> u64 {
    remaining_ms.max(FLOOR_MS)
}

/// Build time left from arrivals `(elapsed_ms, done)`, oldest first: each arrival implies `elapsed / done * remaining`, and the estimate is the mean of
/// those implied values over the arrivals inside [`WINDOW_MS`] of the newest, so one slow early unit stops mattering after five seconds.
#[must_use]
pub fn build_remaining_ms(arrivals: &[(u32, u16)], total: u16) -> Option<u64> {
    let (newest_ms, newest_done) = *arrivals.last()?;
    if total == 0 || newest_done >= total {
        return None;
    }
    let oldest_ms = u64::from(newest_ms).saturating_sub(WINDOW_MS);
    let (sum, count) = arrivals
        .iter()
        .filter(|(elapsed_ms, done)| u64::from(*elapsed_ms) >= oldest_ms && *done > 0)
        .fold((0_u64, 0_usize), |(sum, count), (elapsed_ms, done)| {
            let remaining = u64::from(total.saturating_sub(*done));
            (sum.saturating_add(u64::from(*elapsed_ms).saturating_mul(remaining) / u64::from(*done)), count + 1)
        });
    (count >= MIN_WINDOW_ARRIVALS).then(|| floored(sum / count as u64))
}

/// Build time left from a recorded build of the same inputs: the recorded wall time scaled by the share of units still to go.
#[must_use]
pub fn recorded_build_remaining_ms(recorded_build_ms: u64, done: u16, total: u16) -> Option<u64> {
    if total == 0 || done >= total || recorded_build_ms == 0 {
        return None;
    }
    Some(floored(recorded_build_ms.saturating_mul(u64::from(total - done)) / u64::from(total)))
}

/// Test time left. With a previous run (`previous_test_ms` over `previous_tests` tests) it is that run's time per test times the tests still to go;
/// without one it is this run's own time per settled test, once at least [`SETTLED_PERCENT`] of `expected` have settled.
#[must_use]
pub fn test_remaining_ms(settled: usize, expected: usize, (elapsed_ms, previous): (u64, Option<(u64, u32)>)) -> Option<u64> {
    let remaining = u64::try_from(expected.saturating_sub(settled)).ok().filter(|left| *left > 0)?;
    if let Some((previous_test_ms, previous_tests)) = previous.filter(|(millis, tests)| *millis > 0 && *tests > 0) {
        return Some(floored(previous_test_ms.saturating_mul(remaining) / u64::from(previous_tests)));
    }
    if settled == 0 || settled * 100 < expected * SETTLED_PERCENT {
        return None;
    }
    Some(floored(elapsed_ms.saturating_mul(remaining) / settled as u64))
}

/// Renders `eta ~m:ss`; the tilde is always present because the figure is an estimate.
pub fn write_eta<const FRAME: usize>(frame: &mut crate::line::Line<FRAME>, remaining_ms: u64) {
    let seconds = remaining_ms.div_ceil(1000);
    frame.text(b"eta ~");
    frame.number(usize::try_from(seconds / 60).unwrap_or(usize::MAX));
    frame.text(b":");
    if seconds % 60 < 10 {
        frame.text(b"0");
    }
    frame.number(usize::try_from(seconds % 60).unwrap_or(0));
}

#[cfg(test)]
mod tests {
    use super::{build_remaining_ms, recorded_build_remaining_ms, test_remaining_ms, write_eta};
    use crate::line::Line;

    #[test]
    fn the_build_estimate_is_the_mean_of_the_implied_estimates_inside_the_window() {
        let arrivals = [(10_000, 100), (11_000, 110), (12_000, 120)];

        assert_eq!(build_remaining_ms(&arrivals, 200), Some((10_000 * 100 / 100 + 11_000 * 90 / 110 + 12_000 * 80 / 120) / 3));
    }

    #[test]
    fn an_early_slow_arrival_stops_shaping_the_estimate_after_the_window() {
        let arrivals = [(1_000, 1), (20_000, 200), (21_000, 220), (22_000, 240)];

        assert_eq!(build_remaining_ms(&arrivals, 300), Some((20_000 * 100 / 200 + 21_000 * 80 / 220 + 22_000 * 60 / 240) / 3));
    }

    #[test]
    fn fewer_than_three_arrivals_in_the_window_show_nothing() {
        assert_eq!(build_remaining_ms(&[(5_000, 10), (6_000, 20)], 100), None);
        assert_eq!(build_remaining_ms(&[], 100), None);
    }

    #[test]
    fn a_finished_or_empty_build_has_no_remaining_time() {
        assert_eq!(build_remaining_ms(&[(1, 5), (2, 6), (3, 7)], 7), None);
        assert_eq!(build_remaining_ms(&[(1, 5), (2, 6), (3, 7)], 0), None);
    }

    #[test]
    fn the_estimate_never_drops_below_one_second_while_units_remain() {
        assert_eq!(build_remaining_ms(&[(10, 98), (11, 99), (12, 99)], 100), Some(1_000));
        assert_eq!(recorded_build_remaining_ms(10, 1, 100), Some(1_000));
    }

    #[test]
    fn a_recorded_build_scales_by_the_share_of_units_still_to_go() {
        assert_eq!(recorded_build_remaining_ms(120_000, 0, 400), Some(120_000));
        assert_eq!(recorded_build_remaining_ms(120_000, 100, 400), Some(90_000));
        assert_eq!(recorded_build_remaining_ms(120_000, 400, 400), None);
        assert_eq!(recorded_build_remaining_ms(0, 1, 400), None);
    }

    #[test]
    fn the_previous_run_gives_the_test_estimate_before_any_test_settles() {
        assert_eq!(test_remaining_ms(0, 383, (0, Some((100_000, 383)))), Some(100_000));
        assert_eq!(test_remaining_ms(100, 383, (30_000, Some((100_000, 383)))), Some(73_890));
    }

    #[test]
    fn without_a_previous_run_the_own_rate_counts_only_after_ten_percent_have_settled() {
        assert_eq!(test_remaining_ms(38, 383, (10_000, None)), None);
        assert_eq!(test_remaining_ms(39, 383, (10_000, None)), Some(10_000 * 344 / 39));
        assert_eq!(test_remaining_ms(0, 383, (10_000, None)), None);
    }

    #[test]
    fn no_tests_remaining_means_no_estimate_instead_of_zero() {
        assert_eq!(test_remaining_ms(383, 383, (50_000, Some((100_000, 383)))), None);
        assert_eq!(test_remaining_ms(400, 383, (50_000, None)), None);
    }

    #[test]
    fn the_text_is_eta_tilde_minutes_colon_two_digit_seconds() {
        let mut frame = Line::<32>::new();
        write_eta(&mut frame, 183_000);
        assert_eq!(frame.bytes(), b"eta ~3:03");
        let mut frame = Line::<32>::new();
        write_eta(&mut frame, 59_001);
        assert_eq!(frame.bytes(), b"eta ~1:00");
    }
}
