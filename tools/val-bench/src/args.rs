use crate::arm::{ALL, Arm};
use std::path::PathBuf;

pub struct Config {
    pub fixtures: Vec<PathBuf>,
    pub iterations: u32,
    pub warmup: u32,
    pub filter: Option<String>,
    pub output: PathBuf,
    pub arms: Vec<Arm>,
    pub jobs: u32,
    pub val_jobs: u32,
    pub timeout_secs: u64,
    pub loop_scale: Option<u64>,
    pub quiet_load: f64,
    pub target_dir: PathBuf,
    pub cargo_args: Vec<String>,
}

const USAGE: &str = "usage: val-bench --fixture <crate dir> [--fixture ..] [--iterations N] [--warmup N] [--filter F] --output <jsonl> [--arms cargo-test,nextest,cargo-val] [--jobs N] [--val-jobs N] [--loop-scale N] [--quiet-load LOAD_1M] [--target-dir DIR] [--timeout-secs N] [--cargo-arg ARG ..] [--help]";

const HELP_FLAGS: [&str; 8] = [
    "  --cargo-arg ARG   repeatable; ARG is appended verbatim to every arm's cargo-level args and to the",
    "                    prebuild. cargo test: before the `--` harness separator; nextest: before the trailing",
    "                    filter; cargo val: at the end. A flag with a value takes two uses, e.g.",
    "                    --cargo-arg --workspace --cargo-arg --exclude --cargo-arg omega",
    "  --help            print this text",
    "",
    "records add first_output_ns (first byte on either pipe) and first_result_ns (first per-test result line)",
    "relative to spawn; the summary table adds median first_ms per arm.",
];

pub fn help() -> String {
    format!("val-bench: time cargo test, cargo nextest and cargo val on the same fixture\n\n{USAGE}\n\n{}\n", HELP_FLAGS.join("\n"))
}

fn value_of(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value; {USAGE}"))
}

fn number<Number: std::str::FromStr>(text: &str, flag: &str) -> Result<Number, String> {
    text.parse().map_err(|_| format!("{flag} expects a non-negative integer, got {text:?}"))
}

fn load_threshold(text: &str, flag: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|load| load.is_finite() && *load >= 0.0)
        .ok_or_else(|| format!("{flag} expects a non-negative load average, got {text:?}"))
}

fn parse_arms(text: &str) -> Result<Vec<Arm>, String> {
    text.split(',').map(|name| Arm::parse(name.trim())).collect()
}

pub fn parse(mut args: impl Iterator<Item = String>, default_target: PathBuf) -> Result<Config, String> {
    let mut fixtures = Vec::new();
    let mut iterations = 20_u32;
    let mut warmup = 1_u32;
    let mut filter = None;
    let mut output = None;
    let mut arms = ALL.to_vec();
    let mut jobs = 10_u32;
    let mut val_jobs = None;
    let mut loop_scale = None;
    let mut quiet_load = 3.0_f64;
    let mut target_dir = default_target;
    let mut timeout_secs = 180_u64;
    let mut cargo_args = Vec::new();
    while let Some(flag) = args.next() {
        let value = value_of(&mut args, &flag);
        match flag.as_str() {
            "--fixture" => fixtures.push(PathBuf::from(value?)),
            "--iterations" => iterations = number(&value?, &flag)?,
            "--warmup" => warmup = number(&value?, &flag)?,
            "--filter" => filter = Some(value?),
            "--output" => output = Some(PathBuf::from(value?)),
            "--arms" => arms = parse_arms(&value?)?,
            "--jobs" => jobs = number(&value?, &flag)?,
            "--timeout-secs" => timeout_secs = number(&value?, &flag)?,
            "--val-jobs" => val_jobs = Some(number(&value?, &flag)?),
            "--loop-scale" => loop_scale = Some(number::<u64>(&value?, &flag)?),
            "--quiet-load" => quiet_load = load_threshold(&value?, &flag)?,
            "--target-dir" => target_dir = PathBuf::from(value?),
            "--cargo-arg" => cargo_args.push(value?),
            other => return Err(format!("unknown flag {other}; {USAGE}")),
        }
    }
    if fixtures.is_empty() {
        return Err(format!("at least one --fixture is required; {USAGE}"));
    }
    if iterations == 0 {
        return Err("--iterations must be at least 1".to_string());
    }
    Ok(Config {
        fixtures,
        iterations,
        warmup,
        filter,
        output: output.ok_or_else(|| format!("--output is required; {USAGE}"))?,
        arms,
        jobs,
        val_jobs: val_jobs.unwrap_or(jobs),
        loop_scale,
        quiet_load,
        target_dir,
        timeout_secs,
        cargo_args,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> impl Iterator<Item = String> {
        text.split(' ').map(str::to_string).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_repeatable_fixture_and_defaults() {
        let config = parse(words("--fixture /a --fixture /b --output /o.jsonl"), PathBuf::from("/t")).expect("valid args");
        assert_eq!(config.fixtures.len(), 2);
        assert_eq!((config.iterations, config.warmup, config.jobs, config.val_jobs), (20, 1, 10, 10));
        assert_eq!(config.arms.len(), 3);
    }

    #[test]
    fn quiet_load_defaults_to_three_and_is_overridable() {
        let default = parse(words("--fixture /a --output /o"), PathBuf::from("/t")).expect("valid args");
        let tuned = parse(words("--fixture /a --output /o --quiet-load 1.5"), PathBuf::from("/t")).expect("valid args");
        assert!((default.quiet_load - 3.0).abs() < f64::EPSILON);
        assert!((tuned.quiet_load - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn negative_or_nan_quiet_load_is_an_error() {
        assert!(parse(words("--fixture /a --output /o --quiet-load -1"), PathBuf::from("/t")).is_err());
        assert!(parse(words("--fixture /a --output /o --quiet-load NaN"), PathBuf::from("/t")).is_err());
    }

    #[test]
    fn missing_output_is_an_error() {
        assert!(parse(words("--fixture /a"), PathBuf::from("/t")).is_err());
    }

    #[test]
    fn bad_iteration_count_is_an_error() {
        assert!(parse(words("--fixture /a --output /o --iterations x"), PathBuf::from("/t")).is_err());
    }

    #[test]
    fn cargo_arg_is_repeatable_and_keeps_order() {
        let config = parse(words("--fixture /a --output /o --cargo-arg --workspace --cargo-arg --exclude --cargo-arg omega"), PathBuf::from("/t")).expect("valid args");
        assert_eq!(config.cargo_args, ["--workspace", "--exclude", "omega"]);
    }

    #[test]
    fn cargo_arg_defaults_to_empty() {
        let config = parse(words("--fixture /a --output /o"), PathBuf::from("/t")).expect("valid args");
        assert_eq!(config.cargo_args, Vec::<String>::new());
    }

    #[test]
    fn cargo_arg_without_value_is_an_error() {
        assert!(parse(words("--fixture /a --output /o --cargo-arg"), PathBuf::from("/t")).is_err());
    }

    #[test]
    fn help_documents_the_cargo_arg_flag() {
        assert!(help().contains("--cargo-arg ARG"));
    }
}
