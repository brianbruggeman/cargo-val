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

const PEERS: usize = 12;

#[proxima::test(timeout = "310s")]
async fn twelve_tests_alive_at_once_under_jobs_twelve_all_report_pass() {
    let rendezvous = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rendezvous-{}", std::process::id()));
    let _ = fs::remove_dir_all(&rendezvous);
    fs::create_dir_all(&rendezvous).expect("rendezvous directory is creatable under the target tmpdir");

    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rendezvous/Cargo.toml"))
        .args(["--process", "test", "--no-fail-fast"])
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_PROCESS")
        .env("NO_COLOR", "1")
        .env("CARGO_VAL_OUTPUT", "plain")
        .env("CARGO_VAL_JOBS", PEERS.to_string())
        .env("CARGO_VAL_FIXTURE_RENDEZVOUS_DIR", &rendezvous)
        .envs(isolation::environment("rendezvous-jobs"));
    let output = fixture_output(&mut command).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let passes = plain::names(&stdout, Outcome::Passed).len();
    let arrivals = fs::read_dir(&rendezvous).expect("rendezvous directory is readable").count();
    assert_eq!(output.status.code(), Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(passes, PEERS, "every peer reports PASS\n{stdout}");
    assert_eq!(arrivals, PEERS, "every peer process arrived\n{stdout}");
    for index in 1..=PEERS {
        let name = format!("peer_{index:02}");
        assert!(plain::names(&stdout, Outcome::Passed).iter().any(|test| test.ends_with(&name)), "{name} missing PASS\n{stdout}");
    }
}
