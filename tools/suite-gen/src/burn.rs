use std::hint::black_box;
use std::time::Instant;

pub use crate::generated_lib::mix;

const CALIBRATION_ROUNDS: u32 = 20_000_000;
const CALIBRATION_RUNS: u32 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    pub iters_per_ms: f64,
    pub rounds: u32,
    pub origin: &'static str,
    pub generator_profile: &'static str,
}

impl Calibration {
    #[must_use]
    pub fn fixed(iters_per_ms: f64) -> Self {
        Self { iters_per_ms, rounds: 0, origin: "override", generator_profile: profile() }
    }
}

#[must_use]
pub fn profile() -> &'static str {
    if cfg!(debug_assertions) { "debug" } else { "release" }
}

/// best of several runs: scheduler noise only ever slows a run down
#[must_use]
pub fn calibrate() -> Calibration {
    black_box(mix(black_box(1), u64::from(CALIBRATION_ROUNDS / 10)));
    let fastest = (0..CALIBRATION_RUNS)
        .map(|_| {
            let start = Instant::now();
            black_box(mix(black_box(7), u64::from(CALIBRATION_ROUNDS)));
            start.elapsed().as_secs_f64()
        })
        .fold(f64::INFINITY, f64::min);
    Calibration {
        iters_per_ms: f64::from(CALIBRATION_ROUNDS) / (fastest * 1000.0),
        rounds: CALIBRATION_ROUNDS,
        origin: "measured",
        generator_profile: profile(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_measures_a_positive_rate() {
        let calibration = calibrate();
        assert!(calibration.iters_per_ms > 1000.0, "rate {}", calibration.iters_per_ms);
        assert_eq!(calibration.origin, "measured");
    }

    #[test]
    fn mix_is_deterministic_and_input_sensitive() {
        assert_eq!(mix(3, 1000), mix(3, 1000));
        assert_ne!(mix(3, 1000), mix(3, 1001));
    }
}
