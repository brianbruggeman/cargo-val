use std::path::PathBuf;
use std::process::Command;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");

const PASSED: usize = 40;
const FAILED: usize = 3;
const IGNORED: usize = 2;

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

#[test]
fn demo_fixture_reports_the_counts_the_readme_recording_depends_on() {
    let output = Command::new(CARGO_VAL)
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/demo/Cargo.toml"))
        .arg("--no-fail-fast")
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_IGNORED")
        .env_remove("CARGO_VAL_FILTERSET")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("demo-fixture"))
        .output()
        .expect("cargo-val binary is spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let totals: Vec<&str> = stdout.lines().filter(|line| line.starts_with("total: ")).collect();
    let expected = format!(
        "total: discovered {}, passed {PASSED}, failed {FAILED}, ignored {IGNORED}, filtered 0, wall ",
        PASSED + FAILED + IGNORED
    );
    assert_eq!(totals.len(), 1, "{stdout}{stderr}");
    assert!(totals[0].starts_with(&expected), "{}\n{stdout}{stderr}", totals[0]);
    assert_eq!(output.status.code(), Some(1), "{stdout}{stderr}");
}
