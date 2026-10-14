use core::time::Duration;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::process::{Command, Output};
use futures::future::join3;
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

#[path = "group.rs"]
pub mod group;
#[path = "drain.rs"]
mod drain;

pub use drain::drain_until;

pub mod settings {
    include!("settings.rs");
}

use group::bounded;

/// Like `Command::output`, but the child leads its own process group and a deadline of `setting` (the `CARGO_VAL_TEST_<setting>` bound) is raced against it by [`bounded`], which interrupts the group, gives it [`interrupt_grace()`], then kills it.
///
/// # Errors
///
/// Returns the spawn failure, or the expiry report carrying everything the child printed and any signal that failed to land.
pub async fn output_within(command: &mut Command, deadline: Duration, setting: &str) -> Result<Output, String> {
    let (leader, stdout, stderr, exit) = launch(command)?;
    let collect = join3(drain(&stdout), drain(&stderr), exit.call(()));
    let settled = bounded(leader, deadline, setting, collect).await;
    release_group(leader);
    match settled {
        Ok(finished) => finish(command, leader, finished, None),
        Err((report, finished)) => finish(command, leader, finished, Some(report)),
    }
}

pub(crate) type Finished = ((Vec<u8>, Option<io::Error>), (Vec<u8>, Option<io::Error>), Result<io::Result<std::process::ExitStatus>, Exhausted>);

pub(crate) fn finish(command: &Command, leader: u32, finished: Finished, expiry: Option<String>) -> Result<Output, String> {
    let ((stdout, stdout_fault), (stderr, stderr_fault), waited) = finished;
    let read_faults: Vec<String> = [("stdout", stdout_fault), ("stderr", stderr_fault)]
        .into_iter()
        .filter_map(|(stream, fault)| fault.map(|error| format!("{stream} read failed: {error}")))
        .collect();
    let status = waited
        .map_err(|Exhausted| format!("{command:?}: the exit source was already spent"))?
        .map_err(|error| format!("{command:?}: wait failed: {error}"))?;
    if let Some(report) = expiry {
        return Err(format!(
            "{command:?} (pid {leader}) outlived its deadline and its process group was killed\n{report}\nread faults: {read_faults:?}\nstdout:\n{}\nstderr:\n{}",
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
    let overrides: Vec<Vec<u8>> = changes
        .iter()
        .filter_map(|(key, value)| value.map(|value| environment_entry(key, value)))
        .collect();
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

fn environment_entry(key: &OsStr, value: &OsStr) -> Vec<u8> {
    [key.as_encoded_bytes(), b"=", value.as_encoded_bytes()].concat()
}

// windows registers each group's job in a process-wide table of eight at spawn and kills the group when a full table closes it, so a child that is not released leaves the ninth spawn dead on arrival
#[cfg(not(windows))]
pub(crate) fn release_group(_leader: u32) {}

/// Reads `stream` to EOF.
pub async fn drain(stream: &ChildStream) -> (Vec<u8>, Option<io::Error>) {
    drain_until(stream, |_| false).await
}
