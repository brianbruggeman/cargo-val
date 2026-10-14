use std::path::{Path, PathBuf};

use crate::group::SYNC_DEADLINE;
use crate::output::output_bounded;
use crate::failure_fixture::{Captured, build_command, captured_of, executable_of, run_command};

pub fn build(fixture: &str) -> PathBuf {
    let output = output_bounded(&mut build_command(fixture), SYNC_DEADLINE).unwrap_or_else(|failure| panic!("{failure}"));
    executable_of(&output)
}

pub fn run(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Captured {
    let output = output_bounded(&mut run_command(executable, test, envs), SYNC_DEADLINE).unwrap_or_else(|failure| panic!("{failure}"));
    captured_of(output)
}
