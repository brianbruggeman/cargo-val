#![cfg(unix)]

use std::ffi::CString;
use std::fs;
use std::path::PathBuf;

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/plain.rs"]
mod plain;

use cargo_val::render::Outcome;
use fixture::fixture_output;


fn target_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
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

#[proxima::test(timeout = "310s")]
async fn same_test_name_in_two_test_binaries_runs_while_the_first_is_live() {
    let directory = rendezvous_directory();
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/same_name/Cargo.toml");

    let mut command = binary::command();
    command
        .env("CARGO_VAL_OUTPUT", "plain")
        .arg("--manifest-path")
        .arg(manifest)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env("CARGO_VAL_JOBS", "4")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_RENDEZVOUS_DIR", &directory)
        .envs(isolation::environment("same-name"));
    let output = fixture_output(&mut command).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let passed = plain::names(&stdout, Outcome::Passed).iter().filter(|name| *name == "shared_name").count();
    println!(
        "{{\"event\":\"same_name_two_targets\",\"status\":{:?},\"shared_name_passed\":{passed}}}",
        output.status.code()
    );
    assert_eq!(passed, 2, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
}
