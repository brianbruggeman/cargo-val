//! Turns console control events into an event handle so the event loop can end every child it spawned.

use core::cell::Cell;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use prime::os::readiness::Readiness;
use proxima_core::signal::Signal;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use crate::build_defaults::SIGNAL_SLOTS;
use crate::edge_spawn_windows::{STATUS_CONTROL_C_EXIT, terminate_children};

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const CONVENTIONAL_BASE: i32 = 128;
const GROUP_CAPACITY: usize = SIGNAL_SLOTS;
const RESERVED: u32 = u32::MAX;

/// `ERROR_NO_SYSTEM_RESOURCES`, the code a full group table refuses with before any child is spawned.
pub const TABLE_FULL: i32 = 1450;
const ERROR_ALREADY_EXISTS: i32 = 183;

// the handler runs on a system thread and can only reach process-global state; 0 means no event is installed
static EVENT: AtomicUsize = AtomicUsize::new(0);

// the first control event the handler saw, as a signal number; 0 means none is waiting
static PENDING: AtomicI32 = AtomicI32::new(0);

// each nested group job by the process id of the child that leads it; a zero process id means the slot is free
static GROUP_PROCESSES: [AtomicU32; GROUP_CAPACITY] = [const { AtomicU32::new(0) }; GROUP_CAPACITY];
static GROUP_JOBS: [AtomicUsize; GROUP_CAPACITY] = [const { AtomicUsize::new(0) }; GROUP_CAPACITY];

/// A manual-reset event a prime gate watches and the console control handler sets.
#[derive(Debug)]
pub struct SignalPipe {
    event: OwnedHandle,
    received: Cell<Option<i32>>,
    interrupt: Signal,
}

/// Claims a table slot before the group leader is spawned, so a full table refuses the spawn instead of killing the child.
///
/// # Errors
///
/// Returns [`TABLE_FULL`] when every slot is taken; raise `CARGO_VAL_BUILD_SIGNAL_SLOTS` and rebuild.
pub(crate) fn reserve_group() -> Result<usize, i32> {
    GROUP_PROCESSES
        .iter()
        .position(|process| {
            process
                .compare_exchange(0, RESERVED, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
        .ok_or(TABLE_FULL)
}

/// Frees a slot claimed by [`reserve_group`] when the spawn that wanted it failed.
pub(crate) fn abandon_group(slot: usize) {
    GROUP_PROCESSES[slot].store(0, Ordering::SeqCst);
}

/// Remembers the nested job that holds the group a child leads, so a later interrupt can end the whole group.
///
/// The slot owns `job` until [`release_group`] closes it.
pub(crate) fn register_group(slot: usize, process_id: u32, job: HANDLE) {
    GROUP_JOBS[slot].store(job as usize, Ordering::SeqCst);
    GROUP_PROCESSES[slot].store(process_id, Ordering::SeqCst);
}

/// Group slots are claimed inside the spawn edge by [`reserve_group`], so nothing is reserved ahead of it.
///
/// # Errors
///
/// Never fails on this OS; the result matches the edges with a bounded target table.
pub fn reserve_slot() -> Result<(), i32> {
    Ok(())
}

/// Nothing was reserved ahead of the spawn, so there is nothing to hand back.
pub fn abandon_slot() {}

/// Children join the run job at creation, so there is nothing to register per child.
///
/// # Errors
///
/// Never fails on this OS; the result matches the edges with a bounded target table.
pub fn track_child(_target: u32) -> Result<(), i32> {
    Ok(())
}

/// The nested job is registered when the group leader is spawned.
///
/// # Errors
///
/// Never fails on this OS; the result matches the edges with a bounded target table.
pub fn track_group(_target: u32) -> Result<(), i32> {
    Ok(())
}

/// A reaped child needs no unregistering: the run job holds no process id that Windows could recycle.
pub fn release_child(_target: u32) {}

/// Closes the nested job of the group `target` led, which ends any straggler left in it.
pub fn release_group(target: u32) {
    if let Some(job) = take_group(target) {
        unsafe { CloseHandle(job) };
    }
}

/// Ends the whole group `target` leads through its job, so the end reaches every child whether or not a console Ctrl+C did.
pub fn interrupt_group(target: u32) {
    for (process, slot) in GROUP_PROCESSES.iter().zip(&GROUP_JOBS) {
        if process.load(Ordering::SeqCst) == target {
            let job = slot.load(Ordering::SeqCst) as HANDLE;
            unsafe { TerminateJobObject(job, STATUS_CONTROL_C_EXIT) };
        }
    }
}

fn take_group(target: u32) -> Option<HANDLE> {
    GROUP_PROCESSES
        .iter()
        .zip(&GROUP_JOBS)
        .find(|(process, _)| {
            process
                .compare_exchange(target, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
        .map(|(_, slot)| slot.swap(0, Ordering::SeqCst) as HANDLE)
}

/// The `128 + signal` status a shell reports for a process ended by that signal.
#[must_use]
pub const fn conventional_status(signal: i32) -> i32 {
    CONVENTIONAL_BASE + signal
}

impl SignalPipe {
    /// Installs the console control handler behind an event.
    ///
    /// # Errors
    ///
    /// Returns the Win32 code of the failing event or handler call, or `ERROR_ALREADY_EXISTS`
    /// when another `SignalPipe` is already installed in this process.
    pub fn install() -> Result<Self, i32> {
        let raw = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
        if raw.is_null() {
            return Err(last_error());
        }
        // safety: CreateEventW returned a new handle that nothing else owns.
        let event = unsafe { OwnedHandle::from_raw_handle(raw) };
        EVENT
            .compare_exchange(0, raw as usize, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| ERROR_ALREADY_EXISTS)?;
        PENDING.store(0, Ordering::SeqCst);
        if unsafe { SetConsoleCtrlHandler(Some(record_control_event), 1) } == 0 {
            EVENT.store(0, Ordering::SeqCst);
            return Err(last_error());
        }
        Ok(Self {
            event,
            received: Cell::new(None),
            interrupt: Signal::new(),
        })
    }

    /// A prime gate that opens once a control event has set the event.
    ///
    /// # Errors
    ///
    /// Returns the `DuplicateHandle` error when the event cannot be duplicated for the gate.
    pub fn gate(&self) -> io::Result<Readiness> {
        Readiness::signaled(&self.event)
    }

    /// Fires once, when [`drain`](Self::drain) first forwards a signal.
    #[must_use]
    pub const fn interrupt(&self) -> &Signal {
        &self.interrupt
    }

    /// The first signal observed by [`drain`](Self::drain), if any.
    #[must_use]
    pub fn received(&self) -> Option<i32> {
        self.received.get()
    }

    /// Takes the pending control event and ends every child of the run.
    ///
    /// # Errors
    ///
    /// Returns the Win32 code of a failing event reset.
    pub fn drain(&self) -> Result<(), i32> {
        let signal = PENDING.swap(0, Ordering::SeqCst);
        if signal == 0 {
            return Ok(());
        }
        if unsafe { ResetEvent(self.event.as_raw_handle()) } == 0 {
            return Err(last_error());
        }
        if self.received.get().is_none() {
            self.received.set(Some(signal));
            self.interrupt.fire();
        }
        terminate_children();
        Ok(())
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        unsafe { SetConsoleCtrlHandler(Some(record_control_event), 0) };
        EVENT.store(0, Ordering::SeqCst);
    }
}

unsafe extern "system" fn record_control_event(control: u32) -> i32 {
    let signal = match control {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => SIGINT,
        _ => SIGTERM,
    };
    let _ = PENDING.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    let event = EVENT.load(Ordering::SeqCst);
    if event != 0 {
        unsafe { SetEvent(event as HANDLE) };
    }
    1
}

fn last_error() -> i32 {
    unsafe { GetLastError() }.cast_signed()
}

#[cfg(test)]
mod tests {
    use super::{GROUP_CAPACITY, TABLE_FULL, abandon_group, reserve_group};

    #[proxima::test]
    async fn a_full_group_table_refuses_the_reservation_and_an_abandoned_slot_admits_it() {
        let first = reserve_group().expect("a free slot admits the group");
        for _ in 1..GROUP_CAPACITY {
            reserve_group().expect("a free slot admits the group");
        }

        let refused = reserve_group();
        abandon_group(first);
        let admitted = reserve_group();

        assert_eq!(refused, Err(TABLE_FULL));
        assert_eq!(admitted, Ok(first));
    }
}
