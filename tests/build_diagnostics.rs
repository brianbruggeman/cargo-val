use std::path::{Path, PathBuf};
use std::process::Command;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const ESCAPE: u8 = 0x1b;

struct Run {
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
    let output = Command::new(CARGO_VAL)
        .arg("--manifest-path")
        .arg(manifest())
        .arg("--lib")
        .env("NO_COLOR", "1")
        .env("CARGO_TERM_COLOR", "always")
        .env_remove("CARGO_VAL_COLOR")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-fixture"))
        .output()
        .expect("cargo-val binary is spawnable");
    Run {
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
