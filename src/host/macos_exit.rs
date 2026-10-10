//! macOS child-exit readiness through one coordinator-owned kqueue.

use core::ffi::c_int;
use core::mem::MaybeUninit;
use core::ptr::{null, null_mut};

/// One process-exit or coordinator-wake record written into caller storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacosExitEvent {
    /// Monotonic process token, or zero for the coordinator wake source.
    pub token: u64,
}

/// A nonblocking terminal status for one child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacosExitStatus {
    /// Normal exit code, or `None` when the process ended by signal.
    pub code: Option<i32>,
    /// Signal number when the process ended by signal.
    pub signal: Option<i32>,
}

/// One child identity registered with a [`MacosExitPort`].
#[derive(Debug)]
pub struct MacosProcessWatch {
    process_id: libc::pid_t,
    token: u64,
    reaped: bool,
    pending_status: Option<MacosExitStatus>,
}

/// Coordinator-owned kqueue for child exits and task wake notifications.
#[derive(Debug)]
pub struct MacosExitPort {
    descriptor: c_int,
    next_token: u64,
}

impl MacosExitPort {
    /// Creates a kqueue and registers one user event for remote coordinator wakes.
    pub fn new() -> Result<Self, c_int> {
        let descriptor = unsafe { libc::kqueue() };
        if descriptor < 0 {
            return Err(last_error());
        }
        let port = Self {
            descriptor,
            next_token: 1,
        };
        let mut change = event(0, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0);
        if unsafe { libc::kevent(port.descriptor, &mut change, 1, null_mut(), 0, null()) } < 0 {
            return Err(last_error());
        }
        Ok(port)
    }

    /// Registers a child exit without owning or blocking on the process handle.
    pub fn watch(&mut self, process_id: libc::pid_t) -> Result<MacosProcessWatch, c_int> {
        if process_id <= 0 {
            return Err(libc::EINVAL);
        }
        let token = self.allocate_token()?;
        let mut change = event(
            process_id as usize,
            libc::EVFILT_PROC,
            libc::EV_ADD | libc::EV_ONESHOT,
            libc::NOTE_EXIT,
        );
        change.udata = token as usize as *mut _;
        if unsafe { libc::kevent(self.descriptor, &mut change, 1, null_mut(), 0, null()) } < 0 {
            let code = last_error();
            if code != libc::ESRCH {
                return Err(code);
            }
            let Some(status) = try_reap(process_id)? else {
                return Err(code);
            };
            return Ok(MacosProcessWatch {
                process_id,
                token,
                reaped: false,
                pending_status: Some(status),
            });
        }
        let pending_status = try_reap(process_id)?;
        if pending_status.is_some() {
            let mut remove = event(process_id as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0);
            unsafe {
                libc::kevent(self.descriptor, &mut remove, 1, null_mut(), 0, null());
            }
        }
        Ok(MacosProcessWatch {
            process_id,
            token,
            reaped: false,
            pending_status,
        })
    }

    fn allocate_token(&mut self) -> Result<u64, c_int> {
        let token = self.next_token;
        self.next_token = token.checked_add(1).ok_or(libc::EOVERFLOW)?;
        Ok(token)
    }

    /// Parks the coordinator for a process exit or remote wake event.
    pub fn wait(&self, events: &mut [MacosExitEvent]) -> Result<usize, c_int> {
        self.collect(events, null())
    }

    /// Drains currently ready kqueue records without waiting.
    pub fn poll(&self, events: &mut [MacosExitEvent]) -> Result<usize, c_int> {
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        self.collect(events, &timeout)
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
        let limit = events.len().min(kernel_events.len()) as c_int;
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
        if ready < 0 {
            return Err(last_error());
        }
        for (destination, source) in events
            .iter_mut()
            .zip(kernel_events.iter())
            .take(ready as usize)
        {
            let source = unsafe { source.assume_init_ref() };
            *destination = MacosExitEvent {
                token: source.udata as usize as u64,
            };
        }
        Ok(ready as usize)
    }

    /// Publishes a coalescing user event to release the coordinator park.
    pub fn wake(&self) -> Result<(), c_int> {
        let mut change = event(0, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
        if unsafe { libc::kevent(self.descriptor, &mut change, 1, null_mut(), 0, null()) } < 0 {
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
    /// Caller-owned event identity registered for this child.
    #[must_use]
    pub const fn token(&self) -> u64 {
        self.token
    }

    /// Returns a terminal process record only after exit, without blocking.
    pub fn reap(&mut self) -> Result<Option<MacosExitStatus>, c_int> {
        if self.reaped {
            return Ok(None);
        }
        if let Some(status) = self.pending_status.take() {
            self.reaped = true;
            return Ok(Some(status));
        }
        let status = try_reap(self.process_id)?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }
}

fn try_reap(process_id: libc::pid_t) -> Result<Option<MacosExitStatus>, c_int> {
    let mut raw_status = 0;
    let result = unsafe { libc::waitpid(process_id, &mut raw_status, libc::WNOHANG) };
    if result == 0 {
        return Ok(None);
    }
    if result < 0 {
        let code = last_error();
        return Err(code);
    }
    if libc::WIFEXITED(raw_status) {
        return Ok(Some(MacosExitStatus {
            code: Some(libc::WEXITSTATUS(raw_status)),
            signal: None,
        }));
    }
    if libc::WIFSIGNALED(raw_status) {
        return Ok(Some(MacosExitStatus {
            code: None,
            signal: Some(libc::WTERMSIG(raw_status)),
        }));
    }
    Err(libc::EIO)
}

fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: null_mut(),
    }
}

fn last_error() -> c_int {
    unsafe { *libc::__error() }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;

    use super::{MacosExitEvent, MacosExitPort};

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
                    let count = libc::read(
                        release[0],
                        (&mut value as *mut u8).cast(),
                        size_of_val(&value),
                    );
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
            let value = [b'x'];
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
        let mut port = MacosExitPort::new().expect("kqueue port is created");
        let mut watch = port
            .watch(child.process_id)
            .expect("child exit is registered");
        let mut events = [MacosExitEvent { token: u64::MAX }; 4];
        let before_release = port
            .poll(&mut events)
            .expect("held child has no exit event");
        let premature_reap = watch.reap().expect("held child remains live");
        child.release();
        let exit_count = port.wait(&mut events).expect("kqueue waits for child exit");
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
                    process_id as libc::id_t,
                    &mut exit_record,
                    libc::WEXITED | libc::WNOWAIT,
                )
            },
            0,
            "waitid observes exit without reaping before registration"
        );
        let mut port = MacosExitPort::new().expect("race control kqueue is created");
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
}
