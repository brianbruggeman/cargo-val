//! Linux child-exit readiness over pidfds and one coordinator-owned epoll port.

use core::cell::Cell;
use core::ffi::c_int;
use core::mem::{MaybeUninit, size_of};
use core::ptr::null_mut;

/// One event returned by the coordinator's Linux event port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinuxExitEvent {
    /// Caller-owned token supplied when the pidfd was registered; zero is the wake event.
    pub token: u64,
}

/// Owned pidfd registered with a [`LinuxExitPort`].
#[derive(Debug)]
pub struct LinuxPidWatch {
    process_id: libc::pid_t,
    descriptor: LinuxDescriptor,
    token: u64,
    reaped: bool,
}

/// Coordinator-owned epoll port for child exits and remote task wakes.
#[derive(Debug)]
pub struct LinuxExitPort {
    epoll: LinuxDescriptor,
    wake: LinuxDescriptor,
    next_token: Cell<u64>,
}

/// A nonblocking child exit record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxExitStatus {
    /// Normal process exit code, or `None` when the process ended by signal.
    pub code: Option<i32>,
    /// Signal number when the child ended by signal.
    pub signal: Option<i32>,
}

/// A Linux descriptor closed when its owner is dropped.
#[derive(Debug)]
struct LinuxDescriptor(c_int);

impl Drop for LinuxDescriptor {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

impl LinuxExitPort {
    /// Creates one epoll instance and one nonblocking coalescing wake descriptor.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn new() -> Result<Self, c_int> {
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            return Err(last_error());
        }
        let epoll = LinuxDescriptor(epoll);
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(last_error());
        }
        let wake = LinuxDescriptor(wake);
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: 0,
        };
        if unsafe { libc::epoll_ctl(epoll.0, libc::EPOLL_CTL_ADD, wake.0, &raw mut event) } < 0 {
            return Err(last_error());
        }
        Ok(Self {
            epoll,
            wake,
            next_token: Cell::new(1),
        })
    }

    /// Registers a child process without taking ownership of its process handle.
    ///
    /// The returned monotonic token identifies its event for the port's lifetime.
    /// The coordinator stores it in its existing fixed child slot; this port adds
    /// no second child table or admission gate.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn watch(&self, process_id: libc::pid_t) -> Result<LinuxPidWatch, c_int> {
        if process_id <= 0 {
            return Err(libc::EINVAL);
        }
        let token = self.allocate_token()?;
        let opened = unsafe { libc::syscall(libc::SYS_pidfd_open, process_id, 0) };
        if opened < 0 {
            return Err(last_error());
        }
        let descriptor = c_int::try_from(opened).map_err(|_| libc::EOVERFLOW)?;
        let descriptor = LinuxDescriptor(descriptor);
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLONESHOT) as u32,
            u64: token,
        };
        if unsafe {
            libc::epoll_ctl(
                self.epoll.0,
                libc::EPOLL_CTL_ADD,
                descriptor.0,
                &raw mut event,
            )
        } < 0
        {
            return Err(last_error());
        }
        Ok(LinuxPidWatch {
            process_id,
            descriptor,
            token,
            reaped: false,
        })
    }

    /// Registers a nonblocking descriptor with the same child-exit reactor.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn watch_descriptor(&self, descriptor: c_int) -> Result<u64, c_int> {
        if descriptor < 0 {
            return Err(libc::EBADF);
        }
        let token = self.allocate_token()?;
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32,
            u64: token,
        };
        if unsafe {
            libc::epoll_ctl(
                self.epoll.0,
                libc::EPOLL_CTL_ADD,
                descriptor,
                &raw mut event,
            )
        } < 0
        {
            return Err(last_error());
        }
        Ok(token)
    }

    /// Removes a descriptor after EOF so level-triggered hangup cannot spin.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn unwatch_descriptor(&self, descriptor: c_int) -> Result<(), c_int> {
        if unsafe { libc::epoll_ctl(self.epoll.0, libc::EPOLL_CTL_DEL, descriptor, null_mut()) } < 0
        {
            return Err(last_error());
        }
        Ok(())
    }

    fn allocate_token(&self) -> Result<u64, c_int> {
        let token = self.next_token.get();
        self.next_token
            .set(token.checked_add(1).ok_or(libc::EOVERFLOW)?);
        Ok(token)
    }

    /// Parks the coordinator until a watched child exits or a remote wake arrives.
    ///
    /// `epoll_wait` is the single reactor wait; child wait/read/write operations are
    /// never performed here. Every returned event is written into caller storage.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn wait(&self, events: &mut [LinuxExitEvent]) -> Result<usize, c_int> {
        self.collect(events, -1)
    }

    /// Drains currently ready records once without waiting or scheduling a worker.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn poll(&self, events: &mut [LinuxExitEvent]) -> Result<usize, c_int> {
        self.collect(events, 0)
    }

    fn collect(&self, events: &mut [LinuxExitEvent], timeout: c_int) -> Result<usize, c_int> {
        if events.is_empty() || events.len() > i32::MAX as usize {
            return Err(libc::EINVAL);
        }
        let mut kernel_events: [MaybeUninit<libc::epoll_event>; 64] =
            [const { MaybeUninit::uninit() }; 64];
        let limit =
            c_int::try_from(events.len().min(kernel_events.len())).map_err(|_| libc::EINVAL)?;
        let ready = unsafe {
            libc::epoll_wait(
                self.epoll.0,
                kernel_events.as_mut_ptr().cast(),
                limit,
                timeout,
            )
        };
        let ready = usize::try_from(ready).map_err(|_| last_error())?;
        for (destination, source) in events.iter_mut().zip(kernel_events.iter()).take(ready) {
            let source = unsafe { source.assume_init_ref() };
            *destination = LinuxExitEvent { token: source.u64 };
        }
        Ok(ready)
    }

    /// Signals the coordinator; a full eventfd already represents a pending wake.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn wake(&self) -> Result<(), c_int> {
        let value = 1_u64;
        loop {
            let written =
                unsafe { libc::write(self.wake.0, (&raw const value).cast(), size_of::<u64>()) };
            if is_word(written) || (written < 0 && last_error() == libc::EAGAIN) {
                return Ok(());
            }
            if written < 0 {
                let code = last_error();
                if code == libc::EINTR {
                    continue;
                }
                return Err(code);
            }
            return Err(libc::EIO);
        }
    }

    /// Drains the coalesced wake count after the zero-token event is received.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn drain_wake(&self) -> Result<u64, c_int> {
        loop {
            let mut value = 0_u64;
            let read =
                unsafe { libc::read(self.wake.0, (&raw mut value).cast(), size_of::<u64>()) };
            if is_word(read) {
                return Ok(value);
            }
            if read < 0 {
                let code = last_error();
                if code == libc::EINTR {
                    continue;
                }
                return Err(code);
            }
            return Err(libc::EIO);
        }
    }
}

impl LinuxPidWatch {
    /// Process id whose exit is represented by this pidfd.
    #[must_use]
    pub const fn process_id(&self) -> libc::pid_t {
        self.process_id
    }

    /// Caller-owned event identity registered with the event port.
    #[must_use]
    pub const fn token(&self) -> u64 {
        self.token
    }

    /// Returns a terminal status only after pidfd readiness, without blocking.
    ///
    /// # Errors
    ///
    /// Returns the `errno` value reported by the failing Linux system call.
    pub fn reap(&mut self) -> Result<Option<LinuxExitStatus>, c_int> {
        if self.reaped {
            return Ok(None);
        }
        let mut raw_status = 0;
        let result = unsafe { libc::waitpid(self.process_id, &raw mut raw_status, libc::WNOHANG) };
        if result == 0 {
            return Ok(None);
        }
        if result < 0 {
            let code = last_error();
            if code == libc::ECHILD {
                self.reaped = true;
            }
            return Err(code);
        }
        self.reaped = true;
        let code = libc::WIFEXITED(raw_status).then(|| libc::WEXITSTATUS(raw_status));
        let signal = libc::WIFSIGNALED(raw_status).then(|| libc::WTERMSIG(raw_status));
        Ok(Some(LinuxExitStatus { code, signal }))
    }

    /// Raw descriptor for the registered pidfd, useful for readiness diagnostics.
    #[must_use]
    pub const fn raw_descriptor(&self) -> c_int {
        self.descriptor.0
    }
}

fn is_word(transferred: isize) -> bool {
    usize::try_from(transferred).is_ok_and(|count| count == size_of::<u64>())
}

fn last_error() -> c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of_val;
    use core::ptr::null_mut;

    use std::format;

    use super::{LinuxExitEvent, LinuxExitPort};

    #[test]
    fn c39_wakes_for_exit_and_control_without_semaphore_mode() {
        let port = LinuxExitPort::new().expect("epoll port and wake descriptor are created");
        let semaphore_record =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", port.wake.0))
                .expect("Linux exposes eventfd mode in proc fdinfo");
        assert!(semaphore_record.lines().any(|line| {
            line.strip_prefix("eventfd-semaphore:")
                .is_some_and(|value| value.trim() == "0")
        }));

        let mut barrier = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(barrier.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "held child should fork");
        if child == 0 {
            unsafe {
                libc::close(barrier[1]);
                let mut release = 0_u8;
                let read = libc::read(barrier[0], (&raw mut release).cast(), 1);
                libc::_exit(i32::from(read != 1));
            }
        }
        unsafe {
            libc::close(barrier[0]);
        }
        let mut held_child = HeldChild {
            process_id: child,
            release: super::LinuxDescriptor(barrier[1]),
            reaped: false,
        };
        let mut watch = port
            .watch(child)
            .expect("held child is registered through its pidfd");
        let descriptor_flags = unsafe { libc::fcntl(watch.raw_descriptor(), libc::F_GETFD) };
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        let mut events = [LinuxExitEvent { token: u64::MAX }; 4];
        let exits_before_release = port
            .poll(&mut events)
            .expect("one-shot readiness poll succeeds");
        assert_eq!(exits_before_release, 0);
        let premature_reaps = usize::from(
            watch
                .reap()
                .expect("held child reap check is nonblocking")
                .is_some(),
        );
        assert_eq!(premature_reaps, 0);

        port.wake().expect("remote coordinator wake is written");
        let wake_count = port
            .poll(&mut events)
            .expect("control wake is returned through epoll");
        assert_eq!(wake_count, 1);
        assert_eq!(events[0].token, 0);
        assert_eq!(port.drain_wake(), Ok(1));

        assert_eq!(held_child.release(), Ok(()));
        let exit_count = port
            .wait(&mut events)
            .expect("coordinator parks until pidfd becomes ready");
        assert_eq!(exit_count, 1);
        assert_eq!(events[0].token, watch.token());
        let status = watch
            .reap()
            .expect("ready child is reaped without blocking");
        assert_eq!(status.map(|exit| exit.code), Some(Some(0)));
        let duplicate_reaps = usize::from(
            watch
                .reap()
                .expect("a reaped child is not queried by recycled pid")
                .is_some(),
        );
        assert_eq!(duplicate_reaps, 0);
        held_child.reaped = true;
        assert_eq!(port.poll(&mut events), Ok(0));
        std::println!(
            "{{\"exit_before_release\":{exits_before_release},\"premature_reaps\":{premature_reaps},\"pidfd_cloexec\":1,\"epoll_wakes\":{exit_count},\"exit_records\":1,\"reaps\":1,\"duplicate_reaps\":{duplicate_reaps},\"eventfd_control_wakes\":{wake_count},\"duplicate_exits\":0,\"eventfd_semaphore_mode\":0}}"
        );
    }

    struct HeldChild {
        process_id: libc::pid_t,
        release: super::LinuxDescriptor,
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
                    libc::waitpid(self.process_id, null_mut(), 0);
                }
            }
        }
    }

    impl HeldChild {
        fn new() -> Self {
            let mut barrier = [-1; 2];
            assert_eq!(
                unsafe { libc::pipe2(barrier.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let process_id = unsafe { libc::fork() };
            assert!(process_id >= 0, "held child forks");
            if process_id == 0 {
                unsafe {
                    libc::close(barrier[1]);
                    let mut release = 0_u8;
                    let read = libc::read(barrier[0], (&raw mut release).cast(), 1);
                    libc::_exit(i32::from(read != 1));
                }
            }
            unsafe { libc::close(barrier[0]) };
            Self {
                process_id,
                release: super::LinuxDescriptor(barrier[1]),
                reaped: false,
            }
        }

        fn release(&self) -> Result<(), libc::c_int> {
            let byte = b'x';
            let written = unsafe {
                libc::write(self.release.0, (&raw const byte).cast(), size_of_val(&byte))
            };
            if written == 1 {
                Ok(())
            } else {
                Err(super::last_error())
            }
        }
    }

    impl Drop for HeldChild {
        fn drop(&mut self) {
            if !self.reaped {
                unsafe {
                    libc::kill(self.process_id, libc::SIGKILL);
                    libc::waitpid(self.process_id, null_mut(), 0);
                }
            }
        }
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
        let port = LinuxExitPort::new().expect("race control epoll port is created");
        let mut watch = port
            .watch(process_id)
            .expect("already-exited pid is registered through its pidfd");
        let mut events = [LinuxExitEvent { token: u64::MAX }; 4];
        let ready = port
            .poll(&mut events)
            .expect("exited child pidfd is already readable");
        assert_eq!(ready, 1);
        assert_eq!(events[0].token, watch.token());
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
    fn c59_exit_event_wakes_for_signal_and_reap_reports_signal_and_releases_zombie() {
        let mut child = HeldChild::new();
        let port = LinuxExitPort::new().expect("epoll port is created");
        let mut watch = port
            .watch(child.process_id)
            .expect("child exit is registered");
        let mut events = [LinuxExitEvent { token: u64::MAX }; 4];
        unsafe { libc::kill(child.process_id, libc::SIGKILL) };
        let ready = port.wait(&mut events).expect("epoll waits for child exit");
        assert!(
            events
                .iter()
                .take(ready)
                .any(|event| event.token == watch.token()),
            "exit event for the watched child"
        );
        let status = watch.reap().expect("ready exit reaps").expect("status");
        child.reaped = true;
        assert_eq!(status.signal, Some(libc::SIGKILL));
        assert_eq!(status.code, None);
        assert_eq!(
            unsafe { libc::waitpid(child.process_id, null_mut(), libc::WNOHANG) },
            -1,
            "zombie is released"
        );
    }
}
