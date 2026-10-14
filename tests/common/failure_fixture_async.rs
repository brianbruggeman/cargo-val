use std::path::{Path, PathBuf};

use crate::fixture::fixture_output;
use crate::failure_fixture::{Captured, build_command, captured_of, executable_of, run_command};

pub async fn build(fixture: &str) -> PathBuf {
    let output = fixture_output(&mut build_command(fixture)).await;
    executable_of(&output)
}

pub async fn run(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Captured {
    let output = fixture_output(&mut run_command(executable, test, envs)).await;
    captured_of(output)
}
