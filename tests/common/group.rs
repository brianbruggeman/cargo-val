use core::future::Future;
#[cfg(unix)]
use core::future::ready;
use core::pin::pin;
#[cfg(unix)]
use std::io;
use std::time::Duration;

#[cfg(windows)]
use cargo_val::edge_exit_windows::WindowsExit;
#[cfg(windows)]
use cargo_val::edge_signal_windows::release_group;
#[cfg(windows)]
use cargo_val::edge_spawn_windows::spawn_in_group;
use proxima_core::signal::Signal;
use proxima_core::time::timeout;
#[cfg(windows)]
use proxima_primitives::pipe::Pipe;

pub mod settings {
    include!("settings.rs");
}

pub(crate) fn interrupt_grace() -> Duration {
    settings::bound("INTERRUPT_GRACE")
}

#[cfg(windows)]
fn taskkill_deadline() -> Duration {
    settings::bound("TASKKILL_DEADLINE")
}

/// Races `work` against `deadline` with [`timeout`]. On expiry a [`Signal`] fires and its cancel arm interrupts, grace-waits and kills `leader`'s process group while `work` drains; the error names the `CARGO_VAL_TEST_<setting>` that set the bound and carries what `work` produced.
///
/// # Errors
///
/// The expiry report and the output `work` finished with once the group was gone.
pub async fn bounded<Work: Future>(leader: u32, deadline: Duration, setting: &str, work: Work) -> Result<Work::Output, (String, Work::Output)> {
    let mut work = pin!(work);
    if let Ok(done) = timeout(deadline, work.as_mut()).await {
        return Ok(done);
    }
    let expiry = Signal::new();
    expiry.fire();
    expiry.fired().await;
    let (drained, failures) = interrupt_then_kill(leader, work.as_mut()).await;
    Err((
        format!("pid {leader} outlived {deadline:?} and its process group was killed (signal failures: {failures:?})\nraise CARGO_VAL_TEST_{setting} if the work is only slow"),
        drained,
    ))
}

/// SIGINT to the group, [`interrupt_grace()`] for `collect` to finish on its own, then SIGKILL to the group and the rest of `collect`.
pub async fn interrupt_then_kill<Collect>(leader: u32, mut collect: Collect) -> (Collect::Output, Vec<String>)
where
    Collect: Future + Unpin,
{
    let mut failures: Vec<String> = interrupt(leader).await.err().into_iter().collect();
    let graceful = timeout(interrupt_grace(), &mut collect).await;
    failures.extend(kill_process_group(leader).await.err());
    match graceful {
        Ok(finished) => (finished, failures),
        Err(_) => (collect.await, failures),
    }
}

#[cfg(unix)]
pub fn interrupt(leader: u32) -> impl Future<Output = Result<(), String>> {
    ready(signal_group(leader, libc::SIGINT))
}

#[cfg(unix)]
pub fn kill_process_group(leader: u32) -> impl Future<Output = Result<(), String>> {
    ready(signal_group(leader, libc::SIGKILL))
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
pub fn interrupt(leader: u32) -> impl Future<Output = Result<(), String>> {
    kill_process_group(leader)
}

// taskkill /T walks the parent chain, the closest std-only reach to a job object; its exit is a pipe raced against taskkill_deadline() so a hung taskkill is reported instead of blocking
#[cfg(windows)]
pub async fn kill_process_group(leader: u32) -> Result<(), String> {
    let target = leader.to_string();
    let arguments: [&[u8]; 4] = [b"/PID", target.as_bytes(), b"/T", b"/F"];
    let inherited: Vec<Vec<u8>> = std::env::vars_os().map(|(key, value)| [key.as_encoded_bytes(), b"=", value.as_encoded_bytes()].concat()).collect();
    let inherited: Vec<&[u8]> = inherited.iter().map(Vec::as_slice).collect();
    let child = spawn_in_group(b"taskkill", &arguments, &inherited, &[], None).map_err(|error| format!("taskkill for pid {leader} did not run: {error:?}"))?;
    let exit: WindowsExit = child.exit_source().map_err(|error| format!("taskkill for pid {leader} has no exit source: {error}"))?;
    let (killer, _stdout, _stderr) = child.into_parts();
    let waited = timeout(taskkill_deadline(), exit.call(())).await;
    release_group(killer);
    match waited {
        Ok(Ok(Ok(status))) if status.success() => Ok(()),
        Ok(Ok(Ok(status))) => Err(format!("taskkill for pid {leader} exited {status}")),
        Ok(Ok(Err(error))) => Err(format!("taskkill for pid {leader} could not be waited on: {error}")),
        Ok(Err(_)) => Err(format!("taskkill for pid {leader}: the exit source was already spent")),
        Err(_) => Err(format!("taskkill for pid {leader} did not exit within {:?}", taskkill_deadline())),
    }
}
