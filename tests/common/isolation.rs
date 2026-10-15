use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

mod layout {
    include!("isolation_layout.rs");
}

// the registry and git caches are shared read-mostly through links so nothing is downloaded; `.package-cache` and its lock stay private because they live at the CARGO_HOME root, not inside either
const LINKED_ENTRIES: [&str; 4] = ["registry", "git", "config.toml", "config"];

/// A fresh run's `CARGO_TARGET_DIR` (first) and `CARGO_HOME` (second), for a builder chain that takes `.envs(..)`; the target is `environment(label)[0].1` for a test that reads the run store or last-run record under it.
///
/// The run directory is named `label`, the pid and the first free run number, claimed by `create_dir` so two threads of one process never take the same one. Its CARGO_HOME is private, so `.package-cache` and its lock are too, and its registry and git caches are links to the ambient ones so nothing is downloaded.
pub fn environment(label: &str) -> [(&'static str, PathBuf); 2] {
    let parent = layout::runs_directory();
    fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("{} is not creatable: {error}", parent.display()));
    let run = claim(&parent, label);
    let home = layout::home_in(&run);
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("{} is not creatable: {error}", home.display()));
    link_caches(&ambient_cargo_home(), &home);
    [("CARGO_TARGET_DIR", layout::target_in(&run)), ("CARGO_HOME", home)]
}

fn claim(parent: &Path, label: &str) -> PathBuf {
    let process = std::process::id();
    let mut run = 0_u64;
    loop {
        let candidate = parent.join(format!("{label}-{process}-{run}"));
        match fs::create_dir(&candidate) {
            Ok(()) => return candidate,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => run += 1,
            Err(error) => panic!("{} is not creatable: {error}", candidate.display()),
        }
    }
}

fn ambient_cargo_home() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(|home| PathBuf::from(home).join(".cargo")))
        .expect("HOME or CARGO_HOME names the ambient cargo home")
}

fn link_caches(ambient: &Path, private: &Path) {
    for entry in LINKED_ENTRIES {
        let source = ambient.join(entry);
        if source.exists() {
            link(&source, &private.join(entry));
        }
    }
}

#[cfg(unix)]
fn link(source: &Path, destination: &Path) {
    std::os::unix::fs::symlink(source, destination).unwrap_or_else(|error| panic!("{} is not linkable to {}: {error}", destination.display(), source.display()));
}

#[cfg(windows)]
fn link(source: &Path, destination: &Path) {
    let linked = if source.is_dir() { std::os::windows::fs::symlink_dir(source, destination) } else { std::os::windows::fs::symlink_file(source, destination) };
    linked.unwrap_or_else(|error| panic!("{} is not linkable to {}: {error}", destination.display(), source.display()));
}
