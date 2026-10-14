use std::time::Duration;

mod build_defaults {
    include!(concat!(env!("OUT_DIR"), "/cargo_val_test.rs"));
}

/// The test bound `name` in seconds: `CARGO_VAL_TEST_<NAME>` over the `CARGO_VAL_TESTBUILD_<NAME>` default build.rs generated.
pub fn bound(name: &str) -> Duration {
    let key = format!("CARGO_VAL_TEST_{name}");
    match std::env::var(&key) {
        Ok(text) => Duration::from_secs(text.parse().unwrap_or_else(|_| panic!("{key} must be a whole number of seconds, got {text:?}"))),
        Err(_) => Duration::from_secs(build_default(name)),
    }
}

fn build_default(name: &str) -> u64 {
    match name {
        "FIXTURE_DEADLINE" => build_defaults::FIXTURE_DEADLINE,
        "SYNC_DEADLINE" => build_defaults::SYNC_DEADLINE,
        "FIFO_DEADLINE" => build_defaults::FIFO_DEADLINE,
        "WAIT_LINE_DEADLINE" => build_defaults::WAIT_LINE_DEADLINE,
        "INTERRUPT_GRACE" => build_defaults::INTERRUPT_GRACE,
        "GONE_DEADLINE" => build_defaults::GONE_DEADLINE,
        "TASKKILL_DEADLINE" => build_defaults::TASKKILL_DEADLINE,
        "HELP_DEADLINE" => build_defaults::HELP_DEADLINE,
        "BOUNDED_WAIT" => build_defaults::BOUNDED_WAIT,
        "EXAMPLE_DEADLINE" => build_defaults::EXAMPLE_DEADLINE,
        _ => panic!("{name} is not a test bound declared in build.rs TestDefaults"),
    }
}
