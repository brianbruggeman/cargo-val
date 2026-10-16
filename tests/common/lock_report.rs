use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The cargo lock holders at this instant, for a timeout message that must never leave a hang looking like a lock wait or the reverse: each held lock file with its holder pids, or `no cargo lock held (N lock files checked)`.
///
/// The lock files are the ones the CHILD uses: `command`'s explicit `CARGO_HOME` and `CARGO_TARGET_DIR`, then the environment of `leader` read from the live process, then of every child of this test process when neither is given, then the test's own. Reading only the test's own environment names the wrong directories whenever the child was given its own target.
// a fixture compile that waits on cargo's build-directory lock or the package-cache lock looks identical to a stuck rustc until the holder is named; the holder is usually another worktree's cargo
pub fn lock_holders(command: Option<&Command>, leader: Option<u32>) -> String {
    let files: Vec<PathBuf> = lock_files(command, leader).into_iter().filter(|file| file.exists()).collect();
    let held: Vec<String> = files.iter().filter_map(|file| held_by(file)).collect();
    let verdict = if held.is_empty() { format!("no cargo lock held ({} lock files checked)", files.len()) } else { held.join("\n") };
    format!("{verdict}\nsccache servers:\n{}", sccache_servers())
}

fn held_by(file: &Path) -> Option<String> {
    let listing = capture("lsof", [OsStr::new("-Fp"), OsStr::new("--"), file.as_os_str()]);
    let holders = listing.lines().filter_map(|line| line.strip_prefix('p')).map(holder_line).collect::<Vec<_>>();
    if holders.is_empty() { None } else { Some(format!("{}:\n{}", file.display(), holders.join("\n"))) }
}

fn explicit_variable(command: Option<&Command>, name: &str) -> Option<OsString> {
    command.and_then(|command| command.get_envs().find(|(key, _)| *key == name)).and_then(|(_, value)| value.map(OsString::from))
}

fn child_pids() -> Vec<u32> {
    let own = std::process::id();
    let listing = capture("ps", ["-axo", "pid=,ppid="]);
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace().filter_map(|column| column.parse::<u32>().ok());
            Some((columns.next()?, columns.next()?))
        })
        .filter(|(_, parent)| *parent == own)
        .map(|(pid, _)| pid)
        .collect()
}

#[cfg(target_os = "linux")]
fn process_variable(pid: u32, name: &str) -> Option<OsString> {
    let environment = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{name}=");
    environment.split(|byte| *byte == 0).find_map(|entry| entry.strip_prefix(prefix.as_bytes()).map(|value| OsString::from(String::from_utf8_lossy(value).into_owned())))
}

// `ps eww` prints the environment after argv on macOS; a value containing a space is cut at the space
#[cfg(not(target_os = "linux"))]
fn process_variable(pid: u32, name: &str) -> Option<OsString> {
    let pid_text = pid.to_string();
    let listing = capture("ps", ["eww", "-o", "command=", "-p", pid_text.as_str()]);
    let prefix = format!("{name}=");
    listing.split_whitespace().find_map(|token| token.strip_prefix(prefix.as_str()).map(OsString::from))
}

fn variable_candidates(command: Option<&Command>, leaders: &[u32], name: &str) -> BTreeSet<OsString> {
    let explicit = explicit_variable(command, name);
    let from_processes = leaders.iter().filter_map(|pid| process_variable(*pid, name));
    explicit.into_iter().chain(from_processes).chain(std::env::var_os(name)).collect()
}

const PACKAGE_CACHE_LOCKS: [&str; 2] = [".package-cache", ".package-cache-mutate"];
const BUILD_LOCKS: [&str; 4] = [".cargo-lock", "debug/.cargo-lock", "debug/.cargo-build-lock", "debug/.cargo-artifact-lock"];

fn lock_files(command: Option<&Command>, leader: Option<u32>) -> BTreeSet<PathBuf> {
    let leaders = leader.map_or_else(child_pids, |pid| vec![pid]);
    let mut homes: BTreeSet<PathBuf> = variable_candidates(command, &leaders, "CARGO_HOME").into_iter().map(PathBuf::from).collect();
    if homes.is_empty() {
        homes.extend(variable_candidates(command, &leaders, "HOME").into_iter().map(|home| PathBuf::from(home).join(".cargo")));
    }
    let targets: BTreeSet<PathBuf> = variable_candidates(command, &leaders, "CARGO_TARGET_DIR").into_iter().map(PathBuf::from).collect();
    let package_cache = homes.iter().flat_map(|home| PACKAGE_CACHE_LOCKS.map(|name| home.join(name)));
    let build_directory = targets.iter().flat_map(|target| BUILD_LOCKS.map(|name| target.join(name)));
    package_cache.chain(build_directory).collect()
}

fn holder_line(pid: &str) -> String {
    let directory = capture("lsof", ["-a", "-d", "cwd", "-Fn", "-p", pid]);
    let cwd = directory.lines().find_map(|line| line.strip_prefix('n')).unwrap_or("unknown");
    let argv = capture("ps", ["-o", "args=", "-p", pid]);
    format!("  pid {pid} cwd {cwd} argv {}", argv.trim())
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
