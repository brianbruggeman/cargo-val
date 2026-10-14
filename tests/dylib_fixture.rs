#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/plain.rs"]
mod plain;

use cargo_val::render::Outcome;
use fixture::fixture_output;

const LOADER_VARIABLES: [&str; 3] = [
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "LD_LIBRARY_PATH",
];

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dylib_env/Cargo.toml")
}

fn scrubbed(mut command: Command) -> Command {
    let target_directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dylib_env_target");
    command
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CARGO_TARGET_DIR", target_directory)
        .env("NO_COLOR", "1")
        .env_remove("CARGO_MANIFEST_DIR");
    for variable in LOADER_VARIABLES {
        command.env_remove(variable);
    }
    command
}

async fn run(mut command: Command) -> Output {
    let output = fixture_output(&mut command).await;
    assert!(
        output.status.success(),
        "{command:?} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn cargo_test_names(output: &Output) -> BTreeSet<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("test ")?.strip_suffix(" ... ok"))
        .map(str::to_owned)
        .collect()
}

fn cargo_val_names(output: &Output) -> BTreeSet<String> {
    plain::names(&String::from_utf8_lossy(&output.stdout), Outcome::Passed).into_iter().collect()
}

#[proxima::test(timeout = "310s")]
async fn proc_macro_test_binary_runs_under_cargo_val_with_the_environment_cargo_test_gives_it() {
    let mut cargo_test = scrubbed(Command::new("cargo"));
    cargo_test.args(["test", "--lib", "--manifest-path"]);
    cargo_test.arg(fixture_manifest());
    let mut cargo_val = scrubbed(binary::command());
    cargo_val.env("CARGO_VAL_OUTPUT", "plain");
    cargo_val.arg("--lib").arg("--manifest-path");
    cargo_val.arg(fixture_manifest());

    let expected = cargo_test_names(&run(cargo_test).await);
    let observed = cargo_val_names(&run(cargo_val).await);

    assert_eq!(expected.len(), 4, "cargo test names: {expected:?}");
    assert_eq!(observed.len(), 4, "cargo val names: {observed:?}");
    assert_eq!(observed, expected);
}
