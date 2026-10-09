use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

fn external_command_output() -> Output {
    let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_cargo-val"));
    let binary_directory = binary_path
        .parent()
        .expect("Cargo places the integration-test binary under a target directory");
    let cargo_path = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut search_path = OsString::from(binary_directory);
    search_path.push(if cfg!(windows) { ";" } else { ":" });
    if let Some(existing_path) = env::var_os("PATH") {
        search_path.push(existing_path);
    }

    Command::new(cargo_path)
        .args(["val", "--help"])
        .current_dir(env::temp_dir())
        .env("PATH", search_path)
        .output()
        .expect("Cargo must run its cargo-val external subcommand")
}

#[test]
fn help_banner() {
    let output = external_command_output();
    let expected = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/help-banner.txt"
    ))
    .expect("the literal help banner fixture must be readable");

    assert!(
        output.status.success(),
        "Cargo external command failed: {output:?}"
    );
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty(), "unexpected stderr: {output:?}");
}
