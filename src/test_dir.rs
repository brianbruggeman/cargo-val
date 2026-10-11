use core::sync::atomic::{AtomicUsize, Ordering};
use std::io;
use std::path::{Path, PathBuf};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

pub struct TestDir {
    path: PathBuf,
}

pub fn tempdir() -> io::Result<TestDir> {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let name = std::format!("cargo-val-test-{}-{sequence}", std::process::id());
    let path = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&path)?;
    Ok(TestDir { path })
}

impl TestDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
