use std::fs;
#[cfg(unix)]
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Once;

mod layout {
    include!("isolation_layout.rs");
}

// the registry and git caches are shared read-mostly through links so nothing is downloaded; `.package-cache` and its lock stay private because they live at the CARGO_HOME root, not inside either
const LINKED_ENTRIES: [&str; 4] = ["registry", "git", "config.toml", "config"];

/// A fresh run's `CARGO_TARGET_DIR` (first) and `CARGO_HOME` (second), for a builder chain that takes `.envs(..)`; the target is `environment(label)[0].1` for a test that reads the run store or last-run record under it.
///
/// The run directory is named `label`, the pid and the first free run number, claimed by `create_dir` so two threads of one process never take the same one. Its `CARGO_HOME` is private, so `.package-cache` and its lock are too, and its registry and git caches are links to the ambient ones so nothing is downloaded.
pub fn environment(label: &str) -> [(&'static str, PathBuf); 2] {
    sweep_finished_runs();
    let parent = layout::runs_directory();
    fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("{} is not creatable: {error}", parent.display()));
    let run = claim(&parent, label);
    let home = layout::home_in(&run);
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("{} is not creatable: {error}", home.display()));
    link_caches(&ambient_cargo_home(), &home);
    [("CARGO_TARGET_DIR", layout::target_in(&run)), ("CARGO_HOME", home)]
}

// the run number is claimed on the home, whose name carries no label, so two labels in one process can never share a home
#[cfg(unix)]
fn claim(parent: &Path, label: &str) -> PathBuf {
    let process = std::process::id();
    let mut run = 0_u64;
    loop {
        let candidate = parent.join(format!("{label}-{process}-{run}"));
        let home = layout::home_in(&candidate);
        fs::create_dir_all(home.parent().expect("a home has a parent")).unwrap_or_else(|error| panic!("{} is not creatable: {error}", home.display()));
        match fs::create_dir(&home) {
            Ok(()) => {
                fs::create_dir(&candidate).unwrap_or_else(|error| panic!("{} is not creatable: {error}", candidate.display()));
                return candidate;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => run += 1,
            Err(error) => panic!("{} is not creatable: {error}", home.display()),
        }
    }
}

// windows runs its tests one at a time (`--test-threads=1`), so no two can contend for a lock there, and a cold fixture build per test costs more than the job's time limit allows; a label keeps one warm run per process, numbered by a hash of the label so two labels never share a home
#[cfg(windows)]
fn claim(parent: &Path, label: &str) -> PathBuf {
    let number = label.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
    let run = parent.join(format!("{label}-{}-{number}", std::process::id()));
    for directory in [layout::home_in(&run), run.clone()] {
        fs::create_dir_all(&directory).unwrap_or_else(|error| panic!("{} is not creatable: {error}", directory.display()));
    }
    run
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
        if source.exists() && fs::symlink_metadata(private.join(entry)).is_err() {
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

// every pass leaves a target per fixture run, about seventeen gigabytes for the whole suite, so the first run a process makes removes the runs of processes that have finished; a live process's runs are never touched, and windows keeps them all
#[cfg(unix)]
fn sweep_finished_runs() {
    static SWEPT: Once = Once::new();
    SWEPT.call_once(|| {
        let Ok(entries) = fs::read_dir(layout::runs_directory()) else {
            return;
        };
        for run in entries.flatten().map(|entry| entry.path()) {
            let name = run.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            let process = name.rsplit('-').nth(1).and_then(|text| text.parse::<i32>().ok());
            if process.is_some_and(|process| !process_is_alive(process)) {
                let _ = fs::remove_dir_all(layout::home_in(&run));
                let _ = fs::remove_dir_all(&run);
            }
        }
    });
}

#[cfg(unix)]
fn process_is_alive(process: i32) -> bool {
    // safety: kill with signal 0 only checks that the process exists
    unsafe { libc::kill(process, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
fn sweep_finished_runs() {}
