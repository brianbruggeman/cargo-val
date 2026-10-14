//! Ends the process when the parent that launched it dies.
//!
//! An orphan has no reader left: its output goes nowhere and its caller will never signal it, so a run that waits on a child
//! or blocks in a synchronous write can outlive its caller for good. The event loop cannot notice, because the blocked thread
//! is the one that would have to look. A dedicated blocking thread owns the wait instead: a `kqueue` `EVFILT_PROC` on macOS,
//! a pidfd (`prime::os::child::open_pidfd`) on Linux.

use core::ffi::c_int;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(target_os = "macos")]
use std::os::fd::FromRawFd;
use std::thread;

use crate::edge_signal_unix::{conventional_status, signal_children};

const WATCHER_NAME: &str = "cargo-val-parent-watch";
const INIT: libc::pid_t = 1;

/// Starts the parent watch; once the parent exits, every tracked child is killed and the process exits with the status of `SIGHUP`.
///
/// A process already adopted by init has no parent to lose, so it starts no watch. Call it once, before the run spawns anything.
///
/// # Errors
///
/// Returns the OS error from opening the exit gate or spawning the watch thread.
pub fn watch() -> io::Result<()> {
    let parent = unsafe { libc::getppid() };
    if parent <= INIT {
        return Ok(());
    }
    let gate = match open_gate(parent) {
        Ok(gate) => gate,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => end_run(),
        Err(error) => return Err(error),
    };
    thread::Builder::new().name(WATCHER_NAME.into()).spawn(move || match await_exit(&gate) {
        Ok(()) => end_run(),
        Err(error) => {
            let _ = writeln!(io::stderr(), "cargo-val: parent watch failed: {error}");
        }
    })?;
    Ok(())
}

fn end_run() -> ! {
    signal_children(libc::SIGKILL);
    unsafe { libc::_exit(conventional_status(libc::SIGHUP)) }
}

#[cfg(target_os = "macos")]
fn open_gate(parent: libc::pid_t) -> io::Result<OwnedFd> {
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return Err(io::Error::last_os_error());
    }
    let queue = unsafe { OwnedFd::from_raw_fd(queue) };
    let mut change: libc::kevent = unsafe { core::mem::zeroed() };
    change.ident = usize::try_from(parent).map_err(io::Error::other)?;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    let status = unsafe { libc::kevent(queue.as_raw_fd(), &raw const change, 1, core::ptr::null_mut(), 0, core::ptr::null()) };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(queue)
}

#[cfg(target_os = "macos")]
fn await_exit(gate: &OwnedFd) -> io::Result<()> {
    let mut event: libc::kevent = unsafe { core::mem::zeroed() };
    loop {
        let ready = unsafe { libc::kevent(gate.as_raw_fd(), core::ptr::null(), 0, &raw mut event, 1, core::ptr::null()) };
        if ready > 0 {
            return Ok(());
        }
        if ready < 0 && last_error() != libc::EINTR {
            return Err(io::Error::last_os_error());
        }
    }
}

#[cfg(target_os = "linux")]
fn open_gate(parent: libc::pid_t) -> io::Result<OwnedFd> {
    let parent = core::num::NonZeroI32::new(parent).ok_or_else(|| io::Error::from_raw_os_error(libc::ESRCH))?;
    prime::os::child::open_pidfd(parent)
}

#[cfg(target_os = "linux")]
fn await_exit(gate: &OwnedFd) -> io::Result<()> {
    let mut watched = libc::pollfd { fd: gate.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        let ready = unsafe { libc::poll(&raw mut watched, 1, -1) };
        if ready > 0 {
            return Ok(());
        }
        if ready < 0 && last_error() != libc::EINTR {
            return Err(io::Error::last_os_error());
        }
    }
}

fn last_error() -> c_int {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
