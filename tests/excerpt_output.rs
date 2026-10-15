use std::path::{Path, PathBuf};
use std::process::Output;

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const WIDE_LINE_BYTES: usize = 200 * 1024 + 1;
const ROW_BUDGET: usize = 1024;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).join("Cargo.toml")
}

async fn run_manifest(manifest: &Path, mode: &str, target: &str, arguments: &[&str]) -> Output {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest)
        .args(["--tests", "--no-fail-fast", "--output", mode])
        .args(arguments)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_VAL_LOG", "debug")
        .envs(isolation::environment(target));
    fixture_output(&mut command).await
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("output is UTF-8")
}

fn report(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    format!("status {:?}\nstderr ({} bytes):\n{}\nstdout ({} bytes):\n{}", output.status, output.stderr.len(), String::from_utf8_lossy(&output.stderr), output.stdout.len(), stdout.chars().take(2000).collect::<String>())
}

async fn run_abort(mode: &str) -> String {
    let output = run_manifest(&fixture("failure_class"), mode, "excerpt-absent-target", &["-E", "test(=failing::abort_crash)"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", report(&output));
    stdout_of(&output)
}

fn wide_line_crate(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root.join("src")).expect("the fixture source directory is creatable");
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"wide_line\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").expect("the manifest is writable");
    let wide = "X".repeat(WIDE_LINE_BYTES);
    let source = format!("#[cfg(test)]\nmod tests {{\n    #[test]\n    fn wide_line_fails() {{\n        assert_eq!(\"{wide}\".len(), 0, \"wide line\");\n    }}\n}}\n");
    std::fs::write(root.join("src/lib.rs"), source).expect("the fixture source is writable");
    root.join("Cargo.toml")
}

async fn run_wide(mode: &str) -> String {
    let directory = tempfile::tempdir().expect("tempdir is creatable");
    let manifest = wide_line_crate(directory.path());
    let output = run_manifest(&manifest, mode, "excerpt-wide-target", &[]).await;
    assert_eq!(output.status.code(), Some(1), "{}", report(&output));
    stdout_of(&output)
}

#[proxima::test(timeout = "310s")]
async fn a_failure_with_no_panic_site_prints_the_exact_source_absent_reason_in_plain_output() {
    let text = run_abort("plain").await;

    assert!(text.lines().any(|line| line == "source absent: no_panic_site"), "{text}");
    assert!(!text.contains("------ "), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_failure_with_no_panic_site_reports_excerpt_absent_with_a_null_excerpt_in_json_output() {
    let text = run_abort("json").await;

    let fail = text.lines().find(|line| line.contains("\"t\":\"fail\"")).expect("a fail record");
    assert!(fail.contains("\"excerpt\":null"), "{fail}");
    assert!(fail.contains("\"excerpt_absent\":\"no_panic_site\""), "{fail}");
}

#[proxima::test(timeout = "310s")]
async fn a_source_line_over_200_kb_is_cut_and_marked_so_the_plain_closing_block_still_prints() {
    let text = run_wide("plain").await;

    assert!(text.lines().any(|line| line.starts_with("SUMMARY  ")), "no closing block in {} bytes", text.len());
    let marked = text.lines().find(|line| line.starts_with(">  ")).expect("the failing row");
    assert!(marked.ends_with('…') && marked.len() < ROW_BUDGET, "{} bytes", marked.len());
}

#[proxima::test(timeout = "310s")]
async fn a_source_line_over_200_kb_is_cut_and_marked_so_the_agent_record_after_the_excerpt_still_prints() {
    let text = run_wide("agent").await;

    let lines: Vec<&str> = text.lines().collect();
    let excerpt = lines.iter().position(|line| line.starts_with("EXCERPT")).expect("an EXCERPT record");
    let rerun = lines.iter().position(|line| line.starts_with("RERUN")).expect("the RERUN record after the excerpt");
    assert!(rerun > excerpt && lines[excerpt..rerun].iter().all(|line| line.len() < ROW_BUDGET));
    assert!(lines[excerpt..rerun].iter().any(|line| line.ends_with('…')));
    assert!(text.contains(" exit=1 outcome=failed schema=1\n"), "{} bytes", text.len());
}

#[proxima::test(timeout = "310s")]
async fn a_source_line_over_200_kb_is_cut_and_marked_so_the_json_fail_record_stays_one_closed_object() {
    let text = run_wide("json").await;

    let fail = text.lines().find(|line| line.contains("\"t\":\"fail\"")).expect("a fail record");
    assert!(fail.starts_with('{') && fail.ends_with('}') && fail.contains("\"rerun\":"), "{} bytes", fail.len());
    assert!(fail.contains("\"excerpt_absent\":null") && fail.contains('…'), "{} bytes", fail.len());
}

const BYTES_PANIC_SITE: &str = "/bytes-1.12.1/src/bytes.rs:533";

async fn run_external(mode: &str) -> String {
    let output = run_manifest(&fixture("failure_class"), mode, "excerpt-external-target", &["-E", "test(=failing::registry_panic_under_user_call)"]).await;
    assert_eq!(output.status.code(), Some(1), "{}", report(&output));
    stdout_of(&output)
}

fn external_reason(text: &str) -> String {
    let line = text.lines().find(|line| line.contains("panicked at ")).expect("the captured panic line");
    let site = line.split_once("panicked at ").expect("the panic marker").1;
    let mut parts = site.trim_end().trim_end_matches(':').rsplitn(3, ':');
    let _column = parts.next().expect("a column");
    let line_number = parts.next().expect("a line number");
    let file = parts.next().expect("a file");
    let reason = format!("external:{file}:{line_number}");
    let slashed = reason.replace("\\\\", "/").replace('\\', "/");
    assert!(slashed.ends_with(BYTES_PANIC_SITE) && slashed.contains("/registry/src/"), "{reason}");
    reason
}

#[proxima::test(timeout = "310s")]
async fn a_failure_with_no_panic_site_prints_the_exact_absent_record_in_agent_output() {
    let text = run_abort("agent").await;

    let records: Vec<&str> = text.lines().filter(|line| line.starts_with("EXCERPT")).collect();
    assert_eq!(records, ["EXCERPT  absent:no_panic_site"], "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_panic_inside_a_registry_dependency_prints_the_exact_external_reason_in_plain_output() {
    let text = run_external("plain").await;

    let reason = external_reason(&text);
    assert!(text.lines().any(|line| line == format!("source absent: {reason}")), "{text}");
    assert!(!text.contains("------ "), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_panic_inside_a_registry_dependency_reports_the_exact_external_reason_in_json_output() {
    let text = run_external("json").await;

    let fail = text.lines().find(|line| line.contains("\"t\":\"fail\"")).expect("a fail record");
    let capture = text.lines().find(|line| line.contains("\"t\":\"output\"")).expect("an output record").replace("\\u000a", "\n");
    let reason = external_reason(&capture);
    assert!(fail.contains(&format!("\"excerpt\":null,\"excerpt_absent\":\"{reason}\",")), "{fail}");
}

#[proxima::test(timeout = "310s")]
async fn a_panic_inside_a_registry_dependency_prints_the_exact_external_record_in_agent_output() {
    let text = run_external("agent").await;

    let classified = text.lines().find(|line| line.contains("  external:")).unwrap_or_else(|| panic!("the external site is on the class line:\n{text}"));
    let site = classified.split_once("external:").expect("the external marker").1;
    let reason = format!("external:{}", site.rsplit_once(':').expect("a column").0);
    assert!(reason.replace('\\', "/").contains("/registry/src/") && reason.replace('\\', "/").ends_with(BYTES_PANIC_SITE), "{reason}");
    let records: Vec<&str> = text.lines().filter(|line| line.starts_with("EXCERPT")).collect();
    assert_eq!(records, [format!("EXCERPT  absent:{reason}")], "{text}");
}
