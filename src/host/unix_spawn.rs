//! Unix child creation using borrowed launch data and owned capture descriptors.

use core::ffi::{CStr, c_char, c_int};

use super::unix_pipe_pair::{OwnedDescriptor, UnixPipeError, UnixPipePair};
use crate::unix_arguments::UnixArguments;

/// A child process whose captured output remains owned by the host driver.
#[derive(Debug)]
pub struct UnixChild {
    process_id: libc::pid_t,
    stdout: OwnedDescriptor,
    stderr: OwnedDescriptor,
}

/// A `posix_spawn` or file-action error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixSpawnError {
    /// The error code returned directly by the POSIX operation.
    pub code: c_int,
}

impl UnixChild {
    /// Returns the process identifier watched by the host event source.
    #[must_use]
    pub fn process_id(&self) -> libc::pid_t {
        self.process_id
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
    pub fn into_parts(self) -> (libc::pid_t, OwnedDescriptor, OwnedDescriptor) {
        (self.process_id, self.stdout, self.stderr)
    }
}

/// Spawns one exact-test intent in `current_directory` and captures both streams.
///
/// # Errors
///
/// Returns the pipe, file-action, or `posix_spawn` error code.
pub fn spawn_unix(
    arguments: &UnixArguments<'_>,
    current_directory: &CStr,
) -> Result<UnixChild, UnixSpawnError> {
    let pipes =
        UnixPipePair::new().map_err(|error: UnixPipeError| UnixSpawnError { code: error.code })?;
    spawn_with_pipes(arguments, current_directory, pipes)
}

fn spawn_with_pipes(
    arguments: &UnixArguments<'_>,
    current_directory: &CStr,
    pipes: UnixPipePair,
) -> Result<UnixChild, UnixSpawnError> {
    spawn_raw_with_pipes(
        unsafe { CStr::from_ptr(arguments.argv()[0]) },
        arguments.argv(),
        arguments.envp(),
        Some(current_directory),
        false,
        pipes,
        false,
    )
}

/// Spawns a borrowed argv through the same owned capture-pipe path.
///
/// # Errors
///
/// Returns the pipe, file-action, or `posix_spawn` error code.
pub fn spawn_raw_unix(
    executable: &CStr,
    argv: &[*const c_char],
    envp: &[*const c_char],
    current_directory: Option<&CStr>,
    search_path: bool,
) -> Result<UnixChild, UnixSpawnError> {
    let pipes =
        UnixPipePair::new().map_err(|error: UnixPipeError| UnixSpawnError { code: error.code })?;
    spawn_raw_with_pipes(
        executable,
        argv,
        envp,
        current_directory,
        search_path,
        pipes,
        false,
    )
}

/// Spawns like [`spawn_raw_unix`] but as the leader of a new process group, so one `killpg`
/// reaches the child and everything it spawns.
///
/// # Errors
///
/// Returns the pipe, file-action, or `posix_spawn` error code.
pub fn spawn_raw_unix_in_new_group(
    executable: &CStr,
    argv: &[*const c_char],
    envp: &[*const c_char],
    current_directory: Option<&CStr>,
    search_path: bool,
) -> Result<UnixChild, UnixSpawnError> {
    let pipes =
        UnixPipePair::new().map_err(|error: UnixPipeError| UnixSpawnError { code: error.code })?;
    spawn_raw_with_pipes(
        executable,
        argv,
        envp,
        current_directory,
        search_path,
        pipes,
        true,
    )
}

fn spawn_raw_with_pipes(
    executable: &CStr,
    argv: &[*const c_char],
    envp: &[*const c_char],
    current_directory: Option<&CStr>,
    search_path: bool,
    pipes: UnixPipePair,
    new_group: bool,
) -> Result<UnixChild, UnixSpawnError> {
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
        add_action(unsafe { libc::posix_spawn_file_actions_addclose(actions.raw(), descriptor) })?;
    }
    if let Some(current_directory) = current_directory {
        add_action(unsafe {
            libc::posix_spawn_file_actions_addchdir_np(actions.raw(), current_directory.as_ptr())
        })?;
    }

    let mut process_id = 0;
    let argv = argv.as_ptr().cast::<*mut c_char>();
    let envp = envp.as_ptr().cast::<*mut c_char>();
    let spawn = if search_path {
        libc::posix_spawnp
    } else {
        libc::posix_spawn
    };
    let result = unsafe {
        spawn(
            &raw mut process_id,
            executable.as_ptr(),
            actions.raw(),
            attributes.raw(),
            argv,
            envp,
        )
    };
    if result != 0 {
        return Err(UnixSpawnError { code: result });
    }
    let (stdout, stderr) = pipes.into_parent_reads();
    Ok(UnixChild {
        process_id,
        stdout,
        stderr,
    })
}

fn add_action(result: c_int) -> Result<(), UnixSpawnError> {
    if result == 0 {
        Ok(())
    } else {
        Err(UnixSpawnError { code: result })
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
    fn new() -> Result<Self, UnixSpawnError> {
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
    fn new(new_group: bool) -> Result<Self, UnixSpawnError> {
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
                .map_err(|_| UnixSpawnError { code: libc::EINVAL })?;
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

    use core::ffi::CStr;
    use std::ffi::CString;
    use std::format;
    use std::vec::Vec;

    use super::{UnixSpawnError, spawn_with_pipes};
    use crate::ExecutablePath;
    use crate::host::unix_pipe_pair::UnixPipePair;
    use crate::test_intent::ExactTestIntent;
    use crate::unix_arguments::{UnixArguments, lower_unix_arguments};

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
        let expected_cwd = CString::new(format!("CARGO_VAL_EXPECTED_CWD={working_directory_text}"))
            .expect("temporary directory has no NUL byte");
        let child_mode = CString::new("CARGO_VAL_CHILD_MODE=1")
            .expect("constant environment key has no NUL byte");
        let inherited_storage = inherited_environment();
        let inherited = inherited_storage
            .iter()
            .map(CString::as_c_str)
            .collect::<Vec<_>>();
        let overrides = [expected_cwd.as_c_str(), child_mode.as_c_str()];
        let intent = exact_intent(executable_text, "host::unix_spawn::tests::fixture_alpha");
        let mut argument_bytes = [0; 4096];
        let mut argument_pointers = [core::ptr::null(); 4];
        let mut environment_pointers = [core::ptr::null(); 4096];
        let arguments = lower_arguments(
            &intent,
            &inherited,
            &overrides,
            &mut argument_bytes,
            &mut argument_pointers,
            &mut environment_pointers,
        );
        let pipes = UnixPipePair::new().expect("capture pipes should be created");
        let pipe_descriptors = [
            pipes.stdout().parent_read().raw(),
            pipes.stdout().child_write().raw(),
            pipes.stderr().parent_read().raw(),
            pipes.stderr().child_write().raw(),
        ];
        let pipe_identities = pipe_descriptors.map(descriptor_identity);
        let working_directory_cstr =
            CString::new(working_directory_text).expect("temporary directory has no NUL byte");
        let child = spawn_with_pipes(&arguments, &working_directory_cstr, pipes)
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
            unsafe { libc::waitpid(child.process_id(), &raw mut status, 0) },
            child.process_id()
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
    fn c38b_failed_launch_closes_all_capture_descriptors() {
        let intent = exact_intent(
            "/cargo-val-missing-c38b-executable",
            "host::unix_spawn::tests::fixture_alpha",
        );
        let pipes = UnixPipePair::new().expect("capture pipes should be created");
        let descriptors = [
            pipes.stdout().parent_read().raw(),
            pipes.stdout().child_write().raw(),
            pipes.stderr().parent_read().raw(),
            pipes.stderr().child_write().raw(),
        ];
        let identities = descriptors.map(descriptor_identity);
        let inherited: [&CStr; 0] = [];
        let overrides: [&CStr; 0] = [];
        let mut argument_bytes = [0; 96];
        let mut argument_pointers = [core::ptr::null(); 4];
        let mut environment_pointers = [core::ptr::null(); 1];
        let arguments = lower_arguments(
            &intent,
            &inherited,
            &overrides,
            &mut argument_bytes,
            &mut argument_pointers,
            &mut environment_pointers,
        );
        let working_directory = CString::new("/").expect("root path has no NUL byte");
        let result = spawn_with_pipes(&arguments, &working_directory, pipes);
        match result {
            Err(error) => assert_eq!(error, UnixSpawnError { code: libc::ENOENT }),
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

    fn exact_intent<'name>(
        executable: &'name str,
        test_name: &'name str,
    ) -> crate::test_intent::ExactTestIntent<'name, 'static, 'name> {
        ExactTestIntent::new(ExecutablePath::Borrowed(executable), test_name)
    }
    fn lower_arguments<'storage>(
        intent: &crate::test_intent::ExactTestIntent<'_, '_, '_>,
        inherited: &[&'storage CStr],
        overrides: &[&'storage CStr],
        argument_bytes: &'storage mut [u8],
        argument_pointers: &'storage mut [*const core::ffi::c_char],
        environment_pointers: &'storage mut [*const core::ffi::c_char],
    ) -> UnixArguments<'storage> {
        lower_unix_arguments(
            intent,
            inherited,
            overrides,
            &[],
            argument_bytes,
            argument_pointers,
            environment_pointers,
        )
        .expect("caller storage fits the child launch")
    }

    fn inherited_environment() -> Vec<CString> {
        std::env::vars()
            .map(|(key, value)| {
                CString::new(format!("{key}={value}"))
                    .expect("environment entries cannot contain NUL bytes")
            })
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
