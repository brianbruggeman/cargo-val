use std::fs;
use std::path::PathBuf;
use std::process::Command;

mod common;

use common::fixture_output;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
const PEERS: usize = 12;

#[proxima::test(timeout = "310s")]
async fn twelve_tests_alive_at_once_under_jobs_twelve_all_report_pass() {
    let rendezvous = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rendezvous-{}", std::process::id()));
    let _ = fs::remove_dir_all(&rendezvous);
    fs::create_dir_all(&rendezvous).expect("rendezvous directory is creatable under the target tmpdir");

    let mut command = Command::new(CARGO_VAL);
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rendezvous/Cargo.toml"))
        .args(["--process", "test", "--no-fail-fast"])
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_PROCESS")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_OUTPUT", "rich")
        .env("CARGO_VAL_JOBS", PEERS.to_string())
        .env("CARGO_VAL_FIXTURE_RENDEZVOUS_DIR", &rendezvous)
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fixture-target"));
    let output = fixture_output(&mut command).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let passes = stdout.lines().filter(|line| line.ends_with(" PASS")).count();
    let arrivals = fs::read_dir(&rendezvous).expect("rendezvous directory is readable").count();
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(passes, PEERS, "every peer reports PASS\n{stdout}");
    assert_eq!(arrivals, PEERS, "every peer process arrived\n{stdout}");
    for index in 1..=PEERS {
        let name = format!("peer_{index:02}");
        assert!(stdout.lines().any(|line| line.contains(&name) && line.ends_with(" PASS")), "{name} missing PASS\n{stdout}");
    }
}
