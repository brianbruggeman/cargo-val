use std::format;
use std::string::{String, ToString};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Passed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeRunError {
    ZeroTests,
    OutcomeMismatch { expected: ProbeOutcome },
}

pub fn probe_path(module: &str, function: &str) -> String {
    match module.split_once("::") {
        Some((_, within_crate)) => format!("{within_crate}::{function}"),
        None => function.to_string(),
    }
}

pub fn verify_probe_output(stdout: &str, expected: ProbeOutcome) -> Result<(), ProbeRunError> {
    if !stdout.contains("running 1 test\n") && !stdout.contains("running 1 test\r\n") {
        return Err(ProbeRunError::ZeroTests);
    }
    let summary = match expected {
        ProbeOutcome::Passed => "1 passed; 0 failed",
        ProbeOutcome::Failed => "0 passed; 1 failed",
    };
    if stdout.contains(summary) {
        Ok(())
    } else {
        Err(ProbeRunError::OutcomeMismatch { expected })
    }
}

macro_rules! probe_name {
    ($function:ident) => {{
        let _: fn() = $function;
        $crate::test_probe::probe_path(module_path!(), stringify!($function))
    }};
}
pub(crate) use probe_name;

#[cfg(test)]
mod tests {
    use super::{ProbeOutcome, ProbeRunError, probe_path, verify_probe_output};

    const PASSED_RUN: &str = "\nrunning 1 test\ntest edge_spawn_windows::spawn::tests::child_process_probe ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 212 filtered out; finished in 0.00s\n\n";
    const FAILED_RUN: &str = "\nrunning 1 test\ntest edge_spawn_windows::spawn::tests::child_working_directory_probe ... FAILED\n\nfailures:\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 212 filtered out; finished in 0.00s\n\n";
    const ZERO_RUN: &str = "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 213 filtered out; finished in 0.00s\n\n";

    #[test]
    fn a_zero_test_run_fails_the_ran_exactly_one_test_guard() {
        for expected in [ProbeOutcome::Passed, ProbeOutcome::Failed] {
            assert_eq!(
                verify_probe_output(ZERO_RUN, expected),
                Err(ProbeRunError::ZeroTests)
            );
        }
        assert_eq!(
            verify_probe_output("", ProbeOutcome::Passed),
            Err(ProbeRunError::ZeroTests)
        );
    }

    #[test]
    fn one_test_runs_are_accepted_only_with_the_expected_outcome() {
        assert_eq!(
            verify_probe_output(PASSED_RUN, ProbeOutcome::Passed),
            Ok(())
        );
        assert_eq!(
            verify_probe_output(FAILED_RUN, ProbeOutcome::Failed),
            Ok(())
        );
        assert_eq!(
            verify_probe_output(PASSED_RUN, ProbeOutcome::Failed),
            Err(ProbeRunError::OutcomeMismatch {
                expected: ProbeOutcome::Failed
            })
        );
        assert_eq!(
            verify_probe_output(FAILED_RUN, ProbeOutcome::Passed),
            Err(ProbeRunError::OutcomeMismatch {
                expected: ProbeOutcome::Passed
            })
        );
    }

    #[test]
    fn probe_path_strips_the_crate_segment_and_follows_the_module() {
        assert_eq!(
            probe_path(
                "cargo_val::edge_spawn_windows::spawn::tests",
                "child_process_probe"
            ),
            "edge_spawn_windows::spawn::tests::child_process_probe"
        );
        assert_eq!(probe_path("cargo_val", "root_probe"), "root_probe");
    }

    #[test]
    fn probe_name_macro_resolves_to_this_modules_real_test_path() {
        fn named_probe() {}

        assert_eq!(probe_name!(named_probe), "test_probe::tests::named_probe");
    }
}
