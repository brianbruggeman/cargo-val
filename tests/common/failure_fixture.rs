use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use cargo_val::failure_class::Exit;

pub struct Captured {
    pub stdout: Vec<u8>,
    pub exit: Exit,
}

fn target_directory(name: &str) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from)
        .join("failure-class-fixture")
        .join(name)
}

pub fn build(fixture: &str) -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture).join("Cargo.toml");
    let output = Command::new("cargo")
        .args(["test", "--lib", "--locked", "--no-run", "--message-format=json", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(target_directory(fixture))
        .output()
        .expect("cargo builds the failure fixture");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("\"profile\":{") && line.contains("\"test\":true"))
        .find_map(|line| {
            let after = line.split("\"executable\":\"").nth(1)?;
            Some(PathBuf::from(after.split('"').next()?.replace("\\\\", "\\")))
        })
        .expect("cargo reports the fixture test executable")
}

pub fn run(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Captured {
    let output = Command::new(executable)
        .args(["--exact", test, "--test-threads=1"])
        .env("RUST_BACKTRACE", "1")
        .envs(envs.iter().copied())
        .output()
        .expect("the fixture test executable launches");
    Captured { stdout: output.stdout, exit: exit_of(output.status) }
}

#[cfg(unix)]
fn exit_of(status: ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or_else(|| Exit::Code(status.code().unwrap_or(-1)), Exit::Signal)
}

#[cfg(not(unix))]
fn exit_of(status: ExitStatus) -> Exit {
    Exit::Code(status.code().unwrap_or(-1))
}
