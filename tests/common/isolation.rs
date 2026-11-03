use std::cell::RefCell;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

mod layout {
    include!("isolation_layout.rs");
}

// the registry and git caches are shared read-mostly through links so nothing is downloaded; `.package-cache` and its lock stay private because they live at the CARGO_HOME root, not inside either
const LINKED_ENTRIES: [&str; 4] = ["registry", "git", "config.toml", "config"];

// a test body hands this to `.envs(..)` as a temporary and spawns later, so no call site could hold a guard; the test's own thread holds it instead and its exit releases every run the test leased
thread_local! {
    static HELD: RefCell<Vec<Lease>> = const { RefCell::new(Vec::new()) };
}

/// The exclusive claim on one run; dropping it removes the marker, which frees the run for the next test of the same label.
struct Lease {
    marker: PathBuf,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.marker) {
            eprintln!("{} is not releasable: {error}", self.marker.display());
        }
    }
}

/// A leased run's `CARGO_TARGET_DIR` (first) and `CARGO_HOME` (second), for a builder chain that takes `.envs(..)`; the target is `environment(label)[0].1` for a test that reads the run store or last-run record under it.
///
/// Runs are leased, not created: a run of `label` released by an earlier test of this process is taken warm, and only when every one is held does a new one get made. Two tests running at once therefore never share a target or a `CARGO_HOME`, so no cargo build lock is ever waited on between them, while sequential tests of one label reuse one built target. The lease ends when the leasing test's thread exits.
///
/// The run directory is named `label`, the pid and a run number, and its `CARGO_HOME` is private, so `.package-cache` and its lock are too; its registry and git caches are links to the ambient ones so nothing is downloaded.
pub fn environment(label: &str) -> [(&'static str, PathBuf); 2] {
    let parent = layout::runs_directory();
    fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("{} is not creatable: {error}", parent.display()));
    let (run, lease) = take_warm(&parent, label).unwrap_or_else(|| claim(&parent, label));
    HELD.with(|held| held.borrow_mut().push(lease));
    [("CARGO_TARGET_DIR", layout::target_in(&run)), ("CARGO_HOME", layout::home_in(&run))]
}

// `create_dir` of the marker is the atomic exclusive take, so two threads racing for one released run cannot both win it
fn take_warm(parent: &Path, label: &str) -> Option<(PathBuf, Lease)> {
    let prefix = format!("{label}-{}-", std::process::id());
    let mut runs: Vec<(u64, PathBuf)> = fs::read_dir(parent)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", parent.display()))
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            let number = name.strip_prefix(&prefix)?.parse::<u64>().ok()?;
            Some((number, parent.join(name)))
        })
        .collect();
    runs.sort();
    runs.into_iter().find_map(|(_, run)| take(&run).map(|lease| (run, lease)))
}

fn take(run: &Path) -> Option<Lease> {
    let marker = layout::lease_in(run);
    match fs::create_dir(&marker) {
        Ok(()) => Some(Lease { marker }),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => None,
        Err(error) => panic!("{} is not creatable: {error}", marker.display()),
    }
}

// the run number is claimed on the home, whose name carries no label, so two labels in one process can never share a home; the lease marker is taken before the run directory appears so a scan never finds a run that is not yet leased
fn claim(parent: &Path, label: &str) -> (PathBuf, Lease) {
    let process = std::process::id();
    let mut run = 0_u64;
    loop {
        let candidate = parent.join(format!("{label}-{process}-{run}"));
        let home = layout::home_in(&candidate);
        fs::create_dir_all(home.parent().expect("a home has a parent")).unwrap_or_else(|error| panic!("{} is not creatable: {error}", home.display()));
        match fs::create_dir(&home) {
            Ok(()) => return (candidate.clone(), found(&candidate, &home)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => run += 1,
            Err(error) => panic!("{} is not creatable: {error}", home.display()),
        }
    }
}

fn found(candidate: &Path, home: &Path) -> Lease {
    let lease = take(candidate).expect("a run nobody else can name has no lease yet");
    fs::create_dir(candidate).unwrap_or_else(|error| panic!("{} is not creatable: {error}", candidate.display()));
    link_caches(&ambient_cargo_home(), home);
    lease
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
