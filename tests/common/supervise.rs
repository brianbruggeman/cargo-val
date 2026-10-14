use core::time::Duration;
use std::ffi::{OsStr, OsString};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;

use futures::FutureExt;
use proxima_core::time::timeout;
use proxima_primitives::pipe::UnpinPipe;

use crate::common::group::{interrupt_then_kill, kill_process_group};
use crate::common::{ChildStream, ExitSource, launch, release_group};

/// Spawns `command` as the leader of its own process group and lends `interact` the child's pid, its stdout and stderr streams and its exit source, all driven on the calling prime worker; then guarantees the group is gone before returning or unwinding.
///
/// Where [`crate::common::output_until`] owns the whole run, this is for tests that must watch the child while it runs. A caller panic, a `deadline` missed by `interact`, or a child left running all end the same way: SIGINT to the group, `interrupt_grace()` for cargo-val to forward it, then SIGKILL, so no cargo or cargo-val child outlives the test and holds cargo's build lock.
pub async fn supervised<Outcome>(
    command: &mut Command,
    deadline: Duration,
    interact: impl AsyncFnOnce(u32, &ChildStream, &ChildStream, &ExitSource) -> Outcome,
) -> Outcome {
    let (leader, stdout, stderr, exit) = launch(command).unwrap_or_else(|failure| panic!("{failure}"));
    let body = AssertUnwindSafe(interact(leader, &stdout, &stderr, &exit)).catch_unwind();
    let outcome = timeout(deadline, body).await;
    let group = if outcome.is_err() { group_processes(leader) } else { String::new() };
    let locks = if outcome.is_err() { lock_report(command) } else { String::new() };
    let failures = settle(leader, &exit).await;
    release_group(leader);
    match outcome {
        Ok(Ok(value)) => {
            assert!(failures.is_empty(), "{command:?} (pid {leader}) group cleanup failed: {failures:?}");
            value
        }
        Ok(Err(payload)) => panic::resume_unwind(payload),
        Err(elapsed) => {
            let message = format!(
                "{command:?} (pid {leader}) outlived {deadline:?} ({elapsed:?}); group cleanup: {failures:?}\nprocess group at the deadline:\n{group}\nlock holders at the deadline:\n{locks}\nstdout so far:\n{}\nstderr so far:\n{}",
                String::from_utf8_lossy(&stdout.retained()),
                String::from_utf8_lossy(&stderr.retained()),
            );
            let name = current_test_name();
            let report = persisted_report(&name, deadline, &group, &locks, &tail(&stdout.retained()), &tail(&stderr.retained()));
            panic!("{message}\n{}", persist_report(&name, &report));
        }
    }
}

const REPORT_TAIL: usize = 64 * 1024;

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

fn persisted_report(name: &str, deadline: Duration, group: &str, locks: &str, stdout: &str, stderr: &str) -> String {
    let uptime = capture("uptime", [] as [&str; 0]);
    let sccache = sccache_stats();
    format!(
        "test: {name}\ndeadline: {deadline:?}\nuptime: {}\nprocess group at the deadline:\n{group}\nlock holders at the deadline:\n{locks}\nsccache --show-stats:\n{sccache}\nstdout (last {REPORT_TAIL} bytes):\n{stdout}\nstderr (last {REPORT_TAIL} bytes):\n{stderr}\n",
        uptime.trim(),
    )
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
pub fn lock_report(command: &Command) -> String {
    let held = lock_files(command)
        .into_iter()
        .filter(|file| file.exists())
        .map(|file| format!("{}:\n{}", file.display(), holders_of(&file)))
        .collect::<Vec<_>>();
    format!("{}\nsccache servers:\n{}", held.join("\n"), sccache_servers())
}

fn command_variable(command: &Command, name: &str) -> Option<OsString> {
    match command.get_envs().find(|(key, _)| *key == name) {
        Some((_, value)) => value.map(OsString::from),
        None => std::env::var_os(name),
    }
}

fn lock_files(command: &Command) -> Vec<PathBuf> {
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

// a spent source or a ready status means the exit was already taken; only a pending one means the child still runs
async fn settle(leader: u32, exit: &ExitSource) -> Vec<String> {
    if UnpinPipe::call(exit, ()).now_or_never().is_some() {
        return kill_process_group(leader).await.err().into_iter().collect();
    }
    interrupt_then_kill(leader, UnpinPipe::call(exit, ())).await.1
}
