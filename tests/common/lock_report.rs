use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

mod layout {
    include!("isolation_layout.rs");
}

/// The cargo lock holders at this instant, for a timeout message that must never leave a hang looking like a lock wait or the reverse: either each held lock file with its holder pids, or "no cargo lock held".
///
/// `command` supplies the `CARGO_HOME` and `CARGO_TARGET_DIR` the child ran with; every isolated run this process created is checked as well, so a caller that holds no `Command` still names the holders of the fixture it was waiting on.
// a fixture compile that waits on cargo's build-directory lock or the package-cache lock looks identical to a stuck rustc until the holder is named; the holder is usually another worktree's cargo
pub fn lock_holders(command: Option<&Command>) -> String {
    let files: Vec<String> = lock_files(command).into_iter().filter(|file| file.exists()).map(|file| file.display().to_string()).collect();
    let openers = cargo_val::lock_wait::openers_among(&files);
    let held: Vec<String> = files.iter().filter_map(|file| held_by(file, &openers)).collect();
    let verdict = if held.is_empty() { format!("no cargo lock held ({} lock files checked)", files.len()) } else { held.join("\n") };
    format!("{verdict}\nsccache servers:\n{}", sccache_servers())
}

fn process_runs() -> Vec<PathBuf> {
    let marker = format!("-{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(layout::runs_directory()) else {
        return Vec::new();
    };
    let mut runs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().contains(&marker))).collect();
    runs.sort();
    runs
}

fn held_by(file: &str, openers: &[(String, u32, bool)]) -> Option<String> {
    let holders = holders_of(file, openers);
    if holders.is_empty() { None } else { Some(format!("{file}:\n{}", holders.join("\n"))) }
}

fn command_variable(command: Option<&Command>, name: &str) -> Option<OsString> {
    match command.and_then(|command| command.get_envs().find(|(key, _)| *key == name)) {
        Some((_, value)) => value.map(OsString::from),
        None => std::env::var_os(name),
    }
}

const PACKAGE_CACHE_LOCKS: [&str; 2] = [".package-cache", ".package-cache-mutate"];
const BUILD_LOCKS: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

fn lock_files(command: Option<&Command>) -> Vec<PathBuf> {
    let cargo_home = command_variable(command, "CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| command_variable(command, "HOME").map(|home| PathBuf::from(home).join(".cargo")));
    let target = command_variable(command, "CARGO_TARGET_DIR").map(PathBuf::from);
    let runs = process_runs();
    let homes = cargo_home.into_iter().chain(runs.iter().map(|run| layout::home_in(run)));
    let targets = target.into_iter().chain(runs.iter().map(|run| layout::target_in(run)));
    homes.flat_map(|home| PACKAGE_CACHE_LOCKS.map(|name| home.join(name))).chain(targets.flat_map(|target| build_locks(&target))).collect()
}

// cargo-val's own `--target-dir` puts a named subdirectory under the target, and each profile directory holds its own locks, so the target and its children are both searched one level down
fn build_locks(target: &Path) -> Vec<PathBuf> {
    let children = std::fs::read_dir(target).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.is_dir());
    let directories: Vec<PathBuf> = std::iter::once(target.to_path_buf()).chain(children).collect();
    directories.iter().flat_map(|directory| [directory.clone(), directory.join("debug")]).flat_map(|directory| BUILD_LOCKS.map(|name| directory.join(name))).collect()
}

// the holder read is cargo_val::lock_wait's, so the dump and the deadline classifier name a holder the same way; where lsof marks none as holding, every opener is listed as an opener and not as a holder
fn holders_of(file: &str, openers: &[(String, u32, bool)]) -> Vec<String> {
    let opening: Vec<(u32, bool)> = openers.iter().filter(|(name, _, _)| name == file).map(|(_, pid, held)| (*pid, *held)).collect();
    let marked = opening.iter().any(|(_, held)| *held);
    opening.into_iter().filter(|(_, held)| *held || !marked).map(|(pid, held)| holder_line(pid, held)).collect()
}

fn holder_line(pid: u32, held: bool) -> String {
    let pid_text = pid.to_string();
    let directory = capture("lsof", ["-a", "-d", "cwd", "-Fn", "-p", pid_text.as_str()]);
    let cwd = directory.lines().find_map(|line| line.strip_prefix('n')).unwrap_or("unknown");
    let argv = capture("ps", ["-o", "args=", "-p", pid_text.as_str()]);
    let relation = if held { "holds the lock" } else { "has the file open" };
    format!("  pid {pid} {relation} cwd {cwd} argv {}", argv.trim())
}

fn sccache_servers() -> String {
    let listing = capture("ps", ["-axo", "pid,ppid,etime,command"]);
    let servers = listing.lines().filter(|line| line.contains("sccache") && !line.contains("rustc")).collect::<Vec<_>>();
    if servers.is_empty() { "  none".to_string() } else { servers.join("\n") }
}

// lsof exits 1 when nothing holds the file, so only stdout is read; a program that cannot run is reported in place of its output
pub fn capture<Argument: AsRef<OsStr>>(program: &str, arguments: impl IntoIterator<Item = Argument>) -> String {
    match Command::new(program).args(arguments).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(error) => format!("{program} did not run: {error}"),
    }
}
