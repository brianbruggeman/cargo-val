use std::fs::{self};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::process::{Child, Stdio};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::Duration;

const CARGO_VAL: &str = env!("CARGO_BIN_EXE_cargo-val");
#[cfg(unix)]
const WAIT_LINE: &str = "cargo-val: waiting for ";
#[cfg(unix)]
const WAIT_LINE_DEADLINE: Duration = Duration::from_secs(240);

#[cfg(unix)]
const HOLDER_MANIFEST: &str = r#"[package]
name = "lock_holder"
version = "0.1.0"
edition = "2024"
build = "build.rs"
"#;

#[cfg(unix)]
const HOLDER_BUILD_SCRIPT: &str = r#"use std::io::Read;

fn main() {
    let fifo = std::env::var("HOLD_FIFO").expect("the harness names the fifo");
    let mut reader = std::fs::File::open(fifo).expect("the fifo opens for reading");
    let mut byte = [0_u8; 1];
    reader.read(&mut byte).expect("the harness closes its end to release the lock");
}
"#;

#[cfg(unix)]
struct Holder {
    child: Child,
    release: File,
}

#[cfg(unix)]
impl Holder {
    fn release(self) {
        let Self { mut child, release } = self;
        drop(release);
        let status = child.wait().expect("the holder cargo build is waitable");
        assert!(status.success(), "the holder build finishes once released");
    }

    fn still_building(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the holder cargo build is pollable")
            .is_none()
    }
}

fn scratch(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(directory.join("holder/src")).expect("scratch directory is creatable");
    directory
}

// the build script blocks reading a fifo, so the build lock is held until the harness closes the writer; no timing is involved
#[cfg(unix)]
fn hold_build_lock(scratch: &Path, shared_target: &Path) -> Holder {
    let holder = scratch.join("holder");
    fs::write(holder.join("Cargo.toml"), HOLDER_MANIFEST).expect("holder manifest is writable");
    fs::write(holder.join("build.rs"), HOLDER_BUILD_SCRIPT).expect("holder build script is writable");
    fs::write(holder.join("src/lib.rs"), "").expect("holder library is writable");
    let fifo = scratch.join("hold.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo is runnable");
    assert!(made.success(), "mkfifo creates {}", fifo.display());
    let child = Command::new("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg(holder.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", shared_target)
        .env("CARGO_HOME", scratch.join("holder-home"))
        .env("HOLD_FIFO", &fifo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cargo is spawnable");
    // opening for write returns only once the build script has the fifo open for read, which is after cargo took the lock
    let release = OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("the fifo opens once the build script is running");
    Holder { child, release }
}

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace/Cargo.toml")
}

// the package cache lock is global to CARGO_HOME and cannot be isolated, so the holder and cargo val each get a private CARGO_HOME: a held build lock is then the only contention under test
fn cargo_val(scratch: &Path, shared_target: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(CARGO_VAL);
    command
        .env("CARGO_VAL_OUTPUT", "rich")
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_TARGET_DIR")
        .env("NO_COLOR", "1")
        .env("CARGO_HOME", scratch.join("cargo-home"))
        .env("CARGO_TARGET_DIR", shared_target);
    command
}

// cargo 1.99 holds separate locks for the artifact and build directories; the package cache lock is global and is not under test
#[cfg(unix)]
fn directory_wait(line: &str) -> bool {
    line.starts_with(WAIT_LINE) && !line.contains("package cache")
}

fn passed(stdout: &str) -> usize {
    let total = stdout
        .lines()
        .find(|line| line.starts_with("total: "))
        .unwrap_or_else(|| panic!("a total line is printed:\n{stdout}"));
    let counted = total
        .split(", ")
        .find_map(|part| part.strip_prefix("passed "))
        .unwrap_or_else(|| panic!("the total line carries passed: {total}"));
    counted.parse().expect("passed is a count")
}

#[cfg(unix)]
#[test]
fn a_cargo_val_run_completes_while_another_cargo_holds_the_shared_build_lock() {
    let scratch = scratch("isolated-lock");
    let shared = scratch.join("shared-target");
    let mut holder = hold_build_lock(&scratch, &shared);

    let output = cargo_val(&scratch, &shared, &["--target-dir", "isolated"])
        .output()
        .expect("cargo-val is spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(holder.still_building(), "the lock was held for the whole run");
    assert_eq!(output.status.code(), Some(0), "{stdout}{stderr}");
    assert!(passed(&stdout) > 0, "{stdout}");
    let directory_blocks: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("Blocking waiting for file lock on") && !line.contains("package cache"))
        .collect();
    assert!(directory_blocks.is_empty(), "no child blocked on a target directory lock: {stderr}");
    assert!(!stderr.lines().any(directory_wait), "no wait line named a target directory: {stderr}");
    assert!(shared.join("cargo-val").is_dir(), "the run built under the isolated subtree");
    holder.release();
}

#[cfg(unix)]
#[test]
fn shared_mode_waits_on_the_held_lock_and_says_so_once_per_child() {
    let scratch = scratch("shared-lock");
    let shared = scratch.join("shared-target");
    let holder = hold_build_lock(&scratch, &shared);

    let mut child = cargo_val(&scratch, &shared, &["--target-dir", "shared"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cargo-val is spawnable");
    let stderr = child.stderr.take().expect("stderr is piped");
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if sender.send(line.expect("stderr is utf-8")).is_err() {
                break;
            }
        }
    });
    let mut lines = Vec::new();
    while !lines.iter().any(|seen: &String| directory_wait(seen)) {
        let line = receiver
            .recv_timeout(WAIT_LINE_DEADLINE)
            .unwrap_or_else(|fault| panic!("no wait line arrived ({fault}); stderr so far: {lines:?}"));
        lines.push(line);
    }
    holder.release();
    lines.extend(receiver.iter());
    reader.join().expect("the stderr reader finishes at EOF");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_string(&mut stdout)
        .expect("stdout drains");
    let status = child.wait().expect("cargo-val is waitable");

    let waits: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|candidate| directory_wait(candidate))
        .collect();
    assert_eq!(status.code(), Some(0), "{stdout}{lines:?}");
    assert!(passed(&stdout) > 0, "{stdout}");
    assert!((1..=2).contains(&waits.len()), "one line per waiting child: {waits:?}");
    assert!(
        waits.iter().all(|wait| wait.contains(shared.to_str().expect("utf-8 path"))),
        "{waits:?}"
    );
}

#[test]
fn a_named_target_dir_holds_a_test_and_a_doc_subdirectory() {
    let scratch = scratch("named-target");
    let shared = scratch.join("shared-target");
    let named = scratch.join("named-target");
    let named_text = named.to_str().expect("utf-8 path");

    let by_flag = cargo_val(&scratch, &shared, &["--target-dir", named_text])
        .output()
        .expect("cargo-val is spawnable");
    let by_setting = cargo_val(&scratch, &shared, &[])
        .env("CARGO_VAL_TARGET_DIR", named_text)
        .output()
        .expect("cargo-val is spawnable");

    for output in [&by_flag, &by_setting] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(0), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
        assert!(passed(&stdout) > 0, "{stdout}");
    }
    assert!(named.join("test/debug").is_dir());
    assert!(named.join("doc/debug").is_dir());
    assert!(!shared.exists(), "nothing was built in the environment's target directory");
}

#[test]
fn an_empty_target_dir_setting_is_refused_before_any_build() {
    let scratch = scratch("empty-target");
    let shared = scratch.join("shared-target");

    let output = cargo_val(&scratch, &shared, &[])
        .env("CARGO_VAL_TARGET_DIR", "")
        .output()
        .expect("cargo-val is spawnable");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("CARGO_VAL_TARGET_DIR must be isolated, shared, or a non-empty path")
    );
    assert!(!shared.exists());
}
