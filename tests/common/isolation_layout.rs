use std::path::{Path, PathBuf};

/// Where every isolated run of every test lives; one directory per run, named `<label>-<pid>-<run>`.
pub fn runs_directory() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("isolated")
}

pub fn target_in(run: &Path) -> PathBuf {
    run.join("target")
}

pub fn home_in(run: &Path) -> PathBuf {
    run.join("home")
}
