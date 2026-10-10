//! Owned, nonblocking Unix pipe ends for one child process.

use core::ffi::c_int;

/// The four descriptors used to capture one child's stdout and stderr.
#[derive(Debug)]
pub(crate) struct UnixPipePair {
    stdout: OwnedPipe,
    stderr: OwnedPipe,
}

/// One pipe's parent read end and child write end.
#[derive(Debug)]
pub(crate) struct OwnedPipe {
    parent_read: OwnedDescriptor,
    child_write: OwnedDescriptor,
}

/// A descriptor closed exactly once when its owner is dropped.
#[derive(Debug)]
pub struct OwnedDescriptor(c_int);

/// Errors while creating a nonblocking pipe with close-on-exec set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnixPipeError {
    /// The operating-system error returned by the failed descriptor operation.
    pub(crate) code: c_int,
}

impl UnixPipePair {
    /// Creates stdout and stderr pipes, closing stdout if stderr creation fails.
    pub(crate) fn new() -> Result<Self, UnixPipeError> {
        Self::with_factory(create_pipe)
    }

    fn with_factory(
        mut factory: impl FnMut() -> Result<OwnedPipe, UnixPipeError>,
    ) -> Result<Self, UnixPipeError> {
        let stdout = factory()?;
        let stderr = factory()?;
        Ok(Self { stdout, stderr })
    }

    /// Returns the parent-owned stdout pipe.
    #[must_use]
    pub(crate) fn stdout(&self) -> &OwnedPipe {
        &self.stdout
    }

    /// Returns the parent-owned stderr pipe.
    #[must_use]
    pub(crate) fn stderr(&self) -> &OwnedPipe {
        &self.stderr
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    pub(crate) fn into_parent_reads(self) -> (OwnedDescriptor, OwnedDescriptor) {
        (
            self.stdout.into_parent_read(),
            self.stderr.into_parent_read(),
        )
    }
}

impl OwnedPipe {
    /// Returns the nonblocking descriptor read by the parent.
    #[must_use]
    pub(crate) fn parent_read(&self) -> &OwnedDescriptor {
        &self.parent_read
    }

    /// Returns the descriptor installed as the child's output handle.
    #[must_use]
    pub(crate) fn child_write(&self) -> &OwnedDescriptor {
        &self.child_write
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    fn into_parent_read(self) -> OwnedDescriptor {
        self.parent_read
    }
}

impl OwnedDescriptor {
    /// Returns the raw descriptor for poll, read, or spawn file actions.
    #[must_use]
    pub fn raw(&self) -> c_int {
        self.0
    }
}

impl Drop for OwnedDescriptor {
    fn drop(&mut self) {
        // close is the sole ownership release for this descriptor.
        unsafe {
            libc::close(self.0);
        }
    }
}

fn create_pipe() -> Result<OwnedPipe, UnixPipeError> {
    let mut descriptors = [-1; 2];
    // pipe2 applies both flags atomically where available; macOS uses pipe+fcntl.
    let result = unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK)
        }
        #[cfg(not(target_os = "linux"))]
        {
            libc::pipe(descriptors.as_mut_ptr())
        }
    };
    if result != 0 {
        return Err(last_error());
    }
    let read_end = OwnedDescriptor(descriptors[0]);
    let write_end = OwnedDescriptor(descriptors[1]);
    #[cfg(target_os = "linux")]
    if unsafe {
        let flags = libc::fcntl(write_end.0, libc::F_GETFL);
        flags < 0 || libc::fcntl(write_end.0, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0
    } {
        return Err(last_error());
    }
    #[cfg(not(target_os = "linux"))]
    // darwin has no pipe2; one host owner runs this block without yielding or spawning.
    if let Err(error) = set_descriptor_flags(read_end.0, true)
        .and_then(|()| set_descriptor_flags(write_end.0, false))
    {
        return Err(error);
    }
    let read_end = move_above_stdio(read_end)?;
    let write_end = move_above_stdio(write_end)?;
    Ok(OwnedPipe {
        parent_read: read_end,
        child_write: write_end,
    })
}

fn move_above_stdio(descriptor: OwnedDescriptor) -> Result<OwnedDescriptor, UnixPipeError> {
    if descriptor.0 > libc::STDERR_FILENO {
        return Ok(descriptor);
    }
    let duplicated =
        unsafe { libc::fcntl(descriptor.0, libc::F_DUPFD_CLOEXEC, libc::STDERR_FILENO + 1) };
    if duplicated < 0 {
        return Err(last_error());
    }
    let moved = OwnedDescriptor(duplicated);
    drop(descriptor);
    Ok(moved)
}

#[cfg(not(target_os = "linux"))]
fn set_descriptor_flags(descriptor: c_int, nonblocking: bool) -> Result<(), UnixPipeError> {
    let descriptor_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    let status_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if descriptor_flags < 0 || status_flags < 0 {
        return Err(last_error());
    }
    if unsafe {
        libc::fcntl(
            descriptor,
            libc::F_SETFD,
            descriptor_flags | libc::FD_CLOEXEC,
        )
    } < 0
        || (nonblocking
            && unsafe { libc::fcntl(descriptor, libc::F_SETFL, status_flags | libc::O_NONBLOCK) }
                < 0)
    {
        return Err(last_error());
    }
    Ok(())
}

fn last_error() -> UnixPipeError {
    // The caller can distinguish failure; the syscall errno is captured below per OS.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    let code = unsafe { *libc::__error() };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let code = unsafe { *libc::__errno_location() };
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_os = "linux",
        target_os = "android"
    )))]
    let code = libc::EIO;
    UnixPipeError { code }
}

#[cfg(test)]
mod tests {
    use super::{OwnedPipe, UnixPipeError, UnixPipePair};

    #[test]
    fn c38a_creates_two_owned_nonblocking_pipe_pairs() {
        let pair = UnixPipePair::new().expect("both capture pipes should be created");
        let descriptors = [
            pair.stdout().parent_read().raw(),
            pair.stdout().child_write().raw(),
            pair.stderr().parent_read().raw(),
            pair.stderr().child_write().raw(),
        ];
        for first in 0..descriptors.len() {
            for second in first + 1..descriptors.len() {
                assert_ne!(descriptors[first], descriptors[second]);
            }
        }
        for descriptor in descriptors {
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD must succeed for an owned descriptor");
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        assert_status_flag(descriptors[0], libc::O_NONBLOCK, true);
        assert_status_flag(descriptors[1], libc::O_NONBLOCK, false);
        assert_status_flag(descriptors[2], libc::O_NONBLOCK, true);
        assert_status_flag(descriptors[3], libc::O_NONBLOCK, false);
        drop(pair);
        for descriptor in descriptors {
            assert_closed(descriptor);
        }
        std::println!(
            "{{\"parent_reads\":2,\"child_writes\":2,\"ownership_checks\":4,\"closed_after_drop\":4}}"
        );
    }

    #[test]
    fn c38a_second_pipe_failure_drops_first_pair() {
        use super::OwnedDescriptor;

        let mut calls = 0;
        let mut first_descriptors = [-1; 2];
        let result = UnixPipePair::with_factory(|| {
            calls += 1;
            if calls == 2 {
                return Err(UnixPipeError { code: libc::EMFILE });
            }
            let duplicated = unsafe { libc::dup(0) };
            let second = unsafe { libc::dup(0) };
            assert!(duplicated >= 0 && second >= 0);
            first_descriptors = [duplicated, second];
            Ok(OwnedPipe {
                parent_read: OwnedDescriptor(duplicated),
                child_write: OwnedDescriptor(second),
            })
        });
        assert!(result.is_err());
        assert_eq!(first_descriptors.len(), 2);
        for descriptor in first_descriptors {
            assert_closed(descriptor);
        }
        std::println!("{{\"second_pair_failure\":1,\"first_pair_descriptors_closed\":2}}");
    }

    fn assert_status_flag(descriptor: i32, flag: i32, expected: bool) {
        let status = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        assert!(status >= 0, "F_GETFL must succeed for an owned descriptor");
        assert_eq!(status & flag != 0, expected);
    }

    fn assert_closed(descriptor: i32) {
        assert!(descriptor >= 0, "fixture must own a valid descriptor");
        assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
        assert_eq!(super::last_error().code, libc::EBADF);
    }
}
