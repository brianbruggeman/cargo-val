use std::path::PathBuf;
use std::str::FromStr;

use crate::error::SuiteError;

const MAX_TESTS: u64 = 1_000_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub out: PathBuf,
    pub crates: u32,
    pub binaries_per_crate: u32,
    pub tests_per_binary: u32,
    pub seed: u64,
    pub median_ms: f64,
    pub tail_sigma: f64,
    pub slow_fraction: f64,
    pub slow_ms: f64,
    pub target_serial_secs: Option<f64>,
    pub iters_per_ms: Option<f64>,
}

impl Config {
    #[must_use]
    pub fn with_defaults(out: PathBuf) -> Self {
        Self {
            out,
            crates: 3,
            binaries_per_crate: 4,
            tests_per_binary: 10,
            seed: 1,
            median_ms: 20.0,
            tail_sigma: 0.8,
            slow_fraction: 0.0,
            slow_ms: 1000.0,
            target_serial_secs: None,
            iters_per_ms: None,
        }
    }

    /// total test count; bounded so a typo cannot emit millions of files
    /// # Errors
    /// `Usage` when the total is zero or above the cap.
    pub fn total_tests(&self) -> Result<u64, SuiteError> {
        let total = u64::from(self.crates) * u64::from(self.binaries_per_crate) * u64::from(self.tests_per_binary);
        if total == 0 || total > MAX_TESTS {
            return Err(SuiteError::Usage(format!("total tests {total} outside 1..={MAX_TESTS}")));
        }
        Ok(total)
    }
}

fn value<T: FromStr>(flag: &str, raw: Option<&String>) -> Result<T, SuiteError> {
    let text = raw.ok_or_else(|| SuiteError::Usage(format!("{flag} needs a value")))?;
    text.parse().map_err(|_| SuiteError::Usage(format!("{flag}: cannot parse '{text}'")))
}

/// # Errors
/// `Usage` on an unknown flag, a missing or unparsable value, or invalid ranges.
pub fn parse(args: &[String]) -> Result<Config, SuiteError> {
    let mut config = Config::with_defaults(PathBuf::new());
    let mut out_given = false;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let raw = iter.next();
        match flag.as_str() {
            "--out" => {
                config.out = PathBuf::from(value::<String>(flag, raw)?);
                out_given = true;
            }
            "--crates" => config.crates = value(flag, raw)?,
            "--binaries-per-crate" => config.binaries_per_crate = value(flag, raw)?,
            "--tests-per-binary" => config.tests_per_binary = value(flag, raw)?,
            "--seed" => config.seed = value(flag, raw)?,
            "--median-ms" => config.median_ms = value(flag, raw)?,
            "--tail-sigma" => config.tail_sigma = value(flag, raw)?,
            "--slow-fraction" => config.slow_fraction = value(flag, raw)?,
            "--slow-ms" => config.slow_ms = value(flag, raw)?,
            "--target-serial-secs" => config.target_serial_secs = Some(value(flag, raw)?),
            "--iters-per-ms" => config.iters_per_ms = Some(value(flag, raw)?),
            other => return Err(SuiteError::Usage(format!("unknown flag {other}"))),
        }
    }
    if !out_given {
        return Err(SuiteError::Usage("--out <dir> is required".to_string()));
    }
    validate(&config)?;
    Ok(config)
}

fn validate(config: &Config) -> Result<(), SuiteError> {
    config.total_tests()?;
    let positive = |name: &str, number: f64| {
        if number.is_finite() && number > 0.0 {
            Ok(())
        } else {
            Err(SuiteError::Usage(format!("{name} must be finite and > 0")))
        }
    };
    positive("--median-ms", config.median_ms)?;
    positive("--slow-ms", config.slow_ms)?;
    config.target_serial_secs.map_or(Ok(()), |secs| positive("--target-serial-secs", secs))?;
    config.iters_per_ms.map_or(Ok(()), |rate| positive("--iters-per-ms", rate))?;
    if !(config.tail_sigma.is_finite() && config.tail_sigma >= 0.0) {
        return Err(SuiteError::Usage("--tail-sigma must be finite and >= 0".to_string()));
    }
    if !(0.0..=1.0).contains(&config.slow_fraction) {
        return Err(SuiteError::Usage("--slow-fraction must be within 0..=1".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn parses_the_documented_flags() {
        let config = parse(&args(&[
            "--out", "x", "--crates", "12", "--binaries-per-crate", "8", "--tests-per-binary", "60",
            "--target-serial-secs", "900", "--slow-fraction", "0.01", "--slow-ms", "20000",
        ]))
        .expect("documented flags parse");
        assert_eq!((config.crates, config.binaries_per_crate, config.tests_per_binary), (12, 8, 60));
        assert_eq!(config.target_serial_secs, Some(900.0));
    }

    #[test]
    fn rejects_missing_out_unknown_flag_and_bad_fraction() {
        assert!(parse(&args(&["--crates", "2"])).is_err());
        assert!(parse(&args(&["--out", "x", "--bogus", "1"])).is_err());
        assert!(parse(&args(&["--out", "x", "--slow-fraction", "1.5"])).is_err());
        assert!(parse(&args(&["--out", "x", "--crates", "0"])).is_err());
    }
}
