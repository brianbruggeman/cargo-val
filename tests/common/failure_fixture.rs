use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};

use cargo_val::failure_class::Exit;

pub struct Captured {
    pub stdout: Vec<u8>,
    pub exit: Exit,
}

pub fn build_command(fixture: &str) -> Command {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture).join("Cargo.toml");
    let mut command = Command::new("cargo");
    command
        .args(["test", "--lib", "--locked", "--no-run", "--message-format=json", "--manifest-path"])
        .arg(&manifest)
        .envs(crate::isolation::environment(&format!("failure-class-{fixture}")));
    command
}

pub fn executable_of(output: &Output) -> PathBuf {
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

pub fn run_command(executable: &Path, test: &str, envs: &[(&str, &str)]) -> Command {
    let mut command = Command::new(executable);
    command
        .args(["--exact", test, "--test-threads=1"])
        .env("RUST_BACKTRACE", "1")
        .envs(envs.iter().copied());
    command
}

pub fn captured_of(output: Output) -> Captured {
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
