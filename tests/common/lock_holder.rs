use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::{lead, wait};

const HOLDER_MANIFEST: &str = r#"[package]
name = "lock_holder"
version = "0.1.0"
edition = "2024"
build = "build.rs"
"#;

const HOLDER_BUILD_SCRIPT: &str = r#"use std::io::Read;

fn main() {
    let fifo = std::env::var("HOLD_FIFO").expect("the harness names the fifo");
    let mut reader = std::fs::File::open(fifo).expect("the fifo opens for reading");
    let mut byte = [0_u8; 1];
    reader.read(&mut byte).expect("the harness closes its end to release the lock");
}
"#;

pub struct Holder {
    pub child: Child,
    release: File,
}

impl Holder {
    pub async fn release(self) {
        let Self { child, release } = self;
        drop(release);
        let status = wait::wait_within(&child, "SYNC_DEADLINE").await.expect("the holder cargo build finishes once released");
        assert!(status.success(), "the holder build finishes once released");
    }

    pub fn still_building(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the holder cargo build is pollable")
            .is_none()
    }
}

// the build script blocks reading a fifo, so the build lock is held until the harness closes the writer; no timing is involved
pub fn hold_build_lock(scratch: &Path, shared_target: &Path) -> Holder {
    let holder = scratch.join("holder");
    fs::create_dir_all(holder.join("src")).expect("holder directory is creatable");
    fs::write(holder.join("Cargo.toml"), HOLDER_MANIFEST).expect("holder manifest is writable");
    fs::write(holder.join("build.rs"), HOLDER_BUILD_SCRIPT).expect("holder build script is writable");
    fs::write(holder.join("src/lib.rs"), "").expect("holder library is writable");
    let fifo = scratch.join("hold.fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo is runnable");
    assert!(made.success(), "mkfifo creates {}", fifo.display());
    let mut holder_build = Command::new("cargo");
    lead::lead_group(&mut holder_build);
    let child = holder_build
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
