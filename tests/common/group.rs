#[cfg(unix)]
use std::io;
use std::process::Command;
#[cfg(windows)]
use std::process::Stdio;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub mod settings {
    include!("settings.rs");
}

/// Deadline for synchronous waits on a child whose run time is not itself under test; `CARGO_VAL_TEST_SYNC_DEADLINE` over the build default.
pub fn sync_deadline() -> Duration {
    settings::bound("SYNC_DEADLINE")
}

#[cfg(windows)]
fn taskkill_deadline() -> Duration {
    settings::bound("TASKKILL_DEADLINE")
}


/// Makes `command` the leader of its own process group so a missed deadline can kill it together with everything it spawned.
pub fn lead_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0200)
    }
}

/// A thread that kills `leader`'s process group once `deadline` passes; dropping the returned sender cancels it, and joining the handle says whether it fired.
pub fn watchdog(leader: u32, deadline: Duration) -> (Sender<()>, JoinHandle<bool>) {
    let (cancel, cancelled) = mpsc::channel::<()>();
    let handle = thread::spawn(move || match cancelled.recv_timeout(deadline) {
        Err(RecvTimeoutError::Timeout) => {
            let _ = kill_process_group(leader);
            true
        }
        Ok(()) | Err(RecvTimeoutError::Disconnected) => false,
    });
    (cancel, handle)
}

#[cfg(unix)]
pub fn kill_process_group(leader: u32) -> Result<(), String> {
    // safety: killpg takes plain integers and reports failure through its status
    if unsafe { libc::killpg(leader.cast_signed(), libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // ESRCH means the group already exited, which is the goal
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(format!("killpg({leader}) failed: {error}")),
    }
}

// taskkill /T walks the parent chain, the closest std-only reach to a job object; its exit is awaited on a thread so a hung taskkill is reported at taskkill_deadline() instead of blocking
#[cfg(windows)]
pub fn kill_process_group(leader: u32) -> Result<(), String> {
    let mut child = Command::new("taskkill")
        .args(["/PID", &leader.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("taskkill for pid {leader} did not run: {error}"))?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(child.wait()));
    match receiver.recv_timeout(taskkill_deadline()) {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("taskkill for pid {leader} exited {status}")),
        Ok(Err(error)) => Err(format!("taskkill for pid {leader} could not be waited on: {error}")),
        Err(_) => Err(format!("taskkill for pid {leader} did not exit within {:?}", taskkill_deadline())),
    }
}

