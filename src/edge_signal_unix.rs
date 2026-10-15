//! Turns SIGINT and SIGTERM into readable bytes so the event loop can forward them to every child it spawned.

use core::cell::{Cell, RefCell};
use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, Ordering};
use core::task::{Context, Poll};
use std::io;
use std::os::fd::OwnedFd;

use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::signal::Signal;

use crate::build_defaults::SIGNAL_SLOTS;
use crate::edge_parent_unix;

const FORWARDED: [c_int; 2] = [libc::SIGINT, libc::SIGTERM];
const RESIZE: c_int = libc::SIGWINCH;
const TARGET_CAPACITY: usize = SIGNAL_SLOTS;
const CONVENTIONAL_BASE: i32 = 128;
const RESERVED: c_int = c_int::MIN;

/// The refusal code of a full table.
pub const TABLE_FULL: c_int = libc::ENOSPC;

// the handler can only reach process-global state; -1 means no pipe is installed
static WRITE_END: AtomicI32 = AtomicI32::new(-1);

// every live child this process spawned: a pid, or a negated pgid for a child that leads its own group; 0 means free, RESERVED means a spawn is about to fill it
static TARGETS: [AtomicI32; TARGET_CAPACITY] = [const { AtomicI32::new(0) }; TARGET_CAPACITY];

/// A self-pipe whose read end a prime gate watches and whose write end the handler feeds.
pub struct SignalPipe {
    read_end: c_int,
    write_end: c_int,
    installed: bool,
    received: Cell<Option<c_int>>,
    resized: Cell<bool>,
    interrupt: Signal,
    parent: RefCell<Option<(Readiness, Option<OwnedFd>)>>,
    parent_armed: Cell<bool>,
}

/// Claims a table slot before a child is spawned, so a full table refuses the spawn instead of leaving a live untracked child.
///
/// Reservations are interchangeable: [`track_child`] or [`track_group`] fills one, [`abandon_slot`] frees one.
///
/// # Errors
///
/// Returns `ENOSPC` when every slot is taken.
pub fn reserve_slot() -> Result<(), c_int> {
    replace(0, RESERVED).then_some(()).ok_or(TABLE_FULL)
}

/// Frees a slot claimed by [`reserve_slot`] when the spawn that wanted it failed.
pub fn abandon_slot() {
    replace(RESERVED, 0);
}

/// Makes every later signal reach the child with process id `target`, filling a slot [`reserve_slot`] claimed.
///
/// # Errors
///
/// Returns `EINVAL` when no slot was reserved for the child.
pub fn track_child(target: u32) -> Result<(), c_int> {
    track(target.cast_signed())
}

/// Makes every later signal reach the process group a child leads, which has process id `target`, filling a slot [`reserve_slot`] claimed.
///
/// # Errors
///
/// Returns `EINVAL` when no slot was reserved for the group.
pub fn track_group(target: u32) -> Result<(), c_int> {
    track(-target.cast_signed())
}

/// Stops forwarding to the child with process id `target`; call it as soon as the child is reaped so a recycled pid is never signalled.
pub fn release_child(target: u32) {
    release(target.cast_signed());
}

/// Stops forwarding to the process group led by `target`.
pub fn release_group(target: u32) {
    release(-target.cast_signed());
}

/// Sends SIGINT to the whole process group `target` leads.
pub fn interrupt_group(target: u32) {
    // safety: killpg takes plain integers and reports failure through its status.
    unsafe { libc::killpg(target.cast_signed(), libc::SIGINT) };
}

fn track(target: libc::pid_t) -> Result<(), c_int> {
    replace(RESERVED, target).then_some(()).ok_or(libc::EINVAL)
}

fn release(target: libc::pid_t) {
    replace(target, 0);
}

fn replace(from: libc::pid_t, to: libc::pid_t) -> bool {
    TARGETS.iter().any(|slot| slot.compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst).is_ok())
}

/// The `128 + signal` status a shell reports for a process ended by that signal.
#[must_use]
pub const fn conventional_status(signal: c_int) -> i32 {
    CONVENTIONAL_BASE + signal
}

impl SignalPipe {
    /// Installs the SIGINT and SIGTERM handlers behind a self-pipe.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value of the failing pipe or `sigaction` call, or
    /// `EBUSY` when another `SignalPipe` is already installed in this process.
    pub fn install() -> Result<Self, c_int> {
        let (read_end, write_end) = create_pipe()?;
        let mut pipe = Self {
            read_end,
            write_end,
            installed: false,
            received: Cell::new(None),
            resized: Cell::new(false),
            interrupt: Signal::new(),
            parent: RefCell::new(None),
            parent_armed: Cell::new(false),
        };
        WRITE_END
            .compare_exchange(-1, write_end, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| libc::EBUSY)?;
        pipe.installed = true;
        for signal in FORWARDED.into_iter().chain([RESIZE]) {
            install_handler(signal)?;
        }
        Ok(pipe)
    }

    /// A prime gate that opens while a signal byte waits to be drained.
    ///
    /// # Errors
    ///
    /// Never fails on this OS; the result matches the edges whose gate duplicates a handle.
    pub fn gate(&self) -> io::Result<Readiness> {
        Ok(Readiness::new(self.read_end))
    }

    /// Watches the parent process on the prime reactor; once it exits, [`poll_parent`](Self::poll_parent) kills every tracked child and exits.
    ///
    /// A process already adopted by init has no parent to lose and watches nothing.
    ///
    /// # Errors
    ///
    /// Returns the OS error from opening the exit gate.
    pub fn watch_parent(&self) -> io::Result<()> {
        match edge_parent_unix::open() {
            Ok(gate) => {
                *self.parent.borrow_mut() = gate;
                Ok(())
            }
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => edge_parent_unix::end_run(),
            Err(error) => Err(error),
        }
    }

    /// One arm of the run's merge: pending while the parent lives, divergent once it exits, ready only with a gate fault.
    ///
    /// The first `Retry` of a fresh gate is the registration epoch, not an exit, so it is skipped once.
    pub fn poll_parent(&self, context: &Context<'_>) -> Poll<io::Error> {
        let mut slot = self.parent.borrow_mut();
        let Some((gate, _pidfd)) = slot.as_mut() else {
            return Poll::Pending;
        };
        loop {
            match gate.poll(context) {
                Ok(ReadyState::Parked) => return Poll::Pending,
                Ok(ReadyState::Retry) if self.parent_armed.replace(true) => edge_parent_unix::end_run(),
                Ok(ReadyState::Retry) => {}
                Ok(ReadyState::OffWorker) => return Poll::Ready(io::Error::other("parent gate polled off a prime worker")),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => edge_parent_unix::end_run(),
                Err(error) => return Poll::Ready(error),
            }
        }
    }

    /// Fires once, when [`drain`](Self::drain) first forwards a signal.
    #[must_use]
    pub const fn interrupt(&self) -> &Signal {
        &self.interrupt
    }

    /// The first signal observed by [`drain`](Self::drain), if any.
    #[must_use]
    pub fn received(&self) -> Option<c_int> {
        self.received.get()
    }

    /// Reads every pending signal byte and forwards each signal to every registered group.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value of a failing read.
    pub fn drain(&self) -> Result<(), c_int> {
        let mut bytes = [0_u8; 16];
        loop {
            let count =
                unsafe { libc::read(self.read_end, bytes.as_mut_ptr().cast(), bytes.len()) };
            let Ok(count) = usize::try_from(count) else {
                return match last_error() {
                    libc::EINTR => continue,
                    libc::EAGAIN => Ok(()),
                    code => Err(code),
                };
            };
            if count == 0 {
                return Ok(());
            }
            bytes[..count]
                .iter()
                .for_each(|byte| self.dispatch(c_int::from(*byte)));
        }
    }

    fn dispatch(&self, signal: c_int) {
        if signal == RESIZE {
            self.resized.set(true);
        } else {
            self.forward(signal);
        }
    }

    /// Whether the terminal was resized since the last call; the render state wants a new width.
    pub fn take_resized(&self) -> bool {
        self.resized.replace(false)
    }

    /// Records SIGINT as received the way the handler would, for a key that asks for the same interrupt.
    pub fn raise_interrupt(&self) {
        self.forward(libc::SIGINT);
    }

    /// Asks every tracked child to stop without recording a received signal, for a quit key: the run ends as a quit, not as an interrupt.
    pub fn stop_children(&self) {
        signal_children(libc::SIGINT);
    }

    fn forward(&self, signal: c_int) {
        if self.received.get().is_none() {
            self.received.set(Some(signal));
            self.interrupt.fire();
        }
        signal_children(signal);
    }
}

/// Sends `signal` to every child and child group this process still tracks.
///
/// Reads only the atomic table, so any thread may call it, including one that outlives a blocked event loop.
pub fn signal_children(signal: c_int) {
    TARGETS
        .iter()
        .map(|target| target.load(Ordering::SeqCst))
        .filter(|target| !matches!(*target, 0 | RESERVED))
        .for_each(|target| {
            unsafe { libc::kill(target, signal) };
        });
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        if self.installed {
            for signal in FORWARDED.into_iter().chain([RESIZE]) {
                unsafe { libc::signal(signal, libc::SIG_DFL) };
            }
            WRITE_END.store(-1, Ordering::SeqCst);
        }
        unsafe {
            libc::close(self.read_end);
            libc::close(self.write_end);
        }
    }
}

extern "C" fn record_signal(signal: c_int) {
    // errno is thread state the interrupted code may be about to read
    let saved = unsafe { *errno_slot() };
    let write_end = WRITE_END.load(Ordering::Relaxed);
    if write_end >= 0
        && let Ok(byte) = u8::try_from(signal)
    {
        unsafe { libc::write(write_end, (&raw const byte).cast(), 1) };
    }
    unsafe { *errno_slot() = saved };
}

fn install_handler(signal: c_int) -> Result<(), c_int> {
    let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
    action.sa_sigaction = record_signal as extern "C" fn(c_int) as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
    if unsafe { libc::sigaction(signal, &raw const action, core::ptr::null_mut()) } < 0 {
        return Err(last_error());
    }
    Ok(())
}

fn create_pipe() -> Result<(c_int, c_int), c_int> {
    let mut ends = [-1; 2];
    if unsafe { libc::pipe(ends.as_mut_ptr()) } < 0 {
        return Err(last_error());
    }
    for end in ends {
        let flagged = unsafe { libc::fcntl(end, libc::F_SETFD, libc::FD_CLOEXEC) } >= 0
            && unsafe { libc::fcntl(end, libc::F_SETFL, libc::O_NONBLOCK) } >= 0;
        if !flagged {
            let code = last_error();
            unsafe {
                libc::close(ends[0]);
                libc::close(ends[1]);
            }
            return Err(code);
        }
    }
    Ok((ends[0], ends[1]))
}

#[cfg(target_os = "macos")]
fn errno_slot() -> *mut c_int {
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
fn errno_slot() -> *mut c_int {
    unsafe { libc::__errno_location() }
}

fn last_error() -> c_int {
    unsafe { *errno_slot() }
}
