//! Ends the process when the parent that launched it dies.
//!
//! An orphan has no reader left and its caller will never signal it. The parent is watched as one more source on the run's
//! prime merge: a `Readiness` gate (`Readiness::process_exit` on macOS, a pidfd from `prime::os::child::open_pidfd` on Linux),
//! polled by the same `FanIn` as the tick and exit sources through `SignalPipe::poll_parent`. No thread and no raw event call.
//! `Readiness::process_exit` and a pidfd watch any pid, so unlike `ExitSource` (which reaps) it needs no child relationship.

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;

use prime::os::readiness::Readiness;

use crate::edge_signal_unix::{conventional_status, signal_children};

const INIT: libc::pid_t = 1;

/// The gate and the descriptor it must outlive, or `None` for a process already adopted by init, which has no parent to lose.
pub type ParentGate = Option<(Readiness, Option<OwnedFd>)>;

/// Opens the exit gate for the current parent.
///
/// # Errors
///
/// Returns the OS error from opening the gate; `ESRCH` means the parent is already gone.
pub fn open() -> io::Result<ParentGate> {
    let parent = unsafe { libc::getppid() };
    if parent <= INIT {
        return Ok(None);
    }
    gate(parent).map(Some)
}

/// Kills every tracked child and exits with the status of `SIGHUP`.
pub fn end_run() -> ! {
    signal_children(libc::SIGKILL);
    unsafe { libc::_exit(conventional_status(libc::SIGHUP)) }
}

#[cfg(target_os = "macos")]
fn gate(parent: libc::pid_t) -> io::Result<(Readiness, Option<OwnedFd>)> {
    let parent = core::num::NonZeroI32::new(parent).ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))?;
    Ok((Readiness::process_exit(parent), None))
}

#[cfg(target_os = "linux")]
fn gate(parent: libc::pid_t) -> io::Result<(Readiness, Option<OwnedFd>)> {
    let parent = core::num::NonZeroI32::new(parent).ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))?;
    let pidfd = prime::os::child::open_pidfd(parent)?;
    Ok((Readiness::new(pidfd.as_raw_fd()), Some(pidfd)))
}
