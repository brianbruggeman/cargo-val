use std::path::{Path, PathBuf};
use std::process::Command;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const ESCAPE: u8 = 0x1b;

struct Run {
    status: Option<i32>,
    stderr: Vec<u8>,
    text: String,
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/build_error/Cargo.toml")
}

fn run_build_error() -> Run {
    run_with(&["--lib"])
}

fn run_with(arguments: &[&str]) -> Run {
    let output = Command::new(CARGO_VAL)
        .arg("--manifest-path")
        .arg(manifest())
        .args(arguments)
        .env("NO_COLOR", "1")
        .env("CARGO_TERM_COLOR", "always")
        .env_remove("CARGO_VAL_COLOR")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-fixture"))
        .output()
        .expect("cargo-val binary is spawnable");
    Run {
        status: output.status.code(),
        text: String::from_utf8_lossy(&output.stderr).into_owned(),
        stderr: output.stderr,
    }
}

#[test]
fn a_type_error_shows_rustcs_diagnostic_before_the_build_failed_line() {
    let result = run_build_error();
    let location_text = format!("{}:4", Path::new("src").join("lib.rs").display());

    let error = result
        .text
        .find("error[E0308]: mismatched types")
        .unwrap_or_else(|| panic!("rustc's E0308 line is missing:\n{}", result.text));
    let location = result
        .text
        .find(&location_text)
        .unwrap_or_else(|| panic!("the file:line {location_text} of the type error is missing:\n{}", result.text));
    let failed = result
        .text
        .find("cargo-val: cargo test build failed")
        .unwrap_or_else(|| panic!("the build-failed line is missing:\n{}", result.text));

    assert!(error < failed, "{}", result.text);
    assert!(location < failed, "{}", result.text);
}

#[test]
fn rustcs_diagnostics_precede_cargos_summary_which_precedes_cargo_vals_line() {
    let result = run_build_error();

    let diagnostic = result
        .text
        .find("error[E0308]: mismatched types")
        .unwrap_or_else(|| panic!("rustc's E0308 line is missing:\n{}", result.text));
    let summary = result
        .text
        .find("error: could not compile")
        .unwrap_or_else(|| panic!("cargo's summary line is missing:\n{}", result.text));
    let failed = result
        .text
        .find("cargo-val: cargo test build failed")
        .unwrap_or_else(|| panic!("the build-failed line is missing:\n{}", result.text));

    assert!(diagnostic < summary, "{}", result.text);
    assert!(summary < failed, "{}", result.text);
}

#[test]
fn a_failed_build_also_shows_the_warnings_cargo_reported() {
    let result = run_build_error();

    assert!(
        result.text.contains("warning: unused doc comment"),
        "{}",
        result.text
    );
}

#[test]
fn colour_off_diagnostics_carry_no_escape_bytes() {
    let result = run_build_error();

    let escaped = result.stderr.contains(&ESCAPE);

    assert!(result.text.contains("error[E0308]"), "{}", result.text);
    assert!(!escaped, "{}", result.text);
}

#[test]
fn a_build_failure_exits_3() {
    let result = run_build_error();

    assert_eq!(result.status, Some(3), "{}", result.text);
}

#[test]
fn an_unknown_flag_is_an_invocation_error_naming_the_flag_and_exiting_2() {
    let result = run_with(&["--bogus"]);

    assert_eq!(result.status, Some(2), "{}", result.text);
    assert!(result.text.contains("--bogus"), "{}", result.text);
    assert!(!result.text.contains("build failed"), "{}", result.text);
    assert!(result.text.contains("refused the invocation"), "{}", result.text);
}
