#![cfg(unix)]
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use memchr::memmem;

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const MEBIBYTE: usize = 1_048_576;
const STDERR_HEADER: &str = "---- stderr ----\n";

fn command(environment: &[(&str, PathBuf); 2], mode: &str, bytes: usize) -> Command {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli_run/Cargo.toml"))
        .args(["--tests", "--no-fail-fast"])
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_VAL_OUTPUT", mode)
        .env("CARGO_VAL_C52D_FAIL", "1")
        .env("CARGO_VAL_VIEWS_BYTES", bytes.to_string())
        .envs(environment.clone());
    command
}

fn pattern(total: usize) -> Vec<u8> {
    (0..total).map(|index| if index % 64 == 63 { b'\n' } else { b'a' + u8::try_from(index % 26).unwrap_or(0) }).collect()
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn read_only_run_directory(environment: &[(&str, PathBuf); 2]) {
    let run = environment[0].1.clone().join("cargo-val").join("run");
    let _ = std::fs::remove_dir_all(&run);
    std::fs::create_dir_all(&run).expect("run directory is creatable");
    let mut permissions = std::fs::metadata(&run).expect("run directory exists").permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(&run, permissions).expect("run directory becomes read-only");
}

fn writable_again(environment: &[(&str, PathBuf); 2]) {
    let run = environment[0].1.clone().join("cargo-val").join("run");
    if let Ok(metadata) = std::fs::metadata(&run) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&run, permissions).expect("the run directory is writable again");
    }
}

fn number_after(text: &str, marker: &str) -> usize {
    let start = text.find(marker).expect("the marker is printed") + marker.len();
    let digits: String = text[start..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().expect("digits follow the marker")
}

#[proxima::test(timeout = "310s")]
async fn plain_output_prints_every_byte_of_both_streams_of_a_failure_from_the_run_store() {
    let output: Output = fixture_output(&mut command(&isolation::environment("output-views-full"), "plain", 2 * MEBIBYTE)).await;

    let expected = pattern(2 * MEBIBYTE);
    let header = memmem::find(&output.stdout, STDERR_HEADER.as_bytes()).expect("the stderr header is printed");
    let stdout_block = &output.stdout[..header];
    let stderr_block = &output.stdout[header + STDERR_HEADER.len()..][..expected.len()];
    let at = memmem::find(stdout_block, &expected[..4096]).expect("the stdout pattern is printed before the stderr header");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(digest(&stdout_block[at..at + expected.len()]), digest(&expected), "stdout pattern is byte-identical");
    assert_eq!(digest(stderr_block), digest(&expected), "stderr is the pattern and nothing else");
}

#[proxima::test(timeout = "310s")]
async fn an_unwritable_run_store_degrades_to_the_resident_tail_and_counts_what_it_dropped() {
    let environment = isolation::environment("output-views-memory");
    read_only_run_directory(&environment);

    let output = fixture_output(&mut command(&environment, "plain", MEBIBYTE)).await;

    writable_again(&environment);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let opened = text.find(STDERR_HEADER).expect("the stderr header is printed") + STDERR_HEADER.len();
    let dropped = number_after(&text[opened..], "[output truncated: ");
    assert_eq!(output.status.code(), Some(1), "the exit code is the failing test's, store or not");
    let marker = format!("[output truncated: {dropped} bytes dropped]\n");
    let closed = text[opened..].find(&marker).expect("the dropped count closes the stream");
    let shown = closed;
    assert!(shown < MEBIBYTE, "only the resident tail is shown: {shown}");
    assert_eq!(dropped + shown, MEBIBYTE, "dropped plus shown is the stream total");
}

#[proxima::test(timeout = "310s")]
async fn agent_output_for_a_one_mebibyte_failure_is_the_pinpoint_alone_with_no_raw_output_block() {
    let output = fixture_output(&mut command(&isolation::environment("output-views-agent"), "agent", MEBIBYTE)).await;

    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("requested fixture failure"), "the parsed message is kept:\n{text}");
    assert!(!text.lines().any(|line| line.starts_with("OUTPUT ")), "a parsed panic needs no raw block:\n{text}");
    assert!(text.len() < 4096, "{} bytes of agent output for a 1 MiB stream:\n{text}", text.len());
}

#[proxima::test(timeout = "310s")]
async fn a_panic_site_in_stdout_survives_a_one_mebibyte_stderr_storm_with_its_excerpt() {
    let output = fixture_output(&mut command(&isolation::environment("output-views-storm"), "agent", MEBIBYTE)).await;

    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(text.contains("PANIC   src/lib.rs:12:13") || text.contains("src/lib.rs:12:13"), "the site is kept:\n{}", &text[..text.len().min(2000)]);
    assert!(text.contains("EXCERPT"), "the excerpt is read from the kept site:\n{}", &text[..text.len().min(2000)]);
    assert!(text.contains("requested fixture failure"), "the message is kept");
}
