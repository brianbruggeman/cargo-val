use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use super::group::{lead_group, watchdog};

/// `Command::output` with stdout and stderr read concurrently, so neither pipe can fill and stall the child, and a `deadline` that kills the child's whole process group.
///
/// # Errors
///
/// Returns the spawn or read failure, or the deadline report carrying everything the child printed.
pub fn output_bounded(command: &mut Command, deadline: Duration) -> Result<Output, String> {
    lead_group(command).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("{command:?} did not spawn: {error}"))?;
    let pid = child.id();
    let (cancel, fired) = watchdog(pid, deadline);
    let stdout = child.stdout.take().ok_or("stdout was not piped")?;
    let stderr = child.stderr.take().ok_or("stderr was not piped")?;
    let (waited, stdout, stderr) = thread::scope(|scope| {
        let stdout = scope.spawn(move || read_all(stdout));
        let stderr = scope.spawn(move || read_all(stderr));
        let waited = child.wait();
        (waited, stdout.join(), stderr.join())
    });
    drop(cancel);
    let expired = fired.join().unwrap_or(false);
    let stdout = stdout.map_err(|_| "the stdout reader panicked")?.map_err(|error| format!("stdout read failed: {error}"))?;
    let stderr = stderr.map_err(|_| "the stderr reader panicked")?.map_err(|error| format!("stderr read failed: {error}"))?;
    if expired {
        return Err(format!(
            "{command:?} (pid {pid}) outlived {deadline:?} and its process group was killed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr),
        ));
    }
    let status = waited.map_err(|error| format!("{command:?}: wait failed: {error}"))?;
    Ok(Output { status, stdout, stderr })
}

fn read_all(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}
