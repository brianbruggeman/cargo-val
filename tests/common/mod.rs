use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

// a cold fixture build under load 85-96 took up to ~63 s in shard_process; 240 s leaves ~3.8x headroom and matches isolated_target's WAIT_LINE_DEADLINE
pub const FIXTURE_DEADLINE: Duration = Duration::from_secs(240);

// cargo-val forwards SIGINT to its children's groups and exits 130; interrupt_signals waits up to 60 s for the same exit
const INTERRUPT_GRACE: Duration = Duration::from_secs(60);

/// Runs a fixture command to completion, failing the test with the captured output if it outlives [`FIXTURE_DEADLINE`].
pub fn fixture_output(command: &mut Command) -> Output {
    output_within(command, FIXTURE_DEADLINE).unwrap_or_else(|failure| panic!("{failure}"))
}

/// Like `Command::output`, but the child leads its own process group and a missed `deadline` interrupts it, then kills the whole group.
///
/// # Errors
///
/// Returns the spawn failure, or the deadline report carrying everything the child printed.
pub fn output_within(command: &mut Command, deadline: Duration) -> Result<Output, String> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{command:?} did not spawn: {error}"))?;
    let leader = child.id();
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || sender.send(child.wait()));
    let on_time = receiver.recv_timeout(deadline);
    let timed_out = on_time.is_err();
    let waited = on_time.or_else(|_| {
        interrupt(leader);
        let graceful = receiver.recv_timeout(INTERRUPT_GRACE);
        kill_group(leader);
        graceful.or_else(|_| receiver.recv())
    });
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let status = waited
        .map_err(|error| format!("{command:?}: the wait thread vanished: {error}"))?
        .map_err(|error| format!("{command:?}: wait failed: {error}"))?;
    if timed_out {
        return Err(format!(
            "{command:?} (pid {leader}) outlived {deadline:?} and its process group was killed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr),
        ));
    }
    Ok(Output { status, stdout, stderr })
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

#[cfg(unix)]
fn interrupt(leader: u32) {
    // safety: killpg takes plain integers and reports failure through its status
    unsafe { libc::killpg(leader.cast_signed(), libc::SIGINT) };
}

#[cfg(unix)]
fn kill_group(leader: u32) {
    // safety: killpg takes plain integers and reports failure through its status
    unsafe { libc::killpg(leader.cast_signed(), libc::SIGKILL) };
}

#[cfg(windows)]
fn interrupt(leader: u32) {
    kill_group(leader);
}

// taskkill /T walks the parent chain, the closest std-only reach to a job object
#[cfg(windows)]
fn kill_group(leader: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &leader.to_string(), "/T", "/F"])
        .output();
}
