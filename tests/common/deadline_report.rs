use core::time::Duration;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

const REPORT_TAIL: usize = 64 * 1024;

/// The one writer every missed test deadline goes through: snapshots the process group of `leader` (when the deadline bounded a child) and the build-directory lock holders, writes `$HOME/.cache/cargo-val-deadlines/<ts>-<pid>-<test>.log`, and returns that snapshot with the `deadline report: <path>` line the failure message must end on.
///
/// `command` supplies the `CARGO_HOME` and `CARGO_TARGET_DIR` the child ran with; without one the test's own environment is read.
pub fn persist_deadline_report(deadline: Duration, leader: Option<u32>, command: Option<&Command>, stdout: &[u8], stderr: &[u8]) -> (String, String) {
    let group = leader.map_or_else(|| "  no child: the deadline bounded a stream".to_string(), group_processes);
    let locks = lock_report(command);
    let snapshot = format!("process group at the deadline:\n{group}\nlock holders at the deadline:\n{locks}");
    let name = current_test_name();
    let report = format!(
        "test: {name}\ndeadline: {deadline:?}\nuptime: {}\n{snapshot}\nsccache --show-stats:\n{}\nstdout (last {REPORT_TAIL} bytes):\n{}\nstderr (last {REPORT_TAIL} bytes):\n{}\n",
        capture("uptime", [] as [&str; 0]).trim(),
        sccache_stats(),
        tail(stdout),
        tail(stderr),
    );
    (snapshot, persist_report(&name, &report))
}

fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(REPORT_TAIL)..]).into_owned()
}

// nextest runs one test per process as `--exact <name>`; libtest names its thread after the test; the executable stem is the last resort
fn current_test_name() -> String {
    let arguments = std::env::args().collect::<Vec<_>>();
    let exact = arguments.iter().position(|argument| argument == "--exact").and_then(|index| arguments.get(index + 1)).cloned();
    let thread = std::thread::current().name().filter(|name| *name != "main").map(str::to_owned);
    let executable = std::env::current_exe().ok().and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()));
    exact.or(thread).or(executable).unwrap_or_else(|| "unknown-test".to_string())
}

fn sccache_stats() -> String {
    match Command::new("sccache").arg("--show-stats").output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "sccache is not on PATH".to_string(),
        Err(error) => format!("sccache did not run: {error}"),
    }
}

// the report must outlive the agent's scratch logs; a failed write is reported in the message instead of replacing the test failure
fn persist_report(name: &str, report: &str) -> String {
    match write_report(name, report) {
        Ok(path) => format!("deadline report: {}", path.display()),
        Err(error) => {
            eprintln!("deadline report not written: {error}");
            format!("deadline report not written: {error}")
        }
    }
}

fn write_report(name: &str, report: &str) -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    let directory = PathBuf::from(home).join(".cache/cargo-val-deadlines");
    std::fs::create_dir_all(&directory)?;
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let safe = name.chars().map(|character| if character.is_ascii_alphanumeric() || character == '-' || character == '_' { character } else { '_' }).collect::<String>();
    let path = directory.join(format!("{seconds}-{}-{safe}.log", std::process::id()));
    std::fs::write(&path, report)?;
    Ok(path)
}

// the group as the deadline found it: which of cargo, rustc and the test were alive and in what state is what separates a build that is waiting from a build that is stuck
fn group_processes(leader: u32) -> String {
    let listing = Command::new("ps").args(["-axo", "pgid,pid,ppid,stat,etime,command"]).output();
    match listing {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.split_whitespace().next().is_some_and(|pgid| pgid.parse() == Ok(leader)))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("ps did not run: {error}"),
    }
}

// a fixture compile that waits on cargo's build-directory lock or the package-cache lock looks identical to a stuck rustc until the holder is named; the holder is usually another worktree's cargo
fn lock_report(command: Option<&Command>) -> String {
    let held = lock_files(command)
        .into_iter()
        .filter(|file| file.exists())
        .map(|file| format!("{}:\n{}", file.display(), holders_of(&file)))
        .collect::<Vec<_>>();
    format!("{}\nsccache servers:\n{}", held.join("\n"), sccache_servers())
}

fn command_variable(command: Option<&Command>, name: &str) -> Option<OsString> {
    match command.and_then(|command| command.get_envs().find(|(key, _)| *key == name)) {
        Some((_, value)) => value.map(OsString::from),
        None => std::env::var_os(name),
    }
}

fn lock_files(command: Option<&Command>) -> Vec<PathBuf> {
    let cargo_home = command_variable(command, "CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| command_variable(command, "HOME").map(|home| PathBuf::from(home).join(".cargo")));
    let package_cache = cargo_home.into_iter().flat_map(|home| [home.join(".package-cache"), home.join(".package-cache-mutate")]);
    let build_directory = command_variable(command, "CARGO_TARGET_DIR").into_iter().flat_map(|target| {
        let target = PathBuf::from(target);
        [target.join(".cargo-lock"), target.join("debug/.cargo-lock"), target.join("debug/.cargo-build-lock"), target.join("debug/.cargo-artifact-lock")]
    });
    build_directory.chain(package_cache).collect()
}

fn holders_of(file: &Path) -> String {
    let listing = capture("lsof", [OsStr::new("-Fp"), OsStr::new("--"), file.as_os_str()]);
    let holders = listing.lines().filter_map(|line| line.strip_prefix('p')).map(holder_line).collect::<Vec<_>>();
    if holders.is_empty() { "  no holder".to_string() } else { holders.join("\n") }
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
fn capture<Argument: AsRef<OsStr>>(program: &str, arguments: impl IntoIterator<Item = Argument>) -> String {
    match Command::new(program).args(arguments).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(error) => format!("{program} did not run: {error}"),
    }
}
