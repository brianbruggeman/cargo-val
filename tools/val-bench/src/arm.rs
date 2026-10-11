use crate::exec::Matchers;
use crate::parse;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    CargoTest,
    Nextest,
    CargoVal,
}

pub const ALL: [Arm; 3] = [Arm::CargoTest, Arm::Nextest, Arm::CargoVal];

impl Arm {
    pub fn name(self) -> &'static str {
        match self {
            Arm::CargoTest => "cargo-test",
            Arm::Nextest => "nextest",
            Arm::CargoVal => "cargo-val",
        }
    }

    pub fn parse(text: &str) -> Result<Arm, String> {
        ALL.into_iter()
            .find(|arm| arm.name() == text)
            .ok_or_else(|| format!("unknown arm {text:?}; expected cargo-test, nextest or cargo-val"))
    }

    pub fn subcommand_binary(self) -> Option<&'static str> {
        match self {
            Arm::CargoTest => None,
            Arm::Nextest => Some("cargo-nextest"),
            Arm::CargoVal => Some("cargo-val"),
        }
    }

    pub fn args(self, manifest: &str, filter: Option<&str>, jobs: u32, extra: &[String]) -> Vec<String> {
        let jobs = jobs.to_string();
        let manifest = manifest.to_string();
        let mut args: Vec<String> = match self {
            Arm::CargoTest => vec!["test".into()],
            Arm::Nextest => vec!["nextest".into(), "run".into()],
            Arm::CargoVal => vec!["val".into()],
        };
        if self != Arm::Nextest {
            args.extend(filter.map(str::to_string));
        }
        args.extend(["--manifest-path".into(), manifest]);
        match self {
            Arm::CargoTest => {
                args.extend(["--tests".into(), "--jobs".into(), jobs.clone()]);
                args.extend(extra.iter().cloned());
                args.extend(["--".into(), "--test-threads".into(), jobs]);
            }
            Arm::CargoVal => {
                args.extend(["--tests".into(), "--jobs".into(), jobs]);
                args.extend(extra.iter().cloned());
            }
            Arm::Nextest => {
                args.extend(["-j".into(), jobs]);
                args.extend(extra.iter().cloned());
                args.extend(filter.map(str::to_string));
            }
        }
        args
    }

    pub fn matchers(self) -> Matchers {
        match self {
            Arm::CargoTest => Matchers { stdout: parse::cargo_test_result_line, stderr: parse::never_result_line },
            Arm::Nextest => Matchers { stdout: parse::never_result_line, stderr: parse::nextest_result_line },
            Arm::CargoVal => Matchers { stdout: parse::cargo_val_result_line, stderr: parse::never_result_line },
        }
    }

    pub fn tests_run(self, stdout: &str, stderr: &str) -> u64 {
        match self {
            Arm::CargoTest => parse::cargo_test_tests_run(stdout),
            Arm::Nextest => parse::nextest_tests_run(stderr),
            Arm::CargoVal => parse::cargo_val_tests_run(stdout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_test_args_match_measure_c54b_macos_py_line_120_with_filter_first() {
        let args = Arm::CargoTest.args("/f/Cargo.toml", Some("order_seed"), 10, &[]);
        assert_eq!(args.join(" "), "test order_seed --manifest-path /f/Cargo.toml --tests --jobs 10 -- --test-threads 10");
    }

    #[test]
    fn cargo_val_args_match_measure_c54b_macos_py_line_121() {
        let args = Arm::CargoVal.args("/f/Cargo.toml", None, 10, &[]);
        assert_eq!(args.join(" "), "val --manifest-path /f/Cargo.toml --tests --jobs 10");
    }

    #[test]
    fn nextest_args_match_measure_c54b_macos_py_line_122_with_trailing_filter() {
        let args = Arm::Nextest.args("/f/Cargo.toml", Some("order_seed"), 10, &[]);
        assert_eq!(args.join(" "), "nextest run --manifest-path /f/Cargo.toml -j 10 order_seed");
    }

    #[test]
    fn unknown_arm_is_rejected() {
        assert!(Arm::parse("hyperfine").is_err());
    }

    fn excludes() -> Vec<String> {
        ["--workspace", "--exclude", "omega"].map(str::to_string).to_vec()
    }

    #[test]
    fn cargo_test_extra_args_land_before_the_harness_separator() {
        let args = Arm::CargoTest.args("/f/Cargo.toml", None, 10, &excludes());
        assert_eq!(args.join(" "), "test --manifest-path /f/Cargo.toml --tests --jobs 10 --workspace --exclude omega -- --test-threads 10");
    }

    #[test]
    fn cargo_val_extra_args_are_appended_plain() {
        let args = Arm::CargoVal.args("/f/Cargo.toml", None, 10, &excludes());
        assert_eq!(args.join(" "), "val --manifest-path /f/Cargo.toml --tests --jobs 10 --workspace --exclude omega");
    }

    #[test]
    fn nextest_extra_args_land_before_the_trailing_filter() {
        let args = Arm::Nextest.args("/f/Cargo.toml", Some("order_seed"), 10, &excludes());
        assert_eq!(args.join(" "), "nextest run --manifest-path /f/Cargo.toml -j 10 --workspace --exclude omega order_seed");
    }
}
