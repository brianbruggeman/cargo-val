//! Owned, nonblocking Unix pipe ends for one child process.

use core::ffi::c_int;
#[cfg(all(feature = "edge-os", unix))]
use std::os::fd::{FromRawFd, OwnedFd};

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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
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

    /// Hands the descriptor to a std owner, which closes it instead of this type.
    #[cfg(all(feature = "edge-os", unix))]
    #[must_use]
    pub fn into_fd(self) -> OwnedFd {
        let released = core::mem::ManuallyDrop::new(self);
        // safety: the descriptor is open and `released` never closes it, so exactly one owner remains.
        unsafe { OwnedFd::from_raw_fd(released.0) }
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
    set_descriptor_flags(read_end.0, true)
        .and_then(|()| set_descriptor_flags(write_end.0, false))?;
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
    use core::mem::MaybeUninit;

    use super::{OwnedPipe, UnixPipeError, UnixPipePair};

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct PipeIdentity {
        device: libc::dev_t,
        inode: libc::ino_t,
    }

    #[test]
    fn c38a_creates_two_owned_nonblocking_pipe_pairs() {
        let pair = UnixPipePair::new().expect("both capture pipes should be created");
        let descriptors = [
            pair.stdout().parent_read().raw(),
            pair.stdout().child_write().raw(),
            pair.stderr().parent_read().raw(),
            pair.stderr().child_write().raw(),
        ];
        let identities = descriptors.map(descriptor_identity);
        assert_ne!(
            identities[0], identities[2],
            "stdout and stderr must be distinct pipes"
        );
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
        for (descriptor, identity) in descriptors.into_iter().zip(identities) {
            assert_closed(descriptor, identity);
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
        let mut first_identities = [PipeIdentity::default(); 2];
        let result = UnixPipePair::with_factory(|| {
            calls += 1;
            if calls == 2 {
                return Err(UnixPipeError { code: libc::EMFILE });
            }
            let mut ends = [-1; 2];
            assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
            first_descriptors = ends;
            first_identities = ends.map(descriptor_identity);
            Ok(OwnedPipe {
                parent_read: OwnedDescriptor(ends[0]),
                child_write: OwnedDescriptor(ends[1]),
            })
        });
        assert!(result.is_err());
        assert_eq!(first_descriptors.len(), 2);
        for (descriptor, identity) in first_descriptors.into_iter().zip(first_identities) {
            assert_closed(descriptor, identity);
        }
        std::println!("{{\"second_pair_failure\":1,\"first_pair_descriptors_closed\":2}}");
    }

    fn assert_status_flag(descriptor: i32, flag: i32, expected: bool) {
        let status = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        assert!(status >= 0, "F_GETFL must succeed for an owned descriptor");
        assert_eq!(status & flag != 0, expected);
    }

    fn descriptor_identity(descriptor: i32) -> PipeIdentity {
        let mut metadata = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) };
        assert_eq!(result, 0, "fstat must identify an owned pipe description");
        let metadata = unsafe { metadata.assume_init() };
        PipeIdentity {
            device: metadata.st_dev,
            inode: metadata.st_ino,
        }
    }

    // parallel tests reuse closed fd numbers, so only the pipe's fstat identity proves ownership.
    fn assert_closed(descriptor: i32, identity: PipeIdentity) {
        assert!(descriptor >= 0, "fixture must own a valid descriptor");
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } == -1 {
            assert_eq!(super::last_error().code, libc::EBADF);
        } else {
            assert_ne!(
                descriptor_identity(descriptor),
                identity,
                "identified pipe description is still open"
            );
        }
    }
}
