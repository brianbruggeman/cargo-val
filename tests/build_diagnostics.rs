use std::path::{Path, PathBuf};

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

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

async fn run_build_error() -> Run {
    run_with(&["--lib"]).await
}

async fn run_with(arguments: &[&str]) -> Run {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .args(arguments)
        .env("NO_COLOR", "1")
        .env("CARGO_TERM_COLOR", "always")
        .env_remove("CARGO_VAL_COLOR")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-fixture"));
    let output = fixture_output(&mut command).await;
    Run {
        status: output.status.code(),
        text: String::from_utf8_lossy(&output.stderr).into_owned(),
        stderr: output.stderr,
    }
}

#[proxima::test(timeout = "310s")]
async fn a_type_error_shows_rustcs_diagnostic_before_the_build_failed_line() {
    let result = run_build_error().await;
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

#[proxima::test(timeout = "310s")]
async fn rustcs_diagnostics_precede_cargos_summary_which_precedes_cargo_vals_line() {
    let result = run_build_error().await;

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

#[proxima::test(timeout = "310s")]
async fn a_failed_build_also_shows_the_warnings_cargo_reported() {
    let result = run_build_error().await;

    assert!(
        result.text.contains("warning: unused doc comment"),
        "{}",
        result.text
    );
}

#[proxima::test(timeout = "310s")]
async fn colour_off_diagnostics_carry_no_escape_bytes() {
    let result = run_build_error().await;

    let escaped = result.stderr.contains(&ESCAPE);

    assert!(result.text.contains("error[E0308]"), "{}", result.text);
    assert!(!escaped, "{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn coloured_diagnostics_still_precede_cargos_coloured_summary() {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .arg("--lib")
        .env_remove("NO_COLOR")
        .env("CARGO_VAL_COLOR", "true")
        .env("CARGO_VAL_OUTPUT", "rich")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-coloured-fixture"));
    let output = fixture_output(&mut command).await;
    let text = String::from_utf8_lossy(&output.stderr);

    assert!(output.stderr.contains(&ESCAPE), "colour is on, so cargo's summary carries SGR bytes:\n{text}");
    let diagnostic = text.find("E0308").unwrap_or_else(|| panic!("rustc's E0308 is missing:\n{text}"));
    let summary = text.find(": could not compile").unwrap_or_else(|| panic!("cargo's summary is missing:\n{text}"));
    let failed = text.find("cargo-val: cargo test build failed").unwrap_or_else(|| panic!("the build-failed line is missing:\n{text}"));
    assert!(diagnostic < summary && summary < failed, "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_build_failure_exits_3() {
    let result = run_build_error().await;

    assert_eq!(result.status, Some(3), "{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn an_unknown_flag_is_an_invocation_error_naming_the_flag_and_exiting_2() {
    let result = run_with(&["--bogus"]).await;

    assert_eq!(result.status, Some(2), "{}", result.text);
    assert!(result.text.contains("--bogus"), "{}", result.text);
    assert!(!result.text.contains("build failed"), "{}", result.text);
    assert!(result.text.contains("refused the invocation"), "{}", result.text);
}
