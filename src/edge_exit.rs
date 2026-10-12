pub(crate) mod fixture {
    use core::num::NonZeroI32;
    use std::os::fd::RawFd;

    use prime::os::child;

    const RELEASE_BYTE: u8 = b'R';
    const PASSED_BYTE: u8 = b'P';

    pub struct ForkedChild {
        pub pid: NonZeroI32,
        release_write: RawFd,
    }

    pub fn fork_child(exit_code: i32, wait_for_release: bool) -> ForkedChild {
        let mut release_pipe = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe(release_pipe.as_mut_ptr()) },
            0,
            "pipe for release"
        );
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork child fixture");
        if pid == 0 {
            unsafe {
                libc::close(release_pipe[1]);
                let mut byte = 0_u8;
                if wait_for_release {
                    libc::read(release_pipe[0], (&raw mut byte).cast(), 1);
                }
                libc::_exit(exit_code);
            }
        }
        assert_eq!(
            unsafe { libc::close(release_pipe[0]) },
            0,
            "close release read end"
        );
        ForkedChild {
            pid: NonZeroI32::new(pid).expect("fork returned a positive pid"),
            release_write: release_pipe[1],
        }
    }

    impl ForkedChild {
        pub fn release(&self) {
            let byte = RELEASE_BYTE;
            let written = unsafe { libc::write(self.release_write, (&raw const byte).cast(), 1) };
            assert_eq!(written, 1, "write release byte");
        }
    }

    impl Drop for ForkedChild {
        fn drop(&mut self) {
            unsafe { libc::close(self.release_write) };
        }
    }

    pub fn wait_exit_without_reaping(pid: NonZeroI32) {
        let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
        let observed = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.get().cast_unsigned(),
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(
            observed, 0,
            "waitid sees the child exit and leaves the zombie"
        );
    }

    pub fn zombie_is_released(pid: NonZeroI32) -> bool {
        child::try_reap(pid).is_err_and(|error| error.raw_os_error() == Some(libc::ECHILD))
    }

    pub fn in_single_threaded_process(body: impl FnOnce()) {
        let mut verdict_pipe = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe(verdict_pipe.as_mut_ptr()) },
            0,
            "pipe for verdict"
        );
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork isolated process");
        if pid == 0 {
            let passed = std::panic::catch_unwind(core::panic::AssertUnwindSafe(body)).is_ok();
            let verdict = if passed { PASSED_BYTE } else { b'F' };
            unsafe {
                libc::write(verdict_pipe[1], (&raw const verdict).cast(), 1);
                libc::_exit(0);
            }
        }
        unsafe { libc::close(verdict_pipe[1]) };
        let mut verdict = 0_u8;
        let count = unsafe { libc::read(verdict_pipe[0], (&raw mut verdict).cast(), 1) };
        unsafe { libc::close(verdict_pipe[0]) };
        let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
        let reaped = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.cast_unsigned(),
                &raw mut info,
                libc::WEXITED,
            )
        };
        assert_eq!(reaped, 0, "reap isolated process");
        assert!(
            count == 1 && verdict == PASSED_BYTE,
            "isolated process body failed, read {count} bytes"
        );
    }
}
