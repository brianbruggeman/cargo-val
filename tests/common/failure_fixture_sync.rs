use std::path::{Path, PathBuf};

use crate::block_on::output_blocking;
use crate::common::settings;
use crate::failure_fixture::{Captured, build_command, captured_of, executable_of, run_command};

pub fn build(fixture: &str) -> PathBuf {
    let output = output_blocking(build_command(fixture), settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE").unwrap_or_else(|failure| panic!("{failure}"));
    executable_of(&output)
}

pub fn run(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Captured {
    let output = output_blocking(run_command(executable, test, envs), settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE").unwrap_or_else(|failure| panic!("{failure}"));
    captured_of(output)
}
