use std::path::{Path, PathBuf};

fn base_directory() -> PathBuf {
    option_env!("CARGO_TARGET_TMPDIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tmp"), PathBuf::from)
}

/// Where every isolated run of every test lives; one directory per run, named `<label>-<pid>-<run>`. Examples include the common modules too and cargo sets no `CARGO_TARGET_TMPDIR` for them, so theirs go under the package's own target.
pub fn runs_directory() -> PathBuf {
    base_directory().join("isolated")
}

pub fn target_in(run: &Path) -> PathBuf {
    run.join("target")
}

// the home sits beside the runs under a short name: a git dependency checks out files nested a hundred characters deep, and a windows path past 260 characters cannot be created
/// The private `CARGO_HOME` of `run`, named by the pid and run number that end the run's name.
pub fn home_in(run: &Path) -> PathBuf {
    let name = run.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tail = name.rsplitn(3, '-');
    let (number, process) = (tail.next().unwrap_or_default(), tail.next().unwrap_or_default());
    base_directory().join("h").join(format!("{process}-{number}"))
}

/// The exclusive-lease marker of `run`: a directory beside its home, created to take the run and removed to release it.
pub fn lease_in(run: &Path) -> PathBuf {
    let mut home = home_in(run).into_os_string();
    home.push(".lease");
    PathBuf::from(home)
}
