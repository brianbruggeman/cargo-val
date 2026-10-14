use std::path::{Path, PathBuf};

use crate::group::sync_deadline;
use crate::output::output_bounded;
use crate::failure_fixture::{Captured, build_command, captured_of, executable_of, run_command};

pub fn build(fixture: &str) -> PathBuf {
    let output = output_bounded(&mut build_command(fixture), sync_deadline()).unwrap_or_else(|failure| panic!("{failure}"));
    executable_of(&output)
}

pub fn run(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Captured {
    let output = output_bounded(&mut run_command(executable, test, envs), sync_deadline()).unwrap_or_else(|failure| panic!("{failure}"));
    captured_of(output)
}
