use core::time::Duration;
use std::process::{Command, Output};

use crate::common::{output_within, settings};

pub fn fixture_deadline() -> Duration {
    settings::bound("FIXTURE_DEADLINE")
}

/// Runs a fixture command to completion, failing the test with the captured output if it outlives the fixture deadline (`CARGO_VAL_TEST_FIXTURE_DEADLINE`).
pub async fn fixture_output(command: &mut Command) -> Output {
    output_within(command, fixture_deadline(), "FIXTURE_DEADLINE").await.unwrap_or_else(|failure| panic!("{failure}"))
}
