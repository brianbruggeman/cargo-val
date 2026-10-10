//! Linux child-exit readiness over pidfds and one coordinator-owned epoll port.

use core::ffi::c_int;

/// One event returned by the coordinator's Linux event port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    next_token: u64,
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
        if unsafe { libc::epoll_ctl(epoll.0, libc::EPOLL_CTL_ADD, wake.0, &mut event) } < 0 {
            return Err(last_error());
        }
        Ok(Self {
            epoll,
            wake,
            next_token: 1,
        })
    }

    /// Registers a child process without taking ownership of its process handle.
    ///
    /// The returned monotonic token identifies its event for the port's lifetime.
    /// The coordinator stores it in its existing fixed child slot; this port adds
    /// no second child table or admission gate.
    pub fn watch(&mut self, process_id: libc::pid_t) -> Result<LinuxPidWatch, c_int> {
        if process_id <= 0 {
            return Err(libc::EINVAL);
        }
        let token = self.next_token;
        self.next_token = token.checked_add(1).ok_or(libc::EOVERFLOW)?;
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, process_id, 0) as c_int };
        if descriptor < 0 {
            return Err(last_error());
        }
        let descriptor = LinuxDescriptor(descriptor);
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLONESHOT) as u32,
            u64: token,
        };
        if unsafe { libc::epoll_ctl(self.epoll.0, libc::EPOLL_CTL_ADD, descriptor.0, &mut event) }
            < 0
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

    /// Parks the coordinator until a watched child exits or a remote wake arrives.
    ///
    /// `epoll_wait` is the single reactor wait; child wait/read/write operations are
    /// never performed here. Every returned event is written into caller storage.
    pub fn wait(&self, events: &mut [LinuxExitEvent]) -> Result<usize, c_int> {
        self.collect(events, -1)
    }

    /// Drains currently ready records once without waiting or scheduling a worker.
    pub fn poll(&self, events: &mut [LinuxExitEvent]) -> Result<usize, c_int> {
        self.collect(events, 0)
    }

    fn collect(&self, events: &mut [LinuxExitEvent], timeout: c_int) -> Result<usize, c_int> {
        if events.is_empty() || events.len() > i32::MAX as usize {
            return Err(libc::EINVAL);
        }
        let mut kernel_events: [core::mem::MaybeUninit<libc::epoll_event>; 64] =
            [const { core::mem::MaybeUninit::uninit() }; 64];
        let limit = events.len().min(kernel_events.len()) as c_int;
        let ready = unsafe {
            libc::epoll_wait(
                self.epoll.0,
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
            *destination = LinuxExitEvent { token: source.u64 };
        }
        Ok(ready as usize)
    }

    /// Signals the coordinator; a full eventfd already represents a pending wake.
    pub fn wake(&self) -> Result<(), c_int> {
        let value = 1_u64;
        loop {
            let written = unsafe {
                libc::write(
                    self.wake.0,
                    (&value as *const u64).cast(),
                    core::mem::size_of::<u64>(),
                )
            };
            if written == core::mem::size_of::<u64>() as isize
                || (written < 0 && last_error() == libc::EAGAIN)
            {
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
    pub fn drain_wake(&self) -> Result<u64, c_int> {
        loop {
            let mut value = 0_u64;
            let read = unsafe {
                libc::read(
                    self.wake.0,
                    (&mut value as *mut u64).cast(),
                    core::mem::size_of::<u64>(),
                )
            };
            if read == core::mem::size_of::<u64>() as isize {
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
    pub fn reap(&mut self) -> Result<Option<LinuxExitStatus>, c_int> {
        if self.reaped {
            return Ok(None);
        }
        let mut raw_status = 0;
        let result = unsafe { libc::waitpid(self.process_id, &mut raw_status, libc::WNOHANG) };
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

fn last_error() -> c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use std::format;

    use super::{LinuxExitEvent, LinuxExitPort};

    #[test]
    fn c39_wakes_for_exit_and_control_without_semaphore_mode() {
        let mut port = LinuxExitPort::new().expect("epoll port and wake descriptor are created");
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
                let read = libc::read(barrier[0], (&mut release as *mut u8).cast(), 1);
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

    impl HeldChild {
        fn release(&self) -> Result<(), libc::c_int> {
            let byte = b'x';
            let written = unsafe {
                libc::write(
                    self.release.0,
                    (&byte as *const u8).cast(),
                    core::mem::size_of_val(&byte),
                )
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
                    libc::waitpid(self.process_id, core::ptr::null_mut(), 0);
                }
            }
        }
    }
}
