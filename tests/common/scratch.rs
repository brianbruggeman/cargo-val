use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A scratch tree under `CARGO_TARGET_TMPDIR` that is deleted when the test ends, whether it passed, failed or panicked.
pub struct Scratch {
    directory: PathBuf,
}

impl Scratch {
    pub fn new(label: &str) -> Self {
        let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("scratch directory is creatable");
        Self { directory }
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.directory
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
