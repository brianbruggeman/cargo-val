use std::path::{Path, PathBuf};

// examples include the common modules too and cargo sets no CARGO_TARGET_TMPDIR for them, so their runs go under the package's own target
/// Where every isolated run of every test lives; one directory per run, named `<label>-<pid>-<run>`.
pub fn runs_directory() -> PathBuf {
    let base = option_env!("CARGO_TARGET_TMPDIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp"), PathBuf::from);
    base.join("isolated")
}

pub fn target_in(run: &Path) -> PathBuf {
    run.join("target")
}

pub fn home_in(run: &Path) -> PathBuf {
    run.join("home")
}
