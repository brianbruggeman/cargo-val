use std::path::{Path, PathBuf};

use crate::failure_fixture::{Captured, build_command, captured_of, executable_of, run_command};

pub fn build(fixture: &str) -> PathBuf {
    let output = build_command(fixture).output().expect("cargo builds the failure fixture");
    executable_of(&output)
}

pub fn run(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Captured {
    let output = run_command(executable, test, envs).output().expect("the fixture test executable launches");
    captured_of(output)
}
