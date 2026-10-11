//! macOS child-exit readiness through one coordinator-owned kqueue.

use core::cell::Cell;
use core::ffi::c_int;
use core::mem::MaybeUninit;
use core::ptr::{null, null_mut};

/// One process-exit or coordinator-wake record written into caller storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MacosExitEvent {
    /// Monotonic process token, or zero for the coordinator wake source.
    pub token: u64,
    /// Raw wait status carried by a delivered `NOTE_EXIT`, such as `0x700` for exit code 7.
    pub exit_status: Option<i32>,
}

/// A nonblocking terminal status for one child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacosExitStatus {
    /// Normal exit code, or `None` when the process ended by signal.
    pub code: Option<i32>,
    /// Signal number when the process ended by signal.
    pub signal: Option<i32>,
}

/// Whether kqueue has delivered this child's exit, which makes a blocking reap bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchState {
    Live,
    Exited { raw_status: i32 },
    Reaped,
}

/// One child identity registered with a [`MacosExitPort`].
#[derive(Debug)]
pub struct MacosProcessWatch {
    process_id: libc::pid_t,
    token: u64,
    registered: bool,
    state: WatchState,
    pending_status: Option<MacosExitStatus>,
}

/// Coordinator-owned kqueue for child exits and task wake notifications.
#[derive(Debug)]
pub struct MacosExitPort {
    descriptor: c_int,
    next_token: Cell<u64>,
}

impl MacosExitPort {
    /// Creates a kqueue and registers one user event for remote coordinator wakes.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn new() -> Result<Self, c_int> {
        let descriptor = unsafe { libc::kqueue() };
        if descriptor < 0 {
            return Err(last_error());
        }
        let port = Self {
            descriptor,
            next_token: Cell::new(1),
        };
        let change = event(0, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0);
        if unsafe { libc::kevent(port.descriptor, &raw const change, 1, null_mut(), 0, null()) } < 0
        {
            return Err(last_error());
        }
        Ok(port)
    }

    /// Registers a child exit without owning or blocking on the process handle.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn watch(&self, process_id: libc::pid_t) -> Result<MacosProcessWatch, c_int> {
        if process_id <= 0 {
            return Err(libc::EINVAL);
        }
        let token = self.allocate_token()?;
        let mut change = event(
            identity(process_id)?,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_ONESHOT,
            libc::NOTE_EXIT | libc::NOTE_EXITSTATUS,
        );
        change.udata = token_pointer(token)?;
        if unsafe { libc::kevent(self.descriptor, &raw const change, 1, null_mut(), 0, null()) } < 0
        {
            let code = last_error();
            if code != libc::ESRCH {
                return Err(code);
            }
            // esrch for our own child means it is past kqueue's reach and mid-exit, so no
            // NOTE_EXIT will ever arrive; the reap is bounded and the only way to see it.
            let pending_status = decode_status(wait_blocking(process_id)?)?;
            return Ok(MacosProcessWatch {
                process_id,
                token,
                registered: false,
                state: WatchState::Live,
                pending_status: Some(pending_status),
            });
        }
        let pending_status = try_reap(process_id)?;
        if pending_status.is_some() {
            let remove = event(identity(process_id)?, libc::EVFILT_PROC, libc::EV_DELETE, 0);
            unsafe {
                libc::kevent(self.descriptor, &raw const remove, 1, null_mut(), 0, null());
            }
        }
        Ok(MacosProcessWatch {
            process_id,
            token,
            registered: pending_status.is_none(),
            state: WatchState::Live,
            pending_status,
        })
    }

    /// Registers a nonblocking pipe descriptor in the shared process reactor.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn watch_descriptor(&self, descriptor: c_int) -> Result<u64, c_int> {
        if descriptor < 0 {
            return Err(libc::EBADF);
        }
        let token = self.allocate_token()?;
        let mut change = event(identity(descriptor)?, libc::EVFILT_READ, libc::EV_ADD, 0);
        change.udata = token_pointer(token)?;
        if unsafe { libc::kevent(self.descriptor, &raw const change, 1, null_mut(), 0, null()) } < 0
        {
            return Err(last_error());
        }
        Ok(token)
    }

    /// Removes a read source at EOF so kqueue cannot repeatedly report hangup.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn unwatch_descriptor(&self, descriptor: c_int) -> Result<(), c_int> {
        let change = event(identity(descriptor)?, libc::EVFILT_READ, libc::EV_DELETE, 0);
        if unsafe { libc::kevent(self.descriptor, &raw const change, 1, null_mut(), 0, null()) } < 0
        {
            return Err(last_error());
        }
        Ok(())
    }

    /// Removes a child exit source after its status was collected elsewhere.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn unwatch_process(&self, process_id: libc::pid_t) -> Result<(), c_int> {
        let change = event(identity(process_id)?, libc::EVFILT_PROC, libc::EV_DELETE, 0);
        if unsafe { libc::kevent(self.descriptor, &raw const change, 1, null_mut(), 0, null()) } < 0
        {
            let code = last_error();
            if code != libc::ENOENT && code != libc::ESRCH {
                return Err(code);
            }
        }
        Ok(())
    }

    fn allocate_token(&self) -> Result<u64, c_int> {
        let token = self.next_token.get();
        self.next_token
            .set(token.checked_add(1).ok_or(libc::EOVERFLOW)?);
        Ok(token)
    }

    /// Parks the coordinator for a process exit or remote wake event.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn wait(&self, events: &mut [MacosExitEvent]) -> Result<usize, c_int> {
        self.collect(events, null())
    }

    /// Waits for readiness with a bounded child-exit observation interval.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn wait_for(
        &self,
        events: &mut [MacosExitEvent],
        milliseconds: u32,
    ) -> Result<usize, c_int> {
        let timeout = libc::timespec {
            tv_sec: (milliseconds / 1000).into(),
            tv_nsec: ((milliseconds % 1000) * 1_000_000).into(),
        };
        self.collect(events, &raw const timeout)
    }

    /// Drains currently ready kqueue records without waiting.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn poll(&self, events: &mut [MacosExitEvent]) -> Result<usize, c_int> {
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        self.collect(events, &raw const timeout)
    }

    fn collect(
        &self,
        events: &mut [MacosExitEvent],
        timeout: *const libc::timespec,
    ) -> Result<usize, c_int> {
        if events.is_empty() || events.len() > i32::MAX as usize {
            return Err(libc::EINVAL);
        }
        let mut kernel_events: [MaybeUninit<libc::kevent>; 64] =
            [const { MaybeUninit::uninit() }; 64];
        let limit =
            c_int::try_from(events.len().min(kernel_events.len())).map_err(|_| libc::EINVAL)?;
        let ready = unsafe {
            libc::kevent(
                self.descriptor,
                null(),
                0,
                kernel_events.as_mut_ptr().cast(),
                limit,
                timeout,
            )
        };
        let ready = usize::try_from(ready).map_err(|_| last_error())?;
        for (destination, source) in events.iter_mut().zip(kernel_events.iter()).take(ready) {
            let source = unsafe { source.assume_init_ref() };
            *destination = MacosExitEvent {
                token: source.udata as usize as u64,
                exit_status: exit_status(source),
            };
        }
        Ok(ready)
    }

    /// Publishes a coalescing user event to release the coordinator park.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn wake(&self) -> Result<(), c_int> {
        let change = event(0, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
        if unsafe { libc::kevent(self.descriptor, &raw const change, 1, null_mut(), 0, null()) } < 0
        {
            return Err(last_error());
        }
        Ok(())
    }
}

impl Drop for MacosExitPort {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.descriptor);
        }
    }
}

impl MacosProcessWatch {
    /// Whether kqueue still owns a process-exit notification for this child.
    #[must_use]
    pub const fn registered(&self) -> bool {
        self.registered
    }

    /// Caller-owned event identity registered for this child.
    #[must_use]
    pub const fn token(&self) -> u64 {
        self.token
    }

    /// Records a delivered `NOTE_EXIT` for this child so the next reap may block on `waitpid`.
    ///
    /// Events for other tokens and events without an exit status are ignored.
    pub fn observe(&mut self, event: &MacosExitEvent) {
        let (WatchState::Live, Some(raw_status)) = (self.state, event.exit_status) else {
            return;
        };
        if event.token == self.token {
            self.registered = false;
            self.state = WatchState::Exited { raw_status };
        }
    }

    /// Returns a terminal process record once the child has exited.
    ///
    /// Without an observed exit event this never blocks; after one, `waitpid` is bounded
    /// because the process has already exited.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing kqueue or wait system call.
    pub fn reap(&mut self) -> Result<Option<MacosExitStatus>, c_int> {
        let status = match self.state {
            WatchState::Reaped => return Ok(None),
            WatchState::Exited { raw_status } => {
                release_zombie(self.process_id)?;
                Some(decode_status(raw_status)?)
            }
            WatchState::Live => match self.pending_status.take() {
                Some(status) => Some(status),
                None => try_reap(self.process_id)?,
            },
        };
        if status.is_some() {
            self.state = WatchState::Reaped;
        }
        Ok(status)
    }
}

fn try_reap(process_id: libc::pid_t) -> Result<Option<MacosExitStatus>, c_int> {
    let mut raw_status = 0;
    let result = unsafe { libc::waitpid(process_id, &raw mut raw_status, libc::WNOHANG) };
    if result == 0 {
        return Ok(None);
    }
    if result < 0 {
        return Err(last_error());
    }
    decode_status(raw_status).map(Some)
}

fn release_zombie(process_id: libc::pid_t) -> Result<(), c_int> {
    wait_blocking(process_id).map(drop)
}

fn wait_blocking(process_id: libc::pid_t) -> Result<i32, c_int> {
    loop {
        let mut raw_status = 0;
        let result = unsafe { libc::waitpid(process_id, &raw mut raw_status, 0) };
        if result >= 0 {
            return Ok(raw_status);
        }
        let code = last_error();
        if code != libc::EINTR {
            return Err(code);
        }
    }
}

fn decode_status(raw_status: i32) -> Result<MacosExitStatus, c_int> {
    if libc::WIFEXITED(raw_status) {
        return Ok(MacosExitStatus {
            code: Some(libc::WEXITSTATUS(raw_status)),
            signal: None,
        });
    }
    if libc::WIFSIGNALED(raw_status) {
        return Ok(MacosExitStatus {
            code: None,
            signal: Some(libc::WTERMSIG(raw_status)),
        });
    }
    Err(libc::EIO)
}

fn exit_status(record: &libc::kevent) -> Option<i32> {
    let delivered = record.filter == libc::EVFILT_PROC && record.fflags & libc::NOTE_EXIT != 0;
    if !delivered {
        return None;
    }
    i32::try_from(record.data).ok()
}

fn event(ident: usize, filter: i16, actions: u16, notes: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags: actions,
        fflags: notes,
        data: 0,
        udata: null_mut(),
    }
}

fn identity(value: c_int) -> Result<usize, c_int> {
    usize::try_from(value).map_err(|_| libc::EINVAL)
}

fn token_pointer(token: u64) -> Result<*mut core::ffi::c_void, c_int> {
    let address = usize::try_from(token).map_err(|_| libc::EOVERFLOW)?;
    Ok(core::ptr::without_provenance_mut(address))
}

fn last_error() -> c_int {
    unsafe { *libc::__error() }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;

    use super::{MacosExitEvent, MacosExitPort, MacosProcessWatch, WatchState};

    struct HeldChild {
        process_id: libc::pid_t,
        release: [libc::c_int; 2],
        reaped: bool,
    }

    struct ExitedChild {
        process_id: libc::pid_t,
        reaped: bool,
    }

    impl Drop for ExitedChild {
        fn drop(&mut self) {
            if !self.reaped {
                unsafe {
                    libc::waitpid(self.process_id, core::ptr::null_mut(), 0);
                }
            }
        }
    }

    impl HeldChild {
        fn new() -> Self {
            let mut release = [-1; 2];
            assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
            let process_id = unsafe { libc::fork() };
            assert!(process_id >= 0, "held child forks");
            if process_id == 0 {
                unsafe {
                    libc::close(release[1]);
                    let mut value = 0_u8;
                    let count =
                        libc::read(release[0], (&raw mut value).cast(), size_of_val(&value));
                    libc::close(release[0]);
                    libc::_exit(i32::from(count != 1));
                }
            }
            unsafe { libc::close(release[0]) };
            Self {
                process_id,
                release,
                reaped: false,
            }
        }

        fn release(&mut self) {
            let value = *b"x";
            let count = unsafe { libc::write(self.release[1], value.as_ptr().cast(), value.len()) };
            unsafe { libc::close(self.release[1]) };
            self.release[1] = -1;
            assert_eq!(count, 1);
        }
    }

    impl Drop for HeldChild {
        fn drop(&mut self) {
            if self.release[1] >= 0 {
                unsafe { libc::close(self.release[1]) };
            }
            if !self.reaped {
                unsafe {
                    libc::kill(self.process_id, libc::SIGKILL);
                    libc::waitpid(self.process_id, core::ptr::null_mut(), 0);
                }
            }
        }
    }

    #[test]
    fn c41_wakes_for_child_exit_and_control_without_worker_threads() {
        let mut child = HeldChild::new();
        let port = MacosExitPort::new().expect("kqueue port is created");
        let mut watch = port
            .watch(child.process_id)
            .expect("child exit is registered");
        let mut events = [MacosExitEvent {
            token: u64::MAX,
            ..MacosExitEvent::default()
        }; 4];
        let before_release = port
            .poll(&mut events)
            .expect("held child has no exit event");
        let premature_reap = watch.reap().expect("held child remains live");
        child.release();
        let exit_count = port.wait(&mut events).expect("kqueue waits for child exit");
        for event in events.iter().take(exit_count) {
            watch.observe(event);
        }
        assert!(
            events
                .iter()
                .take(exit_count)
                .any(|event| event.token == watch.token())
        );
        let status = watch.reap().expect("exit event permits nonblocking reap");
        assert_eq!(status.map(|status| status.code), Some(Some(0)));
        assert_eq!(watch.reap().expect("reap is idempotent"), None);
        child.reaped = true;
        port.wake().expect("user event wakes the coordinator");
        let wake_count = port
            .poll(&mut events)
            .expect("wake is immediately available");
        assert!(events.iter().take(wake_count).any(|event| event.token == 0));
        assert_eq!(before_release, 0);
        assert_eq!(premature_reap, None);
        assert_eq!(exit_count, 1);
        std::println!(
            "{{\"before_release\":{before_release},\"premature_reaps\":0,\"exit_events\":{exit_count},\"reaps\":1,\"duplicate_reaps\":0,\"control_wakes\":{wake_count}}}"
        );
    }

    #[test]
    fn c41_registration_race_control_observes_already_exited_child() {
        let process_id = unsafe { libc::fork() };
        assert!(process_id >= 0, "race control child forks");
        if process_id == 0 {
            unsafe { libc::_exit(7) };
        }
        let mut child = ExitedChild {
            process_id,
            reaped: false,
        };
        let mut exit_record = unsafe { core::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    process_id.cast_unsigned(),
                    &raw mut exit_record,
                    libc::WEXITED | libc::WNOWAIT,
                )
            },
            0,
            "waitid observes exit without reaping before registration"
        );
        let port = MacosExitPort::new().expect("race control kqueue is created");
        let mut watch = port
            .watch(process_id)
            .expect("already-exited pid is registered");
        let status = watch
            .reap()
            .expect("race control reaps once readiness is observed");
        assert_eq!(status.map(|status| status.code), Some(Some(7)));
        child.reaped = true;
        assert_eq!(watch.reap().expect("race control record is unique"), None);
        std::println!(
            "{{\"exited_before_registration\":1,\"exit_records\":1,\"reaps\":1,\"duplicate_reaps\":0}}"
        );
    }

    #[test]
    fn c97_watch_never_returns_an_unregistered_unreaped_child() {
        let mut child = HeldChild::new();
        let port = MacosExitPort::new().expect("kqueue port is created");
        let watch = port
            .watch(child.process_id)
            .expect("live child is registered");
        assert!(watch.registered(), "a live child owns a kqueue exit filter");
        unsafe { libc::kill(child.process_id, libc::SIGKILL) };
        let mut exit_record = unsafe { core::mem::zeroed::<libc::siginfo_t>() };
        let observed = unsafe {
            libc::waitid(
                libc::P_PID,
                child.process_id.cast_unsigned(),
                &raw mut exit_record,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(observed, 0, "waitid observes the kill without reaping");
        let mut late = port
            .watch(child.process_id)
            .expect("an exited child still resolves to a watch");
        let first = late.reap().expect("late watch reaps");
        child.reaped = true;
        assert_eq!(first.and_then(|status| status.signal), Some(libc::SIGKILL));
        assert!(!late.registered());
    }

    #[test]
    fn c97_watch_of_a_pid_that_is_not_our_child_errors_instead_of_parking() {
        let process_id = unsafe { libc::fork() };
        assert!(process_id >= 0, "gone-pid child forks");
        if process_id == 0 {
            unsafe { libc::_exit(0) };
        }
        assert_eq!(
            unsafe { libc::waitpid(process_id, core::ptr::null_mut(), 0) },
            process_id
        );
        let port = MacosExitPort::new().expect("kqueue port is created");
        let refused = port.watch(process_id).expect_err("a reaped pid is not watchable");
        assert_eq!(refused, libc::ECHILD, "esrch resolves through waitpid, never an unreaped watch");
    }

    #[test]
    fn c59_exit_event_carries_signal_status_and_blocking_reap_releases_zombie() {
        let mut child = HeldChild::new();
        let port = MacosExitPort::new().expect("kqueue port is created");
        let mut watch = port
            .watch(child.process_id)
            .expect("child exit is registered");
        let mut events = [MacosExitEvent::default(); 4];
        unsafe { libc::kill(child.process_id, libc::SIGKILL) };
        let ready = port.wait(&mut events).expect("kqueue waits for child exit");
        let delivered = events
            .iter()
            .take(ready)
            .find(|event| event.token == watch.token())
            .expect("exit event for the watched child");
        assert_eq!(
            delivered.exit_status.map(|raw| libc::WTERMSIG(raw)),
            Some(libc::SIGKILL)
        );
        watch.observe(delivered);
        let status = watch.reap().expect("observed exit reaps").expect("status");
        child.reaped = true;
        assert_eq!(status.signal, Some(libc::SIGKILL));
        assert_eq!(
            unsafe { libc::waitpid(child.process_id, core::ptr::null_mut(), libc::WNOHANG) },
            -1,
            "zombie is released"
        );
    }

    #[test]
    fn c54_unregistered_exit_uses_bounded_wait_then_reap() {
        let mut child = HeldChild::new();
        let port = MacosExitPort::new().expect("kqueue port is created");
        let mut watch = MacosProcessWatch {
            process_id: child.process_id,
            token: u64::MAX,
            registered: false,
            state: WatchState::Live,
            pending_status: None,
        };
        let mut events = [MacosExitEvent {
            token: 0,
            ..MacosExitEvent::default()
        }; 4];
        assert_eq!(
            port.poll(&mut events)
                .expect("no exit filter is registered"),
            0
        );
        child.release();
        let ready = port
            .wait_for(&mut events, 100)
            .expect("bounded wait returns without an exit filter");
        let status = watch
            .reap()
            .expect("unregistered child is reaped without blocking");
        assert_eq!(ready, 0);
        assert_eq!(status.and_then(|status| status.code), Some(0));
        assert_eq!(watch.reap().expect("reap remains unique"), None);
        child.reaped = true;
        std::println!(
            "{{\"unregistered_watches\":1,\"bounded_waits\":1,\"ready_events\":{ready},\"reaps\":1,\"duplicate_reaps\":0}}"
        );
    }
}
