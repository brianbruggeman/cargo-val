#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_val::failure_class::{Class, Exit, classify};
use cargo_val::isolated_target::{LockKind, LockWatch};
use cargo_val::lock_wait::at_deadline;
use futures::FutureExt;
use proxima_primitives::pipe::UnpinPipe;

mod common;
#[path = "common/binary.rs"]
mod binary;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/lead.rs"]
mod lead;
#[path = "common/lock_holder.rs"]
mod lock_holder;
#[path = "common/supervise.rs"]
mod supervise;
#[path = "common/wait.rs"]
mod wait;

use common::{drain_until, group};
use fixture::fixture_output;
use lock_holder::hold_build_lock;
use supervise::supervised;

const LOCK_LINE: &str = "Blocking waiting for file lock on build directory";

fn scratch(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(directory.join("holder/src")).expect("scratch directory is creatable");
    directory
}

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace/Cargo.toml")
}

// the doc cargo needs the build directory the holder's cargo build keeps locked until its fifo closes
fn cargo_beside(scratch: &Path, shared: &Path) -> Command {
    let mut command = Command::new("cargo");
    command
        .arg("test")
        .arg("--doc")
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .env("CARGO_HOME", scratch.join("cargo-home"))
        .env("CARGO_TARGET_DIR", shared)
        .env("CARGO_TERM_COLOR", "never");
    command
}

fn deadline() -> core::time::Duration {
    common::settings::bound("WAIT_LINE_DEADLINE")
}

#[proxima::test(timeout = "310s")]
async fn a_group_blocked_on_a_held_build_lock_classifies_as_a_lock_wait_naming_the_holders_pid() {
    let scratch = scratch("lock-wait-held");
    let shared = scratch.join("shared-target");
    let mut holder = hold_build_lock(&scratch, &shared);
    let mut waiter = cargo_beside(&scratch, &shared);

    let (exit, seen) = supervised(&mut waiter, deadline(), async |leader, _, stderr, _| {
        let (seen, fault) = drain_until(stderr, |bytes| String::from_utf8_lossy(bytes).contains(LOCK_LINE)).await;
        assert!(fault.is_none(), "stderr failed to read before the lock line: {fault:?}");
        let announced = LockWatch::new().observe(&seen);
        (at_deadline(leader, announced), seen)
    })
    .await;
    assert!(holder.still_building(), "the holder kept the lock through the deadline");
    let holder_pid = holder.child.id();
    holder.release().await;
    let scratch = scratch_two();
    let shared = scratch.join("shared-target");

    let record = classify(&seen, exit, 0).expect("a deadline kill classifies");
    assert_eq!(record.class, Class::LockWait { lock: LockKind::BuildDirectory, holder: holder_pid }, "exit {exit:?}, capture {}", String::from_utf8_lossy(&seen));
}

#[proxima::test(timeout = "310s")]
async fn the_group_that_holds_the_lock_is_not_waiting_on_it_and_stays_a_hang() {
    let scratch = scratch("lock-wait-holder");
    let holder = hold_build_lock(&scratch, &scratch.join("shared-target"));

    let exit = at_deadline(holder.child.id(), Some(LockKind::BuildDirectory));
    holder.release().await;
    let scratch = scratch_two();
    let shared = scratch.join("shared-target");

    assert_eq!(exit, Exit::DeadlineKilled { waiting: None });
    assert_eq!(classify(b"", exit, 0).expect("a deadline kill classifies").class, Class::Hang);
}

#[proxima::test(timeout = "310s")]
async fn a_group_blocked_on_a_fifo_with_no_lock_stays_a_hang() {
    let scratch = scratch("lock-wait-fifo");
    let fifo = scratch.join("never-written.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo is runnable");
    assert!(made.success(), "mkfifo creates {}", fifo.display());
    let mut sleeper = Command::new("cat");
    sleeper.arg(&fifo);

    let (exit, still_blocked) = supervised(&mut sleeper, deadline(), async |leader, _, _, source| {
        let still_blocked = UnpinPipe::call(source, ()).now_or_never().is_none();
        (at_deadline(leader, None), still_blocked)
    })
    .await;

    assert!(still_blocked, "cat is blocked opening a fifo nobody writes");
    assert_eq!(exit, Exit::DeadlineKilled { waiting: None });
    assert_eq!(classify(b"", exit, 0).expect("a deadline kill classifies").class, Class::Hang);
}

#[proxima::test(timeout = "310s")]
async fn a_stale_cargo_lock_under_locked_is_cargos_own_refusal_not_a_hang_or_a_wait() {
    let scratch = scratch("lock-wait-stale");
    let package = scratch.join("stale");
    fs::create_dir_all(package.join("src")).expect("package directory is creatable");
    fs::write(package.join("Cargo.toml"), "[package]\nname = \"stale\"\nversion = \"0.2.0\"\nedition = \"2024\"\n").expect("manifest is writable");
    fs::write(package.join("Cargo.lock"), "version = 4\n\n[[package]]\nname = \"stale\"\nversion = \"0.1.0\"\n").expect("lock file is writable");
    fs::write(package.join("src/lib.rs"), "#[test]\nfn passes() {}\n").expect("library is writable");
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(package.join("Cargo.toml"))
        .args(["--locked", "--output", "plain"])
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", scratch.join("stale-target"))
        .env("CARGO_HOME", scratch.join("cargo-home"));

    let output = fixture_output(&mut command).await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("cannot update the lock file"), "cargo's own message is shown: {stderr}");
