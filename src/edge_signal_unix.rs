//! Turns SIGINT and SIGTERM into readable bytes so the event loop can forward them to every child it spawned.

use core::cell::Cell;
use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, Ordering};

use proxima_core::signal::Signal;

const FORWARDED: [c_int; 2] = [libc::SIGINT, libc::SIGTERM];
const TARGET_CAPACITY: usize = 72;
const CONVENTIONAL_BASE: i32 = 128;

// the handler can only reach process-global state; -1 means no pipe is installed
static WRITE_END: AtomicI32 = AtomicI32::new(-1);

// every live child this process spawned: a pid, or a negated pgid for a child that leads its own group; 0 means free
static TARGETS: [AtomicI32; TARGET_CAPACITY] = [const { AtomicI32::new(0) }; TARGET_CAPACITY];

/// A self-pipe whose read end a prime gate watches and whose write end the handler feeds.
#[derive(Debug)]
pub struct SignalPipe {
    read_end: c_int,
    write_end: c_int,
    installed: bool,
    received: Cell<Option<c_int>>,
    interrupt: Signal,
}

/// Makes every later signal reach `target`: a child's pid, or the negated pid of a child that leads its own process group.
///
/// # Errors
///
/// Returns `ENOSPC` when every slot is taken.
pub fn track_child(target: libc::pid_t) -> Result<(), c_int> {
    TARGETS
        .iter()
        .any(|slot| {
            slot.compare_exchange(0, target, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
        .then_some(())
        .ok_or(libc::ENOSPC)
}

/// Stops forwarding to `target`; call it as soon as the child is reaped so a recycled pid is never signalled.
pub fn release_child(target: libc::pid_t) {
    TARGETS.iter().any(|slot| {
        slot.compare_exchange(target, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    });
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
            interrupt: Signal::new(),
        };
        WRITE_END
            .compare_exchange(-1, write_end, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| libc::EBUSY)?;
        pipe.installed = true;
        for signal in FORWARDED {
            install_handler(signal)?;
        }
        Ok(pipe)
    }

    /// The read end, readable while a signal byte waits, for a gate on a reactor.
    #[must_use]
    pub const fn read_fd(&self) -> c_int {
        self.read_end
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
                .for_each(|byte| self.forward(c_int::from(*byte)));
        }
    }

    fn forward(&self, signal: c_int) {
        if self.received.get().is_none() {
            self.received.set(Some(signal));
            self.interrupt.fire();
        }
        TARGETS
            .iter()
            .map(|target| target.load(Ordering::SeqCst))
            .filter(|target| *target != 0)
            .for_each(|target| {
                unsafe { libc::kill(target, signal) };
            });
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        if self.installed {
            for signal in FORWARDED {
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
