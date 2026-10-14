//! Runs the release `cargo-val` binary and `cargo test` on tests/fixtures/workspace and compares what each reports.
//!
//! Build the binary first: `cargo build --release --bin cargo-val`, then `cargo run --release --example parity`.

use std::collections::BTreeMap;
use std::io::{Read, pipe};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
    val_command.env("CARGO_VAL_OUTPUT", "rich");
    let val_output = run(val_command, target)?;
    let mut cargo = Command::new("cargo");
    cargo.args(["test", "--locked", "--color", "never"]);
    let test_output = run(cargo, target)?;
    let val = parse_val(&val_output);
    let test = parse_cargo_test(&test_output);
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

fn run(mut command: Command, target: &Path) -> Result<String, String> {
    let (mut reader, writer) = pipe().map_err(|error| format!("pipe: {error}"))?;
    let mut child = command
        .args(["--manifest-path", FIXTURE, "--target-dir"])
        .arg(target)
        .stdout(writer.try_clone().map_err(|error| format!("pipe clone: {error}"))?)
        .stderr(writer)
        .spawn()
        .map_err(|error| format!("spawn: {error}"))?;
    drop(command);
    let mut text = String::new();
    reader.read_to_string(&mut text).map_err(|error| format!("read: {error}"))?;
    let status = child.wait().map_err(|error| format!("wait: {error}"))?;
    if status.success() {
        return Ok(text);
    }
    Err(format!("exit {status}: {text}"))
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
        if let Some(name) = line.strip_suffix(" PASS") {
            report.names.push(name.to_string());
        } else if let Some((label, counts)) = line.split_once(": discovered ") {
            let passed = counts.split("passed ").nth(1).and_then(|rest| rest.split(',').next());
            if let (Some(passed), false) = (passed.and_then(|text| text.parse::<usize>().ok()), label == "total") {
                report.passed_per_binary.insert(val_key(label), passed);
            }
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

fn parse_cargo_test(output: &str) -> Report {
    let mut report = Report::default();
    let mut binary = String::new();
    for line in output.lines() {
        let line = line.trim_start();
        if let Some(running) = line.strip_prefix("Running ") {
            binary = test_key(running);
        } else if line.starts_with("Doc-tests ") {
            binary = DOC_TESTS.to_string();
        } else if let Some(name) = line.strip_prefix("test ").and_then(|rest| rest.strip_suffix(" ... ok")) {
            report.names.push(name.to_string());
        } else if let Some(rest) = line.strip_prefix("test result: ok. ") {
            let passed = rest.split(' ').next().and_then(|text| text.parse::<usize>().ok());
            report.passed_per_binary.insert(binary.clone(), passed.unwrap_or(0));
        }
    }
    report.names.sort();
    report
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
