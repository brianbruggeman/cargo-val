use crate::config::Config;
use crate::error::SuiteError;
use crate::rng::SplitMix;

const SLOW_JITTER_SPREAD: f64 = 0.2;

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTest {
    pub crate_index: u32,
    pub binary_index: u32,
    pub test_index: u32,
    pub planned_ms: f64,
    pub iterations: u64,
    pub slow: bool,
    pub mix_seed: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub tests: Vec<PlannedTest>,
    pub effective_median_ms: f64,
    pub total_ms: f64,
}

struct Draw {
    tail_factor: f64,
    slow: bool,
    slow_ms: f64,
    mix_seed: u64,
}

/// log-normal with the given median: median * exp(sigma * z)
pub fn lognormal_ms(rng: &mut SplitMix, median_ms: f64, sigma: f64) -> f64 {
    median_ms * (sigma * rng.next_normal()).exp()
}

fn draw(rng: &mut SplitMix, config: &Config) -> Draw {
    let tail_factor = (config.tail_sigma * rng.next_normal()).exp();
    let slow = rng.next_unit() < config.slow_fraction;
    let jitter = 1.0 + SLOW_JITTER_SPREAD * (rng.next_unit() - 0.5);
    Draw { tail_factor, slow, slow_ms: config.slow_ms * jitter, mix_seed: rng.next_u64() }
}

const U64_RANGE_END: f64 = 18_446_744_073_709_551_616.0;

/// no `as` cast: a float is rendered as a whole decimal and parsed back, so out-of-range is a typed error
fn iterations_for(planned_ms: f64, iters_per_ms: f64) -> Result<u64, SuiteError> {
    let rounded = (planned_ms * iters_per_ms).round();
    if !rounded.is_finite() || rounded < 0.0 || rounded >= U64_RANGE_END {
        return Err(SuiteError::IterationCount { value: rounded });
    }
    let whole = format!("{:.0}", rounded.abs());
    let count: u64 = whole.parse().map_err(|_| SuiteError::IterationCount { value: rounded })?;
    Ok(count.max(1))
}

fn effective_median(config: &Config, draws: &[Draw]) -> Result<f64, SuiteError> {
    let Some(target_secs) = config.target_serial_secs else {
        return Ok(config.median_ms);
    };
    let slow_total: f64 = draws.iter().filter(|each| each.slow).map(|each| each.slow_ms).sum();
    let tail_total: f64 = draws.iter().filter(|each| !each.slow).map(|each| each.tail_factor).sum();
    let median = (target_secs * 1000.0 - slow_total) / tail_total;
    if tail_total > 0.0 && median.is_finite() && median > 0.0 {
        Ok(median)
    } else {
        Err(SuiteError::Infeasible(format!(
            "slow tests alone plan {slow_total:.0} ms against a {:.0} ms target",
            target_secs * 1000.0
        )))
    }
}

/// # Errors
/// `Infeasible` when the target cannot be met; `IterationCount` when an iteration count leaves u64.
pub fn build_plan(config: &Config, iters_per_ms: f64) -> Result<Plan, SuiteError> {
    let count = config.total_tests()?;
    let mut rng = SplitMix::new(config.seed);
    let draws: Vec<Draw> = (0..count).map(|_| draw(&mut rng, config)).collect();
    let median = effective_median(config, &draws)?;
    let mut tests = Vec::new();
    let mut draws_iter = draws.iter();
    for crate_index in 0..config.crates {
        for binary_index in 0..config.binaries_per_crate {
            for test_index in 0..config.tests_per_binary {
                let entry = draws_iter.next().ok_or_else(|| SuiteError::Infeasible("draw underflow".to_string()))?;
                let planned_ms = if entry.slow { entry.slow_ms } else { median * entry.tail_factor };
                tests.push(PlannedTest {
                    crate_index,
                    binary_index,
                    test_index,
                    planned_ms,
                    iterations: iterations_for(planned_ms, iters_per_ms)?,
                    slow: entry.slow,
                    mix_seed: entry.mix_seed,
                });
            }
        }
    }
    let total_ms = tests.iter().map(|each| each.planned_ms).sum();
    Ok(Plan { tests, effective_median_ms: median, total_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterations_for_rejects_non_finite_negative_and_overflow() {
        assert_eq!(iterations_for(2.0, 1000.4).expect("finite"), 2001);
        assert_eq!(iterations_for(0.0, 5.0).expect("floor of one"), 1);
        for bad in [f64::NAN, f64::INFINITY, -3.0, 1e30] {
            assert!(matches!(iterations_for(bad, 1.0), Err(SuiteError::IterationCount { .. })), "{bad}");
        }
    }
    use std::path::PathBuf;

    fn config() -> Config {
        Config::with_defaults(PathBuf::from("unused"))
    }

    fn sorted(mut values: Vec<f64>) -> Vec<f64> {
        values.sort_by(f64::total_cmp);
        values
    }

    #[test]
    fn lognormal_median_and_p90_match_theory_on_200k_samples() {
        let (median, sigma) = (20.0, 0.8);
        let mut rng = SplitMix::new(99);
        let samples = sorted((0..200_000).map(|_| lognormal_ms(&mut rng, median, sigma)).collect());
        let observed_median = samples[samples.len() / 2];
        let observed_p90 = samples[samples.len() * 9 / 10];
        let expected_p90 = median * (1.281_551_6 * sigma).exp();
        assert!((observed_median / median - 1.0).abs() < 0.02, "median {observed_median}");
        assert!((observed_p90 / expected_p90 - 1.0).abs() < 0.03, "p90 {observed_p90} vs {expected_p90}");
    }

    #[test]
    fn target_serial_secs_scales_the_total() {
        let mut target = config();
        target.target_serial_secs = Some(20.0);
        target.slow_fraction = 0.05;
        target.slow_ms = 10.0;
        target.tests_per_binary = 50;
        let plan = build_plan(&target, 1000.0).expect("feasible plan");
        assert!((plan.total_ms - 20_000.0).abs() < 1e-6, "total {}", plan.total_ms);
        assert_eq!(plan.tests.len(), 600);
    }

    #[test]
    fn slow_fraction_selects_roughly_that_share() {
        let mut heavy = config();
        heavy.slow_fraction = 0.1;
        heavy.tests_per_binary = 500;
        let plan = build_plan(&heavy, 1000.0).expect("feasible plan");
        let slow = plan.tests.iter().filter(|each| each.slow).count();
        assert!((450..=750).contains(&slow), "slow {slow} of {}", plan.tests.len());
    }

    #[test]
    fn slow_tests_exceeding_the_target_are_infeasible() {
        let mut impossible = config();
        impossible.target_serial_secs = Some(1.0);
        impossible.slow_fraction = 1.0;
        assert!(build_plan(&impossible, 1000.0).is_err());
    }
}
