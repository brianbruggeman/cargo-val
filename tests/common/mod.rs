use core::future::Future;
use core::pin::pin;
use core::time::Duration;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::process::{Command, Output};

use futures::future::{Either, join3, select};
use proxima_core::time::{sleep, timeout};
use proxima_primitives::pipe::{Exhausted, Pipe};

#[cfg(unix)]
use cargo_val::edge_spawn_unix::spawn_in_group;
#[cfg(windows)]
use cargo_val::edge_spawn_windows::spawn_in_group;

#[cfg(unix)]
pub use cargo_val::edge_stream_unix::ChildStream;
#[cfg(windows)]
pub use cargo_val::edge_stream_windows::ChildStream;

#[cfg(target_os = "linux")]
pub use cargo_val::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
pub use cargo_val::edge_exit_macos::MacosExit as ExitSource;
#[cfg(windows)]
pub use cargo_val::edge_exit_windows::WindowsExit as ExitSource;

#[cfg(windows)]
pub(crate) use cargo_val::edge_signal_windows::release_group;

// a cold fixture build under load 85-96 took up to ~63 s in shard_process; 240 s leaves ~3.8x headroom and matches isolated_target's WAIT_LINE_DEADLINE
pub const FIXTURE_DEADLINE: Duration = Duration::from_secs(240);

// a SIGKILLed harness cannot clean up in-process, so every cargo-val it launches carries the cleanup and exits with its parent
#[cfg(unix)]
pub(crate) const EXIT_WITH_PARENT: &str = "CARGO_VAL_EXIT_WITH_PARENT";

// cargo-val forwards SIGINT to its children's groups and exits 130; interrupt_signals waits up to 60 s for the same exit
pub(crate) const INTERRUPT_GRACE: Duration = Duration::from_secs(60);

/// Runs a fixture command to completion, failing the test with the captured output if it outlives [`FIXTURE_DEADLINE`].
pub async fn fixture_output(command: &mut Command) -> Output {
    output_within(command, FIXTURE_DEADLINE).await.unwrap_or_else(|failure| panic!("{failure}"))
}

/// Like `Command::output`, but the child leads its own process group and a missed `deadline` interrupts it, then kills the whole group.
///
/// # Errors
///
/// Returns the spawn failure, or the deadline report carrying everything the child printed.
pub async fn output_within(command: &mut Command, deadline: Duration) -> Result<Output, String> {
    output_until(command, sleep(deadline)).await
}

/// The deadline path of [`output_within`] with the clock replaced by `expire`: the child's exit and both its streams are read on the calling prime worker, and once `expire` resolves first the group is interrupted, given [`INTERRUPT_GRACE`], then killed.
///
/// # Errors
///
/// Returns the spawn failure, or the expiry report carrying everything the child printed and any signal that failed to land.
pub async fn output_until(command: &mut Command, expire: impl Future<Output = ()>) -> Result<Output, String> {
    let (leader, stdout, stderr, exit) = launch(command)?;
    let mut collect = pin!(join3(drain(&stdout), drain(&stderr), exit.call(())));
    let mut expire = pin!(expire);
    let raced = select(collect.as_mut(), expire.as_mut()).await;
    let (finished, signal_failures) = match raced {
        Either::Left((finished, _)) => (finished, None),
        Either::Right(((), _)) => {
            let (finished, failures) = interrupt_then_kill(leader, collect.as_mut()).await;
            (finished, Some(failures))
        }
    };
    release_group(leader);
    let ((stdout, stdout_fault), (stderr, stderr_fault), waited) = finished;
    let read_faults: Vec<String> = [("stdout", stdout_fault), ("stderr", stderr_fault)]
        .into_iter()
        .filter_map(|(stream, fault)| fault.map(|error| format!("{stream} read failed: {error}")))
        .collect();
    let status = waited
        .map_err(|Exhausted| format!("{command:?}: the exit source was already spent"))?
        .map_err(|error| format!("{command:?}: wait failed: {error}"))?;
    if let Some(failures) = signal_failures {
        return Err(format!(
            "{command:?} (pid {leader}) outlived its deadline and its process group was killed\nsignal failures: {failures:?}\nread faults: {read_faults:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr),
        ));
    }
    if !read_faults.is_empty() {
        return Err(format!("{command:?}: the child exited but its output could not be read: {read_faults:?}"));
    }
    Ok(Output { status, stdout, stderr })
}

/// Spawns `command` as the leader of its own process group through cargo-val's own spawn, and returns the pid with the child's stdout and stderr streams and its exit source.
pub(crate) fn launch(command: &Command) -> Result<(u32, Box<ChildStream>, Box<ChildStream>, ExitSource), String> {
    let changes: BTreeMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();
    let inherited: Vec<Vec<u8>> = std::env::vars_os()
        .filter(|(key, _)| !changes.contains_key(key.as_os_str()))
        .map(|(key, value)| environment_entry(&key, &value))
        .collect();
    let mut overrides: Vec<Vec<u8>> = changes
        .iter()
        .filter_map(|(key, value)| value.map(|value| environment_entry(key, value)))
        .collect();
    overrides.extend(exit_with_parent_entry(&changes));
    let arguments: Vec<&[u8]> = command.get_args().map(OsStr::as_encoded_bytes).collect();
    let inherited: Vec<&[u8]> = inherited.iter().map(Vec::as_slice).collect();
    let overrides: Vec<&[u8]> = overrides.iter().map(Vec::as_slice).collect();
    let directory = command.get_current_dir().map(|path| path.as_os_str().as_encoded_bytes());
    let child = spawn_in_group(command.get_program().as_encoded_bytes(), &arguments, &inherited, &overrides, directory)
        .map_err(|error| format!("{command:?} did not spawn: {error:?}"))?;
    let exit = child.exit_source().map_err(|error| format!("{command:?} has no exit source: {error}"))?;
    let (leader, stdout, stderr) = child.into_parts();
    // boxed: each stream owns a capture window and scratch tail, together ~40 KB, which would make every test future that awaits a run trip clippy::large_futures
    Ok((leader, Box::new(ChildStream::new(stdout)), Box::new(ChildStream::new(stderr)), exit))
}

// windows has no parent watch, so the opt-in would be refused there
#[cfg(unix)]
fn exit_with_parent_entry(changes: &BTreeMap<&OsStr, Option<&OsStr>>) -> Option<Vec<u8>> {
    (!changes.contains_key(OsStr::new(EXIT_WITH_PARENT))).then(|| environment_entry(OsStr::new(EXIT_WITH_PARENT), OsStr::new("1")))
}

#[cfg(not(unix))]
fn exit_with_parent_entry(_changes: &BTreeMap<&OsStr, Option<&OsStr>>) -> Option<Vec<u8>> {
    None
}

fn environment_entry(key: &OsStr, value: &OsStr) -> Vec<u8> {
    [key.as_encoded_bytes(), b"=", value.as_encoded_bytes()].concat()
}

/// Reads `stream` to EOF.
pub async fn drain(stream: &ChildStream) -> (Vec<u8>, Option<io::Error>) {
    drain_until(stream, |_| false).await
}

/// Reads `stream` until `done` accepts everything read so far, or to EOF.
pub async fn drain_until(stream: &ChildStream, done: impl Fn(&[u8]) -> bool) -> (Vec<u8>, Option<io::Error>) {
    let mut bytes = Vec::new();
    while !done(&bytes) {
        match stream.call(()).await {
            Ok(Ok(range)) => match stream.window(range) {
                Some(window) => bytes.extend_from_slice(&window),
                None => return (bytes, Some(io::Error::other("a yielded range reached past the capture window"))),
            },
            Ok(Err(fault)) => return (bytes, Some(fault)),
            Err(Exhausted) => break,
        }
    }
    (bytes, None)
}

/// SIGINT to the group, [`INTERRUPT_GRACE`] for `collect` to finish on its own, then SIGKILL to the group and the rest of `collect`.
pub(crate) async fn interrupt_then_kill<Collect>(leader: u32, mut collect: Collect) -> (Collect::Output, Vec<String>)
where
    Collect: Future + Unpin,
{
    let mut failures: Vec<String> = interrupt(leader).err().into_iter().collect();
    let graceful = timeout(INTERRUPT_GRACE, &mut collect).await;
    failures.extend(kill_group(leader).err());
    match graceful {
        Ok(finished) => (finished, failures),
        Err(_) => (collect.await, failures),
    }
}

#[cfg(unix)]
pub(crate) fn interrupt(leader: u32) -> Result<(), String> {
    signal_group(leader, libc::SIGINT)
}

#[cfg(unix)]
pub(crate) fn kill_group(leader: u32) -> Result<(), String> {
    signal_group(leader, libc::SIGKILL)
}

// ESRCH means the group already exited, which is the goal of both signals
#[cfg(unix)]
fn signal_group(leader: u32, signal: libc::c_int) -> Result<(), String> {
    // safety: killpg takes plain integers and reports failure through its status
    if unsafe { libc::killpg(leader.cast_signed(), signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(format!("killpg({leader}, signal {signal}) failed: {error}")),
    }
}

#[cfg(windows)]
pub(crate) fn interrupt(leader: u32) -> Result<(), String> {
    kill_group(leader)
}

// taskkill /T walks the parent chain, the closest std-only reach to a job object
#[cfg(windows)]
pub(crate) fn kill_group(leader: u32) -> Result<(), String> {
    let output = Command::new("taskkill")
        .args(["/PID", &leader.to_string(), "/T", "/F"])
        .output()
        .map_err(|error| format!("taskkill for pid {leader} did not run: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "taskkill for pid {leader} exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim(),
    ))
}

// windows registers each group's job in a process-wide table of eight at spawn and kills the group when a full table closes it, so a child that is not released leaves the ninth spawn dead on arrival
#[cfg(not(windows))]
pub(crate) fn release_group(_leader: u32) {}
