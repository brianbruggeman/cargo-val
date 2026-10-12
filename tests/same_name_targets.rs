#![cfg(unix)]

use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn rendezvous_directory() -> PathBuf {
    let directory = target_directory()
        .join("same-name-probe")
        .join(std::process::id().to_string());
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory)
        .expect("probe directory is creatable under the target directory");
    let fifo = CString::new(
        directory
            .join("rendezvous")
            .to_str()
            .expect("path is UTF-8"),
    )
    .expect("path has no interior NUL");
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    directory
}

#[test]
fn same_test_name_in_two_test_binaries_runs_while_the_first_is_live() {
    let directory = rendezvous_directory();
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/same_name/Cargo.toml");

    let output = Command::new(CARGO_VAL)
        .arg("--manifest-path")
        .arg(manifest)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env("CARGO_VAL_JOBS", "4")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_RENDEZVOUS_DIR", &directory)
        .env(
            "CARGO_TARGET_DIR",
            target_directory().join("same-name-fixture"),
        )
        .output()
        .expect("cargo-val binary is spawnable");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = stdout
        .lines()
        .filter(|line| *line == "shared_name PASS")
        .count();
    println!(
        "{{\"event\":\"same_name_two_targets\",\"status\":{:?},\"shared_name_passed\":{passed}}}",
        output.status.code()
    );
    assert_eq!(passed, 2, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
}
