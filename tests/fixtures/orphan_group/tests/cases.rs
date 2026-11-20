use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELD_DEADLINE: Duration = Duration::from_secs(120);

fn directory() -> Option<PathBuf> {
    env::var_os("CARGO_VAL_FIXTURE_ORPHAN_DIR").map(PathBuf::from)
}

// the grandchild inherits this test's process group on purpose: it is what a cancelled session must not leave behind
#[test]
fn a_holds_a_grandchild_and_blocks() {
    let Some(directory) = directory() else {
        return;
    };
    let grandchild = Command::new("/usr/bin/tail")
        .args(["-f", "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("tail is spawnable");
    let staged = directory.join("grandchild.staged");
    fs::write(&staged, grandchild.id().to_string()).expect("the grandchild pid is writable");
    fs::rename(&staged, directory.join("grandchild")).expect("the grandchild pid is published");
    loop {
        thread::park();
    }
}

#[test]
fn b_fails_once_the_grandchild_is_held() {
    let Some(directory) = directory() else {
        return;
    };
    let started = Instant::now();
    while !directory.join("grandchild").exists() {
        assert!(started.elapsed() < HELD_DEADLINE, "the grandchild was never published");
        thread::yield_now();
    }
    panic!("failing so the run cancels the blocked session");
}
