use core::time::Duration;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod lock_report {
    include!("lock_report.rs");
}

static REPORTS_WRITTEN: AtomicU64 = AtomicU64::new(0);

const REPORT_TAIL: usize = 64 * 1024;

/// The one writer every missed test deadline goes through: snapshots the process group of `leader` (when the deadline bounded a child) and the build-directory lock holders, writes `$HOME/.cache/cargo-val-deadlines/<ts>-<pid>-<test>.log`, and returns that snapshot with the `deadline report: <path>` line the failure message must end on.
///
/// `command`, else the live `leader`, supplies the `CARGO_HOME` and `CARGO_TARGET_DIR` the child ran with; see [`lock_report::lock_holders`].
pub fn persist_deadline_report(deadline: Duration, leader: Option<u32>, command: Option<&Command>, stdout: &[u8], stderr: &[u8]) -> (String, String) {
    let group = leader.map_or_else(|| "  no child: the deadline bounded a stream".to_string(), group_processes);
    let locks = lock_report::lock_holders(command, leader);
    let snapshot = format!("process group at the deadline:\n{group}\nlock holders at the deadline:\n{locks}");
    let name = current_test_name();
    let report = format!(
        "test: {name}\ndeadline: {deadline:?}\nuptime: {}\n{snapshot}\nsccache --show-stats:\n{}\nstdout (last {REPORT_TAIL} bytes):\n{}\nstderr (last {REPORT_TAIL} bytes):\n{}\n",
        lock_report::capture("uptime", [] as [&str; 0]).trim(),
        sccache_stats(),
        tail(stdout),
        tail(stderr),
    );
    (snapshot, persist_report(&name, &report))
}

fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(REPORT_TAIL)..]).into_owned()
}

// the body runs on a runtime worker whose thread name is not the test and only nextest passes `--exact`; the macro-generated `__proxima_inner` frame is on the stack of every poll of the body, so it names the test under every runner
fn current_test_name() -> String {
    let frames = std::backtrace::Backtrace::force_capture().to_string();
    let inner = frames.lines().find_map(|line| line.split_once("::__proxima_inner").and_then(|(path, _)| path.rsplit("::").next())).map(str::to_owned);
    let arguments = std::env::args().collect::<Vec<_>>();
    let exact = arguments.iter().position(|argument| argument == "--exact").and_then(|index| arguments.get(index + 1)).cloned();
    let thread = std::thread::current().name().filter(|name| *name != "main").map(str::to_owned);
    let executable = std::env::current_exe().ok().and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()));
    inner.or(exact).or(thread).or(executable).unwrap_or_else(|| "unknown-test".to_string())
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
    let sequence = REPORTS_WRITTEN.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!("{seconds}-{}-{sequence}-{safe}.log", std::process::id()));
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
