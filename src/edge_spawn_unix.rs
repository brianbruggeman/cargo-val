//! Unix edge for spawning a child with captured stdout and stderr.

#[cfg(all(unix, feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod pipe_pair {
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
                && unsafe {
                    libc::fcntl(descriptor, libc::F_SETFL, status_flags | libc::O_NONBLOCK)
                } < 0)
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
        use super::{OwnedDescriptor, OwnedPipe, UnixPipeError, UnixPipePair};

        const EOF_WAIT_MILLISECONDS: libc::c_int = 5_000;

        #[test]
        fn c38a_creates_two_owned_nonblocking_pipe_pairs() {
            let first = UnixPipePair::new().expect("both capture pipes should be created");
            let descriptors = [
                first.stdout().parent_read().raw(),
                first.stdout().child_write().raw(),
                first.stderr().parent_read().raw(),
                first.stderr().child_write().raw(),
            ];
            for descriptor in descriptors {
                let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
                assert!(flags >= 0, "F_GETFD must succeed for an owned descriptor");
                assert_ne!(flags & libc::FD_CLOEXEC, 0);
            }
            assert_status_flag(descriptors[0], libc::O_NONBLOCK, true);
            assert_status_flag(descriptors[1], libc::O_NONBLOCK, false);
            assert_status_flag(descriptors[2], libc::O_NONBLOCK, true);
            assert_status_flag(descriptors[3], libc::O_NONBLOCK, false);
            let stdout_eof_probe = probe_of(first.stdout().parent_read().raw());
            let stderr_broken_probe = probe_of(first.stderr().child_write().raw());
            drop(first);
            assert_eof(stdout_eof_probe);
            assert_broken_pipe(stderr_broken_probe);

            let second = UnixPipePair::new().expect("second capture pipes should be created");
            let stdout_broken_probe = probe_of(second.stdout().child_write().raw());
            let stderr_eof_probe = probe_of(second.stderr().parent_read().raw());
            drop(second);
            assert_broken_pipe(stdout_broken_probe);
            assert_eof(stderr_eof_probe);
            std::println!(
                "{{\"parent_reads\":4,\"child_writes\":4,\"ownership_checks\":4,\"closed_after_drop\":4}}"
            );
        }

        #[test]
        fn c38a_second_pipe_failure_drops_first_pair() {
            let mut calls = 0;
            let mut first_eof_probe = -1;
            let result = UnixPipePair::with_factory(|| {
                calls += 1;
                if calls == 2 {
                    return Err(UnixPipeError { code: libc::EMFILE });
                }
                let mut ends = [-1; 2];
                assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
                first_eof_probe = probe_of(ends[0]);
                Ok(OwnedPipe {
                    parent_read: OwnedDescriptor(ends[0]),
                    child_write: OwnedDescriptor(ends[1]),
                })
            });
            assert!(result.is_err());
            assert_eof(first_eof_probe);
            std::println!("{{\"second_pair_failure\":1,\"first_pair_write_end_closed\":1}}");
        }

        fn assert_status_flag(descriptor: i32, flag: i32, expected: bool) {
            let status = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
            assert!(status >= 0, "F_GETFL must succeed for an owned descriptor");
            assert_eq!(status & flag != 0, expected);
        }

        fn probe_of(descriptor: i32) -> i32 {
            let probe = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
            assert!(probe >= 0, "F_DUPFD_CLOEXEC must duplicate an owned descriptor");
            probe
        }

        fn assert_eof(probe: i32) {
            assert!(probe >= 0, "fixture must hold a probe descriptor");
            let mut waiting = libc::pollfd {
                fd: probe,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&raw mut waiting, 1, EOF_WAIT_MILLISECONDS) };
            assert_eq!(ready, 1, "a pipe with no write ends must report hangup");
            let mut byte = [0_u8; 1];
            let read = unsafe { libc::read(probe, byte.as_mut_ptr().cast(), 1) };
            assert_eq!(read, 0, "a read with no write ends must return EOF");
            assert_eq!(unsafe { libc::close(probe) }, 0);
        }

        fn assert_broken_pipe(probe: i32) {
            assert!(probe >= 0, "fixture must hold a probe descriptor");
            let byte = [0_u8; 1];
            let written = unsafe { libc::write(probe, byte.as_ptr().cast(), 1) };
            assert_eq!(written, -1, "a write with no read ends must fail");
            assert_eq!(super::last_error().code, libc::EPIPE);
            assert_eq!(unsafe { libc::close(probe) }, 0);
        }
    }
}

mod spawn {
    //! Unix child creation using lowered launch data and owned capture descriptors.

    use core::ffi::{c_char, c_int};
    use core::num::NonZeroI32;
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd};

    /// The signal cargo-val sends a child it ends itself; a child killed by it reports this signal, not an ordinary exit.
    pub const STOP_SIGNAL: c_int = libc::SIGKILL;

    use super::pipe_pair::{OwnedDescriptor, UnixPipeError, UnixPipePair};
    use crate::ExitSource;
    pub use crate::edge_args_unix::{ArgumentsError, LaunchStorage};
    use crate::edge_args_unix::{Arguments, Launch, Lowered};

    pub type HeldLaunchStorage = std::boxed::Box<LaunchStorage>;

    #[inline(never)]
    #[must_use]
    pub fn held_launch_storage() -> HeldLaunchStorage {
        std::boxed::Box::new(LaunchStorage::default())
    }

    const DOC_NICE: c_int = 10;

    /// A child process whose captured output remains owned by the host driver.
    #[derive(Debug)]
    pub struct Child {
        process_id: libc::pid_t,
        stdout: OwnedDescriptor,
        stderr: OwnedDescriptor,
    }

    /// A spawn failure: the raw `errno`-style code of the operation that refused.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SpawnError {
        /// The error code returned directly by the POSIX operation, or `EINVAL` for launch data it cannot hold.
        pub os_code: i32,
    }

    impl From<ArgumentsError> for SpawnError {
        fn from(_: ArgumentsError) -> Self {
            Self { os_code: libc::EINVAL }
        }
    }

    /// Why an exact test launch was refused: its arguments did not fit the fixed storage, or the OS refused the spawn.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ExactError {
        /// The launch data holds a NUL, a malformed environment entry, or more than the storage holds.
        Arguments(ArgumentsError),
        /// A pipe, file-action, or `posix_spawn` operation returned this code.
        Spawn(SpawnError),
    }

    impl Child {
        /// Returns the process identifier watched by the host event source.
        #[must_use]
        pub fn process_id(&self) -> u32 {
            self.process_id.cast_unsigned()
        }

        /// Returns the nonblocking stdout descriptor owned by the parent.
        #[must_use]
        pub fn stdout(&self) -> &OwnedDescriptor {
            &self.stdout
        }

        /// Returns the nonblocking stderr descriptor owned by the parent.
        #[must_use]
        pub fn stderr(&self) -> &OwnedDescriptor {
            &self.stderr
        }

        /// Transfers the two capture descriptors to poll-driven host readers.
        #[must_use]
        pub fn into_parts(self) -> (u32, OwnedFd, OwnedFd) {
            (self.process_id.cast_unsigned(), self.stdout.into_fd(), self.stderr.into_fd())
        }

        /// Duplicates both capture descriptors for a reader while this child keeps the originals.
        ///
        /// # Errors
        ///
        /// Returns the `fcntl` error when a descriptor cannot be duplicated.
        pub fn duplicate_streams(&self) -> io::Result<(OwnedFd, OwnedFd)> {
            Ok((duplicate(self.stdout.raw())?, duplicate(self.stderr.raw())?))
        }

        /// The source that reports this child's exit.
        ///
        /// # Errors
        ///
        /// Returns an error when the child has no usable process id.
        pub fn exit_source(&self) -> io::Result<ExitSource> {
            let process_id = NonZeroI32::new(self.process_id)
                .ok_or_else(|| io::Error::other("child has no process id"))?;
            Ok(ExitSource::new(process_id))
        }

        /// Kills the child with `STOP_SIGNAL`; a child already gone is not an error.
        pub fn terminate(&self) {
            // safety: kill takes plain integers and reports failure through its status.
            unsafe { libc::kill(self.process_id, STOP_SIGNAL) };
        }

        /// Lowers the scheduling priority of the child's process group so doc tests yield cores to the build.
        ///
        /// # Errors
        ///
        /// Returns the `setpriority` code when the kernel refuses.
        pub fn lower_priority(&self) -> Result<(), SpawnError> {
            let who = libc::id_t::try_from(self.process_id).map_err(|_| SpawnError { os_code: libc::EINVAL })?;
            // safety: setpriority takes plain integers and reports failure through its status.
            if unsafe { libc::setpriority(libc::PRIO_PGRP, who, DOC_NICE) } == 0 {
                return Ok(());
            }
            Err(SpawnError {
                os_code: io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO),
            })
        }
    }

    fn duplicate(descriptor: c_int) -> io::Result<OwnedFd> {
        // safety: the descriptor is an open pipe end owned by the child, and the duplicate is owned by the returned OwnedFd alone.
        let copy = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(copy) })
    }

    /// Spawns `program` with `arguments`, the inherited `environment` overlaid by `overrides`, and captures both streams.
    ///
    /// A `program` without a slash is searched on `PATH`. `directory` becomes the child's working directory.
    ///
    /// # Errors
    ///
    /// Returns the pipe, file-action, or `posix_spawn` error code, or `EINVAL` for launch data holding a NUL.
    pub fn spawn(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
    ) -> Result<Child, SpawnError> {
        launch(program, arguments, environment, overrides, directory, false)
    }

    /// Spawns like [`spawn`] but as the leader of a new process group, so one `killpg` reaches the child and everything it spawns.
    ///
    /// # Errors
    ///
    /// Returns the pipe, file-action, or `posix_spawn` error code, or `EINVAL` for launch data holding a NUL.
    pub fn spawn_in_group(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
    ) -> Result<Child, SpawnError> {
        launch(program, arguments, environment, overrides, directory, true)
    }

    fn launch(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
        new_group: bool,
    ) -> Result<Child, SpawnError> {
        let pipes = UnixPipePair::new()
            .map_err(|error: UnixPipeError| SpawnError { os_code: error.code })?;
        spawn_with_pipes(program, arguments, environment, overrides, directory, pipes, new_group)
    }

    fn spawn_with_pipes(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
        pipes: UnixPipePair,
        new_group: bool,
    ) -> Result<Child, SpawnError> {
        let arguments = Arguments::lower(Launch {
            program,
            arguments,
            names: &[],
            trailing: &[],
            inherited: environment,
            overrides,
            directory,
        })?;
        spawn_lowered(&arguments.lowered(), pipes, new_group)
    }

    /// Spawns one exact test child, lowering its launch into `storage` so the call makes no heap allocation.
    ///
    /// The child runs `program --exact name...` followed by `harness_arguments`, in `directory`, with the
    /// inherited `environment` overlaid by `overrides`. `names` is one test name, or several joined by NUL for a shard;
    /// each is lowered as its own argument in the same caller-owned storage.
    ///
    /// # Errors
    ///
    /// Returns [`ExactError::Arguments`] with required and available sizes when the launch does not fit `storage`,
    /// and [`ExactError::Spawn`] with the pipe, file-action, or `posix_spawn` code otherwise.
    pub fn spawn_exact(
        storage: &mut LaunchStorage,
        program: &[u8],
        names: &[u8],
        harness_arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        let exact: &[&[u8]] = &[b"--exact"];
        spawn_stored(storage, program, (exact, names), harness_arguments, environment, overrides, directory)
    }

    /// Children beyond the job limit that may wait created and frozen; Unix creation is cheap enough that none do.
    pub const READY_AHEAD: usize = 0;

    /// Spawns like [`spawn_exact`]; a Unix child has no frozen state, so it runs at once.
    ///
    /// # Errors
    ///
    /// Returns [`ExactError`] like [`spawn_exact`].
    pub fn spawn_exact_ready(
        storage: &mut LaunchStorage,
        program: &[u8],
        names: &[u8],
        harness_arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        spawn_exact(storage, program, names, harness_arguments, environment, overrides, directory)
    }

    /// Starts a child created by [`spawn_exact_ready`]; a Unix child already runs.
    pub fn resume_ready(_child: &Child) {}

    /// Spawns one listing child as `program --list`, or `program --list --ignored`, lowering its launch into `storage`
    /// so the call makes no heap allocation.
    ///
    /// # Errors
    ///
    /// Returns [`ExactError::Arguments`] with required and available sizes when the launch does not fit `storage`,
    /// and [`ExactError::Spawn`] with the pipe, file-action, or `posix_spawn` code otherwise.
    pub fn spawn_listed(
        storage: &mut LaunchStorage,
        program: &[u8],
        ignored: bool,
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        let listing: &[&[u8]] = if ignored { &[b"--list", b"--ignored"] } else { &[b"--list"] };
        spawn_stored(storage, program, (listing, &[]), &[], environment, overrides, directory)
    }

    fn spawn_stored(
        storage: &mut LaunchStorage,
        program: &[u8],
        (arguments, names): (&[&[u8]], &[u8]),
        trailing: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        let lowered = storage
            .lower(Launch {
                program,
                arguments,
                names,
                trailing,
                inherited: environment,
                overrides,
                directory: Some(directory),
            })
            .map_err(ExactError::Arguments)?;
        let pipes = UnixPipePair::new()
            .map_err(|error: UnixPipeError| ExactError::Spawn(SpawnError { os_code: error.code }))?;
        spawn_lowered(&lowered, pipes, false).map_err(ExactError::Spawn)
    }

    fn spawn_lowered(
        lowered: &Lowered<'_>,
        pipes: UnixPipePair,
        new_group: bool,
    ) -> Result<Child, SpawnError> {
        let executable = lowered.program()?;
        let current_directory = lowered.directory()?;
        let (argv, envp) = (lowered.argv(), lowered.envp());
        let mut actions = FileActions::new()?;
        let mut attributes = SpawnAttributes::new(new_group)?;
        #[cfg(target_os = "macos")]
        add_action(unsafe {
            posix_spawn_file_actions_addinherit_np(actions.raw(), libc::STDIN_FILENO)
        })?;
        add_action(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                actions.raw(),
                pipes.stdout().child_write().raw(),
                libc::STDOUT_FILENO,
            )
        })?;
        add_action(unsafe {
            libc::posix_spawn_file_actions_adddup2(
                actions.raw(),
                pipes.stderr().child_write().raw(),
                libc::STDERR_FILENO,
            )
        })?;
        for descriptor in [
            pipes.stdout().parent_read().raw(),
            pipes.stdout().child_write().raw(),
            pipes.stderr().parent_read().raw(),
            pipes.stderr().child_write().raw(),
        ] {
            add_action(unsafe {
                libc::posix_spawn_file_actions_addclose(actions.raw(), descriptor)
            })?;
        }
        if let Some(current_directory) = current_directory {
            add_action(unsafe {
                libc::posix_spawn_file_actions_addchdir_np(
                    actions.raw(),
                    current_directory.as_ptr(),
                )
            })?;
        }

        let mut process_id = 0;
        let result = unsafe {
            libc::posix_spawnp(
                &raw mut process_id,
                executable.as_ptr(),
                actions.raw(),
                attributes.raw(),
                argv.as_ptr().cast::<*mut c_char>(),
                envp.as_ptr().cast::<*mut c_char>(),
            )
        };
        if result != 0 {
            return Err(SpawnError { os_code: result });
        }
        let (stdout, stderr) = pipes.into_parent_reads();
        Ok(Child {
            process_id,
            stdout,
            stderr,
        })
    }

    fn add_action(result: c_int) -> Result<(), SpawnError> {
        if result == 0 {
            Ok(())
        } else {
            Err(SpawnError { os_code: result })
        }
    }

    #[cfg(target_os = "macos")]
    unsafe extern "C" {
        fn posix_spawn_file_actions_addinherit_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            descriptor: c_int,
        ) -> c_int;
    }

    struct FileActions(libc::posix_spawn_file_actions_t);

    impl FileActions {
        fn new() -> Result<Self, SpawnError> {
            let mut actions = core::mem::MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
            add_action(unsafe { libc::posix_spawn_file_actions_init(actions.as_mut_ptr()) })?;
            Ok(Self(unsafe { actions.assume_init() }))
        }

        fn raw(&mut self) -> *mut libc::posix_spawn_file_actions_t {
            &raw mut self.0
        }
    }

    impl Drop for FileActions {
        fn drop(&mut self) {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&raw mut self.0);
            }
        }
    }

    struct SpawnAttributes(libc::posix_spawnattr_t);

    impl SpawnAttributes {
        fn new(new_group: bool) -> Result<Self, SpawnError> {
            let mut attributes = core::mem::MaybeUninit::<libc::posix_spawnattr_t>::uninit();
            add_action(unsafe { libc::posix_spawnattr_init(attributes.as_mut_ptr()) })?;
            let mut owner = Self(unsafe { attributes.assume_init() });
            let mut flags = 0;
            #[cfg(target_os = "macos")]
            {
                flags |= libc::c_int::from(libc::POSIX_SPAWN_CLOEXEC_DEFAULT);
            }
            if new_group {
                flags |= libc::c_int::from(libc::POSIX_SPAWN_SETPGROUP);
                add_action(unsafe { libc::posix_spawnattr_setpgroup(&raw mut owner.0, 0) })?;
            }
            if flags != 0 {
                let flags = libc::c_short::try_from(flags)
                    .map_err(|_| SpawnError { os_code: libc::EINVAL })?;
                add_action(unsafe { libc::posix_spawnattr_setflags(&raw mut owner.0, flags) })?;
            }
            Ok(owner)
        }

        fn raw(&mut self) -> *const libc::posix_spawnattr_t {
            &raw const self.0
        }
    }

    impl Drop for SpawnAttributes {
        fn drop(&mut self) {
            unsafe {
                libc::posix_spawnattr_destroy(&raw mut self.0);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use core::mem::MaybeUninit;

        use std::format;
        use std::string::String;
        use std::vec::Vec;

        use super::{LaunchStorage, SpawnError, spawn, spawn_exact, spawn_with_pipes};
        use crate::edge_spawn_unix::pipe_pair::UnixPipePair;
        use crate::test_probe::probe_name;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct DescriptorIdentity {
            device: libc::dev_t,
            inode: libc::ino_t,
            mode: libc::mode_t,
        }

        #[test]
        fn c38b_launches_exact_child_in_requested_directory_and_owns_reads() {
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable_text = executable
                .to_str()
                .expect("test executable path is valid UTF-8");
            let working_directory =
                crate::test_dir::tempdir().expect("child working directory is created");
            let working_directory_path = working_directory
                .path()
                .canonicalize()
                .expect("temporary working directory resolves to its native path");
            let working_directory_text = working_directory_path
                .to_str()
                .expect("temporary directory path is valid UTF-8");
            let expected_cwd = format!("CARGO_VAL_EXPECTED_CWD={working_directory_text}");
            let inherited_storage = inherited_environment();
            let inherited = inherited_storage.iter().map(String::as_bytes).collect::<Vec<_>>();
            let overrides = [expected_cwd.as_bytes(), b"CARGO_VAL_CHILD_MODE=1".as_slice()];
            let probe = probe_name!(fixture_alpha);
            let arguments = [b"--exact".as_slice(), probe.as_bytes()];
            let pipes = UnixPipePair::new().expect("capture pipes should be created");
            let pipe_descriptors = [
                pipes.stdout().parent_read().raw(),
                pipes.stdout().child_write().raw(),
                pipes.stderr().parent_read().raw(),
                pipes.stderr().child_write().raw(),
            ];
            let pipe_identities = pipe_descriptors.map(descriptor_identity);
            let child = spawn_with_pipes(
                executable_text.as_bytes(),
                &arguments,
                &inherited,
                &overrides,
                Some(working_directory_text.as_bytes()),
                pipes,
                false,
            )
            .expect("child should launch in the selected directory");

            assert!(child.process_id() > 0);
            assert_ne!(child.stdout().raw(), child.stderr().raw());
            for descriptor in [child.stdout().raw(), child.stderr().raw()] {
                let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
                assert!(flags >= 0, "captured descriptor must remain open");
            }
            assert_closed(pipe_descriptors[1], pipe_identities[1]);
            assert_closed(pipe_descriptors[3], pipe_identities[3]);
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(child.process_id().cast_signed(), &raw mut status, 0) },
                child.process_id().cast_signed()
            );
            let mut stdout_bytes = [0; 8192];
            let mut stderr_bytes = [0; 8192];
            let stdout_count = read_capture(child.stdout().raw(), &mut stdout_bytes);
            let stderr_count = read_capture(child.stderr().raw(), &mut stderr_bytes);
            std::println!(
                "{{\"child_status\":{status},\"stdout\":{:?},\"stderr\":{:?}}}",
                std::string::String::from_utf8_lossy(&stdout_bytes[..stdout_count]),
                std::string::String::from_utf8_lossy(&stderr_bytes[..stderr_count])
            );
            assert_eq!(status, 0);
            assert_payload(&stdout_bytes[..stdout_count], b"cargo-val-child-stdout");
            assert_payload(&stderr_bytes[..stderr_count], b"cargo-val-child-stderr");
            let child_id = child.process_id();
            drop(child);
            assert_closed(pipe_descriptors[0], pipe_identities[0]);
            assert_closed(pipe_descriptors[2], pipe_identities[2]);
            std::println!(
                "{{\"children\":1,\"captured_descriptors\":2,\"stdout_payloads\":1,\"stderr_payloads\":1,\"cwd_matches\":1,\"exit_status\":0,\"parent_child_writes_closed\":2,\"captured_reads_closed\":2,\"child_id_nonzero\":{}}}",
                usize::from(child_id > 0)
            );
        }

        #[test]
        fn c38c_exact_test_launch_makes_zero_heap_allocations() {
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable_text = executable
                .to_str()
                .expect("test executable path is valid UTF-8");
            let inherited_storage = inherited_environment();
            let inherited = inherited_storage.iter().map(String::as_bytes).collect::<Vec<_>>();
            let overrides = [b"CARGO_VAL_CHILD_MODE=1".as_slice()];
            let mut storage = LaunchStorage::default();
            let mut observed = Vec::new();

            for _ in 0..2 {
                let (result, allocations) = crate::test_alloc::measure_allocations(|| {
                    spawn_exact(
                        &mut storage,
                        executable_text.as_bytes(),
                        b"edge_spawn_unix::spawn::tests::fixture_alpha",
                        &[],
                        &inherited,
                        &overrides,
                        b"/",
                    )
                });
                let child = result.expect("exact test child should launch");
                let mut status = 0;
                assert_eq!(
                    unsafe { libc::waitpid(child.process_id().cast_signed(), &raw mut status, 0) },
                    child.process_id().cast_signed()
                );
                assert_eq!(status, 0);
                observed.push(allocations);
            }

            std::println!(
                "{{\"exact_launch_allocations\":{observed:?},\"inherited_entries\":{}}}",
                inherited.len()
            );
            assert_eq!(observed, [0, 0], "an exact test launch must not touch the heap");
        }

        #[test]
        fn shard_launch_lowers_every_name_into_caller_storage_with_zero_heap_allocations() {
            const NAMES: &[u8] =
                b"edge_spawn_unix::spawn::tests::fixture_alpha\0edge_spawn_unix::spawn::tests::fixture_beta";
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable_text = executable
                .to_str()
                .expect("test executable path is valid UTF-8");
            let inherited_storage = inherited_environment();
            let inherited = inherited_storage.iter().map(String::as_bytes).collect::<Vec<_>>();
            let mut storage = LaunchStorage::default();
            let mut observed = Vec::new();

            for _ in 0..2 {
                let (result, allocations) = crate::test_alloc::measure_allocations(|| {
                    spawn_exact(
                        &mut storage,
                        executable_text.as_bytes(),
                        NAMES,
                        &[b"--test-threads=1"],
                        &inherited,
                        &[],
                        b"/",
                    )
                });
                let child = result.expect("shard child should launch");
                let mut status = 0;
                assert_eq!(
                    unsafe { libc::waitpid(child.process_id().cast_signed(), &raw mut status, 0) },
                    child.process_id().cast_signed()
                );
                let mut stdout_bytes = [0; 8192];
                let stdout_count = read_capture(child.stdout().raw(), &mut stdout_bytes);
                let stdout = std::string::String::from_utf8_lossy(&stdout_bytes[..stdout_count]).into_owned();
                assert_eq!(status, 0, "{stdout}");
                assert!(stdout.contains("running 2 tests"), "{stdout}");
                assert!(stdout.contains("tests::fixture_alpha ... ok"), "{stdout}");
                assert!(stdout.contains("tests::fixture_beta ... ok"), "{stdout}");
                observed.push(allocations);
            }

            assert_eq!(observed, [0, 0], "a shard launch must not touch the heap");
        }

        #[test]
        fn fixture_beta() {}

        #[test]
        fn c38c_setup_spawn_allocates_its_launch_once() {
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable_text = executable
                .to_str()
                .expect("test executable path is valid UTF-8");
            let inherited_storage = inherited_environment();
            let inherited = inherited_storage.iter().map(String::as_bytes).collect::<Vec<_>>();
            let arguments = [
                b"--exact".as_slice(),
                b"edge_spawn_unix::spawn::tests::fixture_alpha".as_slice(),
            ];

            let (result, allocations) = crate::test_alloc::measure_allocations(|| {
                spawn(executable_text.as_bytes(), &arguments, &inherited, &[], Some(b"/"))
            });

            let child = result.expect("setup child should launch");
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(child.process_id().cast_signed(), &raw mut status, 0) },
                child.process_id().cast_signed()
            );
            std::println!("{{\"setup_launch_allocations\":{allocations}}}");
            assert_eq!(allocations, 3, "string bytes, argv, and envp are the setup launch's only allocations");
        }

        #[test]
        fn c38b_failed_launch_closes_all_capture_descriptors() {
            let pipes = UnixPipePair::new().expect("capture pipes should be created");
            let descriptors = [
                pipes.stdout().parent_read().raw(),
                pipes.stdout().child_write().raw(),
                pipes.stderr().parent_read().raw(),
                pipes.stderr().child_write().raw(),
            ];
            let identities = descriptors.map(descriptor_identity);
            let result = spawn_with_pipes(
                b"/cargo-val-missing-c38b-executable",
                &[b"--exact", b"edge_spawn_unix::spawn::tests::fixture_alpha"],
                &[],
                &[],
                Some(b"/"),
                pipes,
                false,
            );
            match result {
                Err(error) => assert_eq!(error, SpawnError { os_code: libc::ENOENT }),
                Ok(_) => panic!("missing executable unexpectedly launched"),
            }
            for (descriptor, identity) in descriptors.into_iter().zip(identities) {
                assert_closed(descriptor, identity);
            }
            std::println!("{{\"launch_errors\":1,\"owned_descriptors\":0}}");
        }

        #[test]
        fn fixture_alpha() {
            if !std::env::args().any(|argument| argument == "--exact") {
                return;
            }
            let Some(expected_directory) = std::env::var_os("CARGO_VAL_EXPECTED_CWD") else {
                return;
            };
            assert_eq!(
                std::env::current_dir().expect("child cwd is readable"),
                expected_directory
            );
            assert_eq!(std::env::var("CARGO_VAL_CHILD_MODE").as_deref(), Ok("1"));
            let stdout = b"cargo-val-child-stdout";
            let stderr = b"cargo-val-child-stderr";
            assert_eq!(
                unsafe { libc::write(libc::STDOUT_FILENO, stdout.as_ptr().cast(), stdout.len()) }
                    .cast_unsigned(),
                stdout.len()
            );
            assert_eq!(
                unsafe { libc::write(libc::STDERR_FILENO, stderr.as_ptr().cast(), stderr.len()) }
                    .cast_unsigned(),
                stderr.len()
            );
        }

        fn inherited_environment() -> Vec<String> {
            std::env::vars()
                .map(|(key, value)| format!("{key}={value}"))
                .collect()
        }

        fn read_capture(descriptor: i32, bytes: &mut [u8; 8192]) -> usize {
            let mut offset = 0;
            loop {
                let count = unsafe {
                    libc::read(
                        descriptor,
                        bytes[offset..].as_mut_ptr().cast(),
                        bytes.len() - offset,
                    )
                };
                if count < 0 && last_error_code() == libc::EAGAIN {
                    wait_readable(descriptor);
                    continue;
                }
                assert!(
                    count >= 0,
                    "nonblocking child output read must succeed, errno {}",
                    last_error_code()
                );
                if count == 0 {
                    break;
                }
                offset += count.cast_unsigned();
                assert!(
                    offset < bytes.len(),
                    "child test output fits caller storage"
                );
            }
            offset
        }

        fn wait_readable(descriptor: i32) {
            let mut poll_descriptor = libc::pollfd {
                fd: descriptor,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&raw mut poll_descriptor, 1, -1) };
            assert!(
                ready == 1 || last_error_code() == libc::EINTR,
                "poll on the capture descriptor must report readiness or hangup, errno {}",
                last_error_code()
            );
        }

        fn assert_payload(bytes: &[u8], expected_payload: &[u8]) {
            assert!(
                bytes
                    .windows(expected_payload.len())
                    .any(|window| window == expected_payload),
                "child output must reach the matching capture descriptor"
            );
        }

        fn descriptor_identity(descriptor: i32) -> DescriptorIdentity {
            let mut metadata = MaybeUninit::<libc::stat>::uninit();
            assert_eq!(unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) }, 0);
            let metadata = unsafe { metadata.assume_init() };
            DescriptorIdentity {
                device: metadata.st_dev,
                inode: metadata.st_ino,
                mode: metadata.st_mode,
            }
        }

        fn assert_closed(descriptor: i32, identity: DescriptorIdentity) {
            if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } == -1 {
                assert_eq!(last_error_code(), libc::EBADF);
            } else {
                assert_ne!(descriptor_identity(descriptor), identity);
            }
        }

        fn last_error_code() -> i32 {
            #[cfg(target_os = "macos")]
            {
                unsafe { *libc::__error() }
            }
            #[cfg(target_os = "linux")]
            {
                unsafe { *libc::__errno_location() }
            }
        }
    }
}
pub use spawn::*;
