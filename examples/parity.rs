//! Runs the release `cargo-val` binary and `cargo test` on tests/fixtures/workspace and compares what each reports.
//!
//! Build the binary first: `cargo build --release --bin cargo-val`, then `cargo run --release --example parity`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/block_on.rs"]
mod block_on;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/workspace/Cargo.toml");
const LIBRARY: &str = "lib";
const DOC_TESTS: &str = "doc-tests";

#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    names: Vec<String>,
    passed_per_binary: BTreeMap<String, usize>,
}

fn main() -> ExitCode {
    match compare() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("parity: {message}");
            ExitCode::FAILURE
        }
    }
}

fn compare() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    let profile_directory = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("example executable has no profile directory")?;
    let binary = profile_directory.join(format!("cargo-val{}", std::env::consts::EXE_SUFFIX));
    if !binary.is_file() {
        return Err(format!("{} is missing; run `cargo build --release --bin cargo-val`", binary.display()));
    }
    let target = profile_directory.join("parity-fixture");
    let outcome = observe(&binary, &target);
    let _ = std::fs::remove_dir_all(&target);
    outcome
}

fn observe(binary: &Path, target: &Path) -> Result<(), String> {
    let mut val_command = Command::new(binary);
    #[cfg(unix)]
    val_command.env("CARGO_VAL_EXIT_WITH_PARENT", "1");
    val_command.env("CARGO_VAL_OUTPUT", "plain");
    let (val_stdout, val_stderr) = run(val_command, target)?;
    let val_output = format!("{val_stdout}{val_stderr}");
    let mut cargo = Command::new("cargo");
    cargo.args(["test", "--locked", "--color", "never"]);
    let (test_stdout, test_stderr) = run(cargo, target)?;
    let val = parse_val(&val_output);
    let test = parse_cargo_test(&test_stdout, &test_stderr);
    print_report("cargo val", &val);
    print_report("cargo test", &test);
    if val.names.is_empty() {
        return Err("cargo val reported no test names".into());
    }
    if val != test {
        return Err("cargo val and cargo test disagree".into());
    }
    println!("parity: identical, {} test names across {} binaries", val.names.len(), val.passed_per_binary.len());
    Ok(())
}

fn run(mut command: Command, target: &Path) -> Result<(String, String), String> {
    command.args(["--manifest-path", FIXTURE, "--target-dir"]).arg(target);
    let output = block_on::output_blocking(command, common::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok((stdout, stderr));
    }
    Err(format!("exit {}: {stdout}{stderr}", output.status))
}

fn strip_escapes(line: &str) -> String {
    let mut plain = String::new();
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            characters.by_ref().find(char::is_ascii_alphabetic);
        } else {
            plain.push(character);
        }
    }
    plain
}

fn parse_val(output: &str) -> Report {
    let mut report = Report::default();
    for line in output.lines().map(strip_escapes) {
        if let Some(settled) = line.strip_prefix("PASS ") {
            let mut fields = settled.split_whitespace();
            if let Some(binary) = fields.next() {
                *report.passed_per_binary.entry(val_key(binary)).or_default() += 1;
            }
            report.names.extend(fields.next().map(str::to_string));
        }
    }
    report.names.sort();
    report
}

fn val_key(label: &str) -> String {
    match label.rsplit_once("::") {
        Some((_, target)) => target.to_string(),
        None if label == DOC_TESTS => DOC_TESTS.to_string(),
        None => LIBRARY.to_string(),
    }
}

fn parse_cargo_test(stdout: &str, stderr: &str) -> Report {
    let mut report = Report::default();
    let mut binaries = stderr.lines().filter_map(binary_key);
    for line in stdout.lines().map(str::trim_start) {
        if let Some(name) = line.strip_prefix("test ").and_then(|rest| rest.strip_suffix(" ... ok")) {
            report.names.push(name.to_string());
        } else if let Some(rest) = line.strip_prefix("test result: ok. ") {
            let passed = rest.split(' ').next().and_then(|text| text.parse::<usize>().ok());
            report.passed_per_binary.insert(binaries.next().unwrap_or_default(), passed.unwrap_or(0));
        }
    }
    report.names.sort();
    report
}

fn binary_key(line: &str) -> Option<String> {
    let line = line.trim_start();
    match line.strip_prefix("Running ") {
        Some(running) => Some(test_key(running)),
        None => line.starts_with("Doc-tests ").then(|| DOC_TESTS.to_string()),
    }
}

fn test_key(running: &str) -> String {
    let path = running.split(" (").next().unwrap_or(running);
    match path.strip_prefix("unittests ") {
        Some(_) => LIBRARY.to_string(),
        None => PathBuf::from(path)
            .file_stem()
            .map_or_else(|| path.to_string(), |stem| stem.to_string_lossy().into_owned()),
    }
}

fn print_report(label: &str, report: &Report) {
    println!("{label}: {} names", report.names.len());
    for name in &report.names {
        println!("  {name}");
    }
    for (binary, passed) in &report.passed_per_binary {
        println!("  binary {binary}: passed {passed}");
    }
}
