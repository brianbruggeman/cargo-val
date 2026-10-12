//! Windows edge for spawning a child with captured stdout and stderr.

pub mod handles {
    //! Caller-storage `STARTUPINFOEXW` handle lists for isolated child inheritance.

    use core::ffi::c_void;
    use core::mem::{MaybeUninit, size_of};
    use std::os::windows::io::BorrowedHandle;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetHandleInformation, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute,
    };

    use crate::edge_args_windows::{
        WINDOWS_BELOW_NORMAL_PRIORITY, windows_attribute_count, windows_attribute_storage_words,
        windows_job_limit_flags,
    };

    /// Owns one Win32 handle shared by child launch and host I/O adapters.
    #[derive(Debug)]
    pub(crate) struct WindowsOwnedHandle(HANDLE);

    impl WindowsOwnedHandle {
        pub(crate) const fn new(handle: HANDLE) -> Self {
            Self(handle)
        }

        pub(crate) const fn raw(&self) -> HANDLE {
            self.0
        }

        /// Releases the raw handle without closing it; the caller closes it.
        pub(crate) fn into_raw(self) -> HANDLE {
            core::mem::ManuallyDrop::new(self).0
        }

        /// Lends the handle to a prime gate.
        pub(crate) fn as_borrowed(&self) -> BorrowedHandle<'_> {
            // safety: the handle is open for as long as `self` lives, which outlives this borrow.
            unsafe { BorrowedHandle::borrow_raw(self.0) }
        }
    }

    impl Drop for WindowsOwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// Failures while creating the run-scoped job object.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowsJobError {
        /// `CreateJobObjectW` rejected the request.
        CreateFailed { code: u32 },
        /// `SetInformationJobObject` rejected the limit flags.
        LimitFailed { code: u32 },
        /// The limit structure size does not fit its `u32` length field.
        SizeOverflow,
    }

    /// One job object per run; closing it kills every child created in it.
    ///
    /// Children join at creation through `PROC_THREAD_ATTRIBUTE_JOB_LIST` in
    /// [`WindowsHandleList::new_in_job`], so there is no suspend, assign, resume.
    pub struct WindowsJob {
        handle: WindowsOwnedHandle,
    }

    impl WindowsJob {
        /// Creates the job with kill-on-close, die-on-unhandled-exception, and below-normal priority.
        ///
        /// # Errors
        ///
        /// Returns [`WindowsJobError`] when Windows rejects job creation or its limits.
        pub fn new() -> Result<Self, WindowsJobError> {
            let raw = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
            if raw.is_null() {
                return Err(WindowsJobError::CreateFailed {
                    code: unsafe { GetLastError() },
                });
            }
            let handle = WindowsOwnedHandle::new(raw);
            let limits_bytes = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .map_err(|_| WindowsJobError::SizeOverflow)?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = windows_job_limit_flags();
            limits.BasicLimitInformation.PriorityClass = WINDOWS_BELOW_NORMAL_PRIORITY;
            let configured = unsafe {
                SetInformationJobObject(
                    handle.raw(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    limits_bytes,
                )
            };
            if configured == 0 {
                return Err(WindowsJobError::LimitFailed {
                    code: unsafe { GetLastError() },
                });
            }
            Ok(Self { handle })
        }

        /// Returns the borrowed job handle for membership queries.
        #[must_use]
        pub const fn raw(&self) -> HANDLE {
            self.handle.raw()
        }
    }

    // a job handle is a kernel object that any thread may use
    unsafe impl Send for WindowsJob {}
    unsafe impl Sync for WindowsJob {}

    impl WindowsJob {
        /// Releases the job handle without closing it; the caller closes it.
        pub(crate) fn into_raw(self) -> HANDLE {
            self.handle.into_raw()
        }
    }

    /// Failures while constructing the bounded child stdio handle list.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowsHandleListError {
        /// A standard stream handle is null or invalid.
        InvalidHandle { index: usize },
        /// A standard stream handle is not inheritable, as required by Win32.
        HandleNotInheritable { index: usize },
        /// The caller's storage cannot hold the Win32 attribute list.
        AttributeStorageTooSmall {
            required_bytes: usize,
            available_bytes: usize,
        },
        /// The operating system rejected attribute-list initialization.
        InitializeFailed { code: u32 },
        /// The operating system rejected the three-handle list.
        UpdateFailed { code: u32 },
        /// The operating system rejected the job list.
        JobUpdateFailed { code: u32 },
        /// A storage-size calculation overflowed.
        SizeOverflow,
    }

    /// One initialized Win32 attribute list borrowing its handles and backing storage.
    pub struct WindowsHandleList<'storage> {
        storage: &'storage mut [MaybeUninit<usize>],
        handles: &'storage [HANDLE; 3],
        job: Option<&'storage WindowsJob>,
        startup_info: STARTUPINFOEXW,
        initialized: bool,
    }

    impl<'storage> WindowsHandleList<'storage> {
        /// Builds a one-attribute handle whitelist without allocating storage; children are in no job.
        ///
        /// # Errors
        ///
        /// Returns [`WindowsHandleListError`] when a handle is invalid or not inheritable, attribute storage is too small, or Windows rejects the attribute list.
        pub fn new(
            handles: &'storage [HANDLE; 3],
            storage: &'storage mut [MaybeUninit<usize>],
        ) -> Result<Self, WindowsHandleListError> {
            Self::build(handles, None, storage)
        }

        /// Builds the handle whitelist plus a job list so the child is created inside `job`.
        ///
        /// # Errors
        ///
        /// Returns [`WindowsHandleListError`] when a handle is invalid or not inheritable, attribute storage is too small, or Windows rejects either attribute.
        pub fn new_in_job(
            handles: &'storage [HANDLE; 3],
            job: &'storage WindowsJob,
            storage: &'storage mut [MaybeUninit<usize>],
        ) -> Result<Self, WindowsHandleListError> {
            Self::build(handles, Some(job), storage)
        }

        fn build(
            handles: &'storage [HANDLE; 3],
            job: Option<&'storage WindowsJob>,
            storage: &'storage mut [MaybeUninit<usize>],
        ) -> Result<Self, WindowsHandleListError> {
            for (index, handle) in handles.iter().enumerate() {
                if handle.is_null() || *handle == INVALID_HANDLE_VALUE {
                    return Err(WindowsHandleListError::InvalidHandle { index });
                }
                let mut flags = 0;
                if unsafe { GetHandleInformation(*handle, &raw mut flags) } == 0 {
                    return Err(WindowsHandleListError::InvalidHandle { index });
                }
                if flags & HANDLE_FLAG_INHERIT == 0 {
                    return Err(WindowsHandleListError::HandleNotInheritable { index });
                }
            }

            let attribute_count = windows_attribute_count(job.is_some());
            let mut required_bytes = 0;
            unsafe {
                InitializeProcThreadAttributeList(
                    core::ptr::null_mut(),
                    attribute_count,
                    0,
                    &raw mut required_bytes,
                );
            }
            let available_bytes = storage
                .len()
                .checked_mul(size_of::<usize>())
                .ok_or(WindowsHandleListError::SizeOverflow)?;
            if required_bytes == 0
                || storage.len() < windows_attribute_storage_words(required_bytes)
            {
                return Err(WindowsHandleListError::AttributeStorageTooSmall {
                    required_bytes,
                    available_bytes,
                });
            }

            let attribute_list = storage.as_mut_ptr().cast::<c_void>();
            if unsafe {
                InitializeProcThreadAttributeList(
                    attribute_list,
                    attribute_count,
                    0,
                    &raw mut required_bytes,
                )
            } == 0
            {
                return Err(WindowsHandleListError::InitializeFailed {
                    code: unsafe { GetLastError() },
                });
            }
            if unsafe {
                UpdateProcThreadAttribute(
                    attribute_list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    size_of::<[HANDLE; 3]>(),
                    core::ptr::null_mut(),
                    core::ptr::null(),
                )
            } == 0
            {
                let code = unsafe { GetLastError() };
                unsafe { DeleteProcThreadAttributeList(attribute_list) };
                return Err(WindowsHandleListError::UpdateFailed { code });
            }
            if let Some(job) = job
                && unsafe {
                    UpdateProcThreadAttribute(
                        attribute_list,
                        0,
                        PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                        (&raw const job.handle.0).cast(),
                        size_of::<HANDLE>(),
                        core::ptr::null_mut(),
                        core::ptr::null(),
                    )
                } == 0
            {
                let code = unsafe { GetLastError() };
                unsafe { DeleteProcThreadAttributeList(attribute_list) };
                return Err(WindowsHandleListError::JobUpdateFailed { code });
            }

            let mut startup_info = STARTUPINFOEXW::default();
            startup_info.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())
                .map_err(|_| WindowsHandleListError::SizeOverflow)?;
            startup_info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup_info.StartupInfo.hStdInput = handles[0];
            startup_info.StartupInfo.hStdOutput = handles[1];
            startup_info.StartupInfo.hStdError = handles[2];
            startup_info.lpAttributeList = attribute_list;
            Ok(Self {
                storage,
                handles,
                job,
                startup_info,
                initialized: true,
            })
        }

        /// Returns the startup structure to pass with `EXTENDED_STARTUPINFO_PRESENT`.
        #[must_use]
        pub const fn startup_info(&self) -> &STARTUPINFOEXW {
            &self.startup_info
        }

        /// Returns the job every child created from this list joins, if any.
        #[must_use]
        pub const fn job(&self) -> Option<&WindowsJob> {
            self.job
        }

        /// Returns the exact three handles admitted for child inheritance.
        #[must_use]
        pub const fn handles(&self) -> &[HANDLE; 3] {
            self.handles
        }
    }

    impl Drop for WindowsHandleList<'_> {
        fn drop(&mut self) {
            if self.initialized {
                unsafe { DeleteProcThreadAttributeList(self.startup_info.lpAttributeList) };
                self.initialized = false;
            }
            let _ = self.storage;
        }
    }

    #[cfg(test)]
    pub(crate) mod tests {
        use core::ffi::c_void;
        use core::mem::{MaybeUninit, size_of};

        use windows_sys::Win32::Foundation::{
            CloseHandle, GetHandleInformation, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        };
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
        use windows_sys::Win32::System::Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
            GetCurrentProcess, GetExitCodeProcess, GetProcessHandleCount, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, STARTUPINFOW, WaitForSingleObject,
        };

        use super::{WindowsHandleList, WindowsHandleListError};
        use crate::edge_args_windows::lower_windows_arguments;
        use crate::test_probe::{ProbeOutcome, probe_name, verify_probe_output};

        pub(crate) struct TestHandle(HANDLE);

        impl TestHandle {
            pub(crate) const fn raw(&self) -> HANDLE {
                self.0
            }
        }

        impl Drop for TestHandle {
            fn drop(&mut self) {
                if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                    unsafe { CloseHandle(self.0) };
                }
            }
        }

        pub(crate) fn create_pipe() -> (TestHandle, TestHandle) {
            let security = SECURITY_ATTRIBUTES {
                nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                    .expect("struct size fits u32"),
                lpSecurityDescriptor: core::ptr::null_mut(),
                bInheritHandle: 1,
            };
            let mut read_handle = core::ptr::null_mut();
            let mut write_handle = core::ptr::null_mut();
            assert_eq!(
                unsafe {
                    CreatePipe(
                        &raw mut read_handle,
                        &raw mut write_handle,
                        &raw const security,
                        0,
                    )
                },
                1,
                "fixture pipe handles are created"
            );
            (TestHandle(read_handle), TestHandle(write_handle))
        }

        pub(crate) fn captured_output(read: &TestHandle) -> std::string::String {
            let mut buffer = [0_u8; 8192];
            let mut available = 0_u32;
            assert_eq!(
                unsafe {
                    PeekNamedPipe(
                        read.0,
                        buffer.as_mut_ptr().cast(),
                        u32::try_from(buffer.len()).expect("buffer length fits u32"),
                        &raw mut available,
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                    )
                },
                1,
                "child stdout pipe is peekable"
            );
            std::string::String::from_utf8_lossy(&buffer[..available as usize]).into_owned()
        }

        pub(crate) fn assert_child_ran(read: &TestHandle, expected: ProbeOutcome) {
            let stdout = captured_output(read);
            if let Err(error) = verify_probe_output(&stdout, expected) {
                panic!(
                    "child probe did not run exactly one test as {expected:?}: {error:?}; child stdout was {stdout:?}"
                );
            }
        }

        pub(crate) fn exact_test_intent<'text>(
            executable: &'text str,
            test_name: &'text str,
        ) -> [&'text str; 3] {
            [executable, "--exact", test_name]
        }

        fn launch_probe(
            command_line: &mut [u16],
            environment_block: &[u16],
            startup_info: &STARTUPINFOEXW,
            extended: bool,
        ) -> u32 {
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is representable as UTF-8");
            let executable_wide = executable
                .encode_utf16()
                .chain([0])
                .collect::<std::vec::Vec<_>>();
            let mut process_information = unsafe { core::mem::zeroed() };
            let startup_info = core::ptr::from_ref::<STARTUPINFOEXW>(startup_info);
            let startup_info = startup_info.cast();
            let creation_flags = CREATE_UNICODE_ENVIRONMENT
                | if extended {
                    EXTENDED_STARTUPINFO_PRESENT
                } else {
                    0
                };
            assert_eq!(
                unsafe {
                    CreateProcessW(
                        executable_wide.as_ptr(),
                        command_line.as_mut_ptr(),
                        core::ptr::null(),
                        core::ptr::null(),
                        1,
                        creation_flags,
                        environment_block.as_ptr().cast::<c_void>(),
                        core::ptr::null(),
                        startup_info,
                        &raw mut process_information,
                    )
                },
                1,
                "child process is created with the requested inheritance mode"
            );
            assert_eq!(
                unsafe { WaitForSingleObject(process_information.hProcess, u32::MAX) },
                WAIT_OBJECT_0,
                "child test process exits"
            );
            let mut exit_code = u32::MAX;
            assert_eq!(
                unsafe { GetExitCodeProcess(process_information.hProcess, &raw mut exit_code) },
                1,
                "child process exit code is readable"
            );
            unsafe {
                CloseHandle(process_information.hThread);
                CloseHandle(process_information.hProcess);
            }
            exit_code
        }

        #[test]
        fn c44a_restricts_child_inheritance_to_three_stdio_handles() {
            let (stdin_read, stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (stderr_read, stderr_write) = create_pipe();
            let (sentinel_read, sentinel_write) = create_pipe();
            let (control_stdout_read, control_stdout_write) = create_pipe();
            let stdio_handles = [stdin_read.0, stdout_write.0, stderr_write.0];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&stdio_handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            assert_eq!(handle_list.handles(), &stdio_handles);
            assert_eq!(
                handle_list.startup_info().StartupInfo.dwFlags,
                STARTF_USESTDHANDLES
            );
            write_sentinel_bytes(&sentinel_write);
            let sentinel_id = sentinel_read.0 as usize;
            let sentinel_environment = std::format!("CARGO_VAL_SENTINEL_HANDLE={sentinel_id}");
            let expected_environment = "CARGO_VAL_EXPECT_SENTINEL=0";
            let overrides = [sentinel_environment.as_str(), expected_environment];
            let executable_path = std::env::current_exe().expect("test executable path exists");
            let executable_path = executable_path
                .to_str()
                .expect("Windows test executable path is representable as UTF-8");
            let probe_name = probe_name!(child_handle_probe);
            let intent = exact_test_intent(executable_path, &probe_name);
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 256];
            let mut environment_order = [usize::MAX; 2];
            let mut lowered = lower_windows_arguments(
                &intent,
                &[],
                &overrides,
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the exact child probe invocation");
            let (command_line, environment_block) = lowered.create_process_buffers();
            let restricted_exit = launch_probe(
                command_line,
                environment_block,
                handle_list.startup_info(),
                true,
            );
            assert_eq!(
                restricted_exit, 0,
                "restricted child does not inherit sentinel"
            );
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);

            let unrestricted_environment = "CARGO_VAL_EXPECT_SENTINEL=1";
            let overrides = [sentinel_environment.as_str(), unrestricted_environment];
            let probe_name = probe_name!(child_handle_probe);
            let intent = exact_test_intent(executable_path, &probe_name);
            let mut unrestricted_command_line = [0_u16; 2048];
            let mut unrestricted_environment_block = [0_u16; 256];
            let mut unrestricted_order = [usize::MAX; 2];
            let mut unrestricted = lower_windows_arguments(
                &intent,
                &[],
                &overrides,
                &mut unrestricted_command_line,
                &mut unrestricted_environment_block,
                &mut unrestricted_order,
            )
            .expect("caller buffers fit the inheritance control");
            let (command_line, environment_block) = unrestricted.create_process_buffers();
            let mut basic_startup = STARTUPINFOEXW::default();
            basic_startup.StartupInfo.cb =
                u32::try_from(size_of::<STARTUPINFOW>()).expect("struct size fits u32");
            basic_startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            basic_startup.StartupInfo.hStdInput = stdin_read.0;
            basic_startup.StartupInfo.hStdOutput = control_stdout_write.0;
            basic_startup.StartupInfo.hStdError = stderr_write.0;
            basic_startup.lpAttributeList = core::ptr::null_mut();
            let control_exit = launch_probe(command_line, environment_block, &basic_startup, false);
            assert_eq!(
                control_exit, 0,
                "unrestricted control observes inherited sentinel"
            );
            assert_child_ran(&control_stdout_read, ProbeOutcome::Passed);
            std::println!(
                "{{\"stdio_handles\":{},\"restricted_sentinel_inherited\":0,\"unrestricted_control_sentinel_inherited\":1,\"restricted_exit\":{restricted_exit},\"control_exit\":{control_exit}}}",
                handle_list.handles().len()
            );
            drop((
                stdin_write,
                stdout_read,
                stderr_read,
                sentinel_write,
                control_stdout_read,
                control_stdout_write,
            ));
        }

        #[test]
        fn c44a_rejects_builder_storage_shortage_without_handle_leaks() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, _stdout_write) = create_pipe();
            let (stderr_read, _stderr_write) = create_pipe();
            let stdio_handles = [stdin_read.0, stdout_read.0, stderr_read.0];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 1];
            let mut handle_count_before = 0;
            assert_eq!(
                unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut handle_count_before) },
                1
            );
            let result = WindowsHandleList::new(&stdio_handles, &mut attribute_storage);
            assert!(matches!(
                result,
                Err(WindowsHandleListError::AttributeStorageTooSmall { .. })
            ));
            let mut handle_count_after = 0;
            assert_eq!(
                unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut handle_count_after) },
                1
            );
            assert_eq!(handle_count_after, handle_count_before);
            std::println!("{{\"attribute_storage_too_small\":1,\"handle_count_delta\":0}}");
        }

        const SENTINEL_BYTES: &[u8] = b"cargo-val-c44a-sentinel";

        fn write_sentinel_bytes(write: &TestHandle) {
            let mut written = 0;
            assert_eq!(
                unsafe {
                    WriteFile(
                        write.0,
                        SENTINEL_BYTES.as_ptr(),
                        u32::try_from(SENTINEL_BYTES.len()).expect("sentinel length fits u32"),
                        &raw mut written,
                        core::ptr::null_mut(),
                    )
                },
                1,
                "sentinel pipe accepts its identity bytes"
            );
        }

        // a numeric handle value can name a different open object in the child, so identity is the bytes waiting in the sentinel pipe
        fn holds_sentinel_bytes(handle: HANDLE) -> bool {
            let mut buffer = [0_u8; SENTINEL_BYTES.len()];
            let mut available = 0;
            let peeked = unsafe {
                PeekNamedPipe(
                    handle,
                    buffer.as_mut_ptr().cast(),
                    u32::try_from(buffer.len()).expect("buffer length fits u32"),
                    &raw mut available,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            };
            peeked != 0 && buffer == *SENTINEL_BYTES
        }

        #[test]
        fn child_handle_probe() {
            let Ok(sentinel_value) = std::env::var("CARGO_VAL_SENTINEL_HANDLE") else {
                return;
            };
            let sentinel = sentinel_value
                .parse::<usize>()
                .expect("sentinel handle value is decimal");
            let sentinel = sentinel as HANDLE;
            let mut flags = 0;
            let sentinel_inherited = holds_sentinel_bytes(sentinel);
            let expected = std::env::var("CARGO_VAL_EXPECT_SENTINEL")
                .expect("expected sentinel state is supplied by parent");
            assert_eq!(sentinel_inherited, expected == "1");
            for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = unsafe { GetStdHandle(standard_handle) };
                assert!(!handle.is_null());
                assert_ne!(handle, INVALID_HANDLE_VALUE);
                assert_eq!(unsafe { GetHandleInformation(handle, &raw mut flags) }, 1);
            }
        }
    }
}

pub mod pipe_pair {
    //! Overlapped Windows pipe endpoints for child process output.

    use core::mem::size_of;

    use windows_sys::Win32::Foundation::{
        GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use crate::edge_spawn_windows::handles::WindowsOwnedHandle;

    // nonpaged pool is billed per pipe byte and reads drain in 64 KiB slots, so one page suffices
    const PIPE_KERNEL_BUFFER_BYTES: u32 = 4096;

    /// Failure while creating one overlapped parent-read/child-write pipe pair.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowsPipePairError {
        /// The supplied name is not a single NUL-terminated UTF-16 string.
        InvalidName,
        /// Windows rejected creation of the overlapped parent endpoint.
        CreateServerFailed { code: u32 },
        /// Windows rejected opening the inheritable child endpoint.
        OpenChildWriterFailed { code: u32 },
        /// A Win32 structure size does not fit its `u32` length field.
        SizeOverflow,
    }

    /// Owns an overlapped parent reader and inheritable child writer.
    ///
    /// The parent endpoint is read by an overlapped completion adapter using
    /// stable caller-owned session storage. Its completed byte slice enters the
    /// existing Proxima `HostEventPipe` flow. Pass `child_write_handle` into
    /// [`super::handles::WindowsHandleList`] for child stdio, then call
    /// [`Self::close_child_write_handle`] after process creation so child exit can
    /// produce EOF on the parent reader.
    pub struct WindowsPipePair {
        parent_reader: WindowsOwnedHandle,
        child_writer: Option<WindowsOwnedHandle>,
    }

    impl WindowsPipePair {
        /// Creates one byte-mode named pipe from a caller-provided unique name.
        ///
        /// `pipe_name` must be a unique, fully qualified Win32 pipe name ending in
        /// one NUL unit, for example `\\.\pipe\cargo-val-123-4\0`.
        ///
        /// # Errors
        ///
        /// Returns [`WindowsPipePairError`] for an invalid name, a size that overflows `u32`, or when Windows rejects either endpoint.
        pub fn new(pipe_name: &[u16]) -> Result<Self, WindowsPipePairError> {
            let parent_reader = create_server(pipe_name)?;
            let child_writer = open_client(pipe_name)?;
            Ok(Self {
                parent_reader,
                child_writer: Some(child_writer),
            })
        }

        /// Returns the overlapped parent-side read handle.
        #[must_use]
        pub const fn parent_read_handle(&self) -> HANDLE {
            self.parent_reader.raw()
        }

        /// Returns the inheritable write handle for the child's standard stream.
        #[must_use]
        pub const fn child_write_handle(&self) -> Option<HANDLE> {
            match &self.child_writer {
                Some(handle) => Some(handle.raw()),
                None => None,
            }
        }

        /// Closes this process's writer after the child inherited its copy.
        pub fn close_child_write_handle(&mut self) {
            self.child_writer = None;
        }

        /// Keeps the overlapped parent reader and closes the child writer if it is still open.
        pub(crate) fn into_parent_reader(self) -> WindowsOwnedHandle {
            self.parent_reader
        }
    }

    /// Creates the overlapped parent-side instance of a unique named pipe; it listens for one client at a time.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsPipePairError`] for an invalid name or when Windows rejects creation.
    pub(crate) fn create_server(pipe_name: &[u16]) -> Result<WindowsOwnedHandle, WindowsPipePairError> {
        if pipe_name.len() < 2
            || pipe_name.last() != Some(&0)
            || pipe_name[..pipe_name.len() - 1].contains(&0)
        {
            return Err(WindowsPipePairError::InvalidName);
        }
        let parent_reader = unsafe {
            CreateNamedPipeW(
                pipe_name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                0,
                PIPE_KERNEL_BUFFER_BYTES,
                0,
                core::ptr::null(),
            )
        };
        if parent_reader == INVALID_HANDLE_VALUE || parent_reader.is_null() {
            return Err(WindowsPipePairError::CreateServerFailed {
                code: unsafe { GetLastError() },
            });
        }
        Ok(WindowsOwnedHandle::new(parent_reader))
    }

    /// Opens the inheritable child-side writer of a listening pipe instance.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsPipePairError`] when Windows rejects the open.
    pub(crate) fn open_client(pipe_name: &[u16]) -> Result<WindowsOwnedHandle, WindowsPipePairError> {
        let child_security = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .map_err(|_| WindowsPipePairError::SizeOverflow)?,
            lpSecurityDescriptor: core::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let child_writer = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_WRITE,
                0,
                &raw const child_security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        if child_writer == INVALID_HANDLE_VALUE || child_writer.is_null() {
            return Err(WindowsPipePairError::OpenChildWriterFailed {
                code: unsafe { GetLastError() },
            });
        }
        Ok(WindowsOwnedHandle::new(child_writer))
    }

    #[cfg(test)]
    mod tests {
        use core::sync::atomic::{AtomicU32, Ordering};

        use std::format;
        use std::vec::Vec;
        use windows_sys::Win32::Foundation::{
            ERROR_IO_PENDING, GetHandleInformation, GetLastError, HANDLE_FLAG_INHERIT,
            INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        };
        use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
        use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};
        use windows_sys::Win32::System::Threading::{
            CreateEventW, GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount,
            WaitForSingleObject,
        };

        use super::{WindowsPipePair, WindowsPipePairError};
        use crate::edge_spawn_windows::handles::WindowsOwnedHandle;
        use crate::edge_spawn_windows::handles::tests::create_pipe;

        static NEXT_PIPE_ID: AtomicU32 = AtomicU32::new(0);

        fn current_process_handle_count() -> u32 {
            let mut count = 0;
            assert_eq!(
                unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut count) },
                1,
                "process handle count is readable"
            );
            count
        }

        fn unique_pipe_name() -> Vec<u16> {
            let pipe_id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
            format!(r"\\.\pipe\cargo-val-c45a-{}-{pipe_id}", unsafe {
                GetCurrentProcessId()
            })
            .encode_utf16()
            .chain(Some(0))
            .collect()
        }

        #[test]
        fn c45a_creates_one_overlapped_parent_reader_and_inheritable_writer() {
            let handles_before = current_process_handle_count();
            let mut pipe_pair = WindowsPipePair::new(&unique_pipe_name())
                .expect("the operating system creates a connected named-pipe pair");
            assert_eq!(current_process_handle_count(), handles_before + 2);

            let parent_reader = pipe_pair.parent_read_handle();
            let child_writer = pipe_pair
                .child_write_handle()
                .expect("the child writer remains open before inheritance");
            let mut parent_flags = 0;
            let mut child_flags = 0;
            assert_eq!(
                unsafe { GetHandleInformation(parent_reader, &raw mut parent_flags) },
                1
            );
            assert_eq!(
                unsafe { GetHandleInformation(child_writer, &raw mut child_flags) },
                1
            );
            assert_eq!(parent_flags & HANDLE_FLAG_INHERIT, 0);
            assert_ne!(child_flags & HANDLE_FLAG_INHERIT, 0);

            let event = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
            assert!(!event.is_null());
            assert_ne!(event, INVALID_HANDLE_VALUE);
            let event = WindowsOwnedHandle::new(event);
            let mut overlapped = OVERLAPPED {
                hEvent: event.raw(),
                ..OVERLAPPED::default()
            };
            let mut read_buffer = [0_u8; 1];
            let mut bytes_read = 0;
            assert_eq!(
                unsafe {
                    ReadFile(
                        parent_reader,
                        read_buffer.as_mut_ptr(),
                        u32::try_from(read_buffer.len()).expect("buffer length fits u32"),
                        &raw mut bytes_read,
                        &raw mut overlapped,
                    )
                },
                0,
                "an empty overlapped read remains pending"
            );
            assert_eq!(unsafe { GetLastError() }, ERROR_IO_PENDING);
            assert_eq!(
                unsafe { CancelIoEx(parent_reader, &raw const overlapped) },
                1
            );
            assert_eq!(
                unsafe { WaitForSingleObject(event.raw(), 5_000) },
                WAIT_OBJECT_0,
                "test cleanup observes completion of the canceled read"
            );
            drop(event);

            let (control_reader, control_writer) = create_pipe();
            let control_input = *b"x";
            let mut control_bytes_written = 0;
            assert_eq!(
                unsafe {
                    WriteFile(
                        control_writer.raw(),
                        control_input.as_ptr(),
                        u32::try_from(control_input.len()).expect("buffer length fits u32"),
                        &raw mut control_bytes_written,
                        core::ptr::null_mut(),
                    )
                },
                1,
                "anonymous control pipe receives its input before reading"
            );
            assert_eq!(control_bytes_written, 1);
            let mut control_overlapped = OVERLAPPED::default();
            let mut control_output = [0_u8; 1];
            let mut control_bytes_read = 0;
            assert_eq!(
                unsafe {
                    ReadFile(
                        control_reader.raw(),
                        control_output.as_mut_ptr(),
                        u32::try_from(control_output.len()).expect("buffer length fits u32"),
                        &raw mut control_bytes_read,
                        &raw mut control_overlapped,
                    )
                },
                1,
                "anonymous control pipe completes immediately with available data"
            );
            assert_eq!(control_output, control_input);
            assert_eq!(control_bytes_read, 1);
            drop(control_reader);
            drop(control_writer);

            pipe_pair.close_child_write_handle();
            assert!(pipe_pair.child_write_handle().is_none());
            drop(pipe_pair);
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!(
                "{{\"named_pipe_pairs\":1,\"overlapped_parent_reads\":1,\"anonymous_control_immediate_reads\":1,\"handles_after_drop\":0}}"
            );
        }

        #[test]
        fn c45a_duplicate_pipe_creation_leaves_no_handle_leaks() {
            let handles_before = current_process_handle_count();
            let pipe_name = unique_pipe_name();
            let pipe_pair = WindowsPipePair::new(&pipe_name)
                .expect("the first instance claims its unique name");
            let handles_with_pair = current_process_handle_count();
            assert_eq!(handles_with_pair, handles_before + 2);

            let duplicate = WindowsPipePair::new(&pipe_name);
            match duplicate {
                Err(WindowsPipePairError::CreateServerFailed { code }) => assert_ne!(code, 0),
                Err(WindowsPipePairError::InvalidName) => {
                    panic!("the duplicate name remains a valid pipe name")
                }
                Err(WindowsPipePairError::OpenChildWriterFailed { code }) => {
                    panic!("duplicate server creation should fail before client open: {code}")
                }
                Err(WindowsPipePairError::SizeOverflow) => {
                    panic!("Win32 structure sizes fit their u32 length fields")
                }
                Ok(_) => panic!("the first-instance control unexpectedly created another pair"),
            }
            assert_eq!(current_process_handle_count(), handles_with_pair);
            drop(pipe_pair);
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!("{{\"duplicate_creation_failures\":1,\"leaked_handles\":0}}");
        }
    }
}

pub mod pool {
    //! Run arena and slot pool: launch data lowered once per run and named-pipe server ends reused across children.

    use core::cell::{Cell, RefCell};
    use core::fmt;
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::boxed::Box;
    use std::os::windows::io::{IntoRawHandle, OwnedHandle};
    use std::rc::Rc;
    use std::string::String;
    use std::vec::Vec;

    use prime::os::readiness::Readiness;
    use windows_sys::Win32::Foundation::{
        ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

    use crate::edge_args_windows::{WINDOWS_CREATE_NO_WINDOW, WindowsArgumentsError, lower_arguments, windows_creation_flags, lower_command_line, merge_environment, write_executable_path, write_working_directory};
    use crate::edge_spawn_windows::handles::{WindowsHandleList, WindowsJob, WindowsOwnedHandle};
    use crate::edge_spawn_windows::pipe_pair::{create_server, open_client};
    use crate::edge_spawn_windows::spawn::{
        ArgumentsError, Child, ChildEnvironment, ChildWorkingDirectory, ExactError, SpawnError, open_null_input,
        resolve_program, spawn_windows_child_suspended, spawn_windows_child_grouped,
    };

    pub const POOL_SLOTS: usize = 64;
    const PATH_UNITS: usize = 32_768;
    const COMMAND_UNITS: usize = 32_768;
    const OVERRIDE_UNITS: usize = 16_384;
    const ATTRIBUTE_WORDS: usize = 64;
    const CONNECT_WAIT_MILLIS: u32 = 10_000;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ERROR_PIPE_BUSY: i32 = 231;
    const ERROR_TOO_MANY_OPEN_FILES: i32 = 4;

    static PIPE_SEQUENCE: AtomicU32 = AtomicU32::new(0);

    /// Where a pooled pipe server end is in its connect cycle.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Phase {
        /// A fresh instance waiting for its first client.
        Listening,
        /// A client is or was connected and EOF has not been read to the end.
        Connected,
        /// EOF was read: the instance can disconnect and listen again with its reactor registration intact.
        Drained,
    }

    /// One overlapped pipe server end plus the reactor gate registered on it, shared by the launching slot, the child and its stream.
    pub struct PooledEnd {
        name: Box<[u16]>,
        handle: WindowsOwnedHandle,
        gate: RefCell<Option<Readiness>>,
        phase: Cell<Phase>,
    }

    /// A shared pipe server end; the slot pool holds one reference and each user of the child holds another.
    pub type PipeEnd = Rc<PooledEnd>;

    impl fmt::Debug for PooledEnd {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_struct("PooledEnd")
                .field("handle", &self.handle)
                .field("phase", &self.phase.get())
                .finish_non_exhaustive()
        }
    }

    impl PooledEnd {
        /// Wraps an already connected server end that no pool will reuse.
        #[must_use]
        pub fn unpooled(handle: OwnedHandle) -> PipeEnd {
            Rc::new(Self {
                name: Box::default(),
                handle: WindowsOwnedHandle::new(handle.into_raw_handle()),
                gate: RefCell::new(None),
                phase: Cell::new(Phase::Connected),
            })
        }

        pub(crate) fn wrap(handle: WindowsOwnedHandle) -> PipeEnd {
            Rc::new(Self {
                name: Box::default(),
                handle,
                gate: RefCell::new(None),
                phase: Cell::new(Phase::Connected),
            })
        }

        fn create() -> Result<PipeEnd, SpawnError> {
            let name = pipe_name();
            let handle = create_server(&name).map_err(|_| SpawnError::last())?;
            Ok(Rc::new(Self {
                name,
                handle,
                gate: RefCell::new(None),
                phase: Cell::new(Phase::Listening),
            }))
        }

        /// The raw overlapped server handle reads are issued on.
        #[must_use]
        pub fn raw(&self) -> HANDLE {
            self.handle.raw()
        }

        pub(crate) fn borrowed(&self) -> std::os::windows::io::BorrowedHandle<'_> {
            self.handle.as_borrowed()
        }

        pub(crate) const fn gate(&self) -> &RefCell<Option<Readiness>> {
            &self.gate
        }

        /// Records that the stream read this end to EOF, so the next child may reuse the instance and its registration.
        pub fn drained(&self) {
            if self.phase.get() == Phase::Connected && !self.name.is_empty() {
                self.phase.set(Phase::Drained);
            }
        }

        /// Where this end is in its connect cycle.
        #[must_use]
        pub fn phase(&self) -> Phase {
            self.phase.get()
        }

        fn connect_client(&self, event: HANDLE) -> Result<WindowsOwnedHandle, SpawnError> {
            let client = match self.phase.get() {
                Phase::Listening => open_client(&self.name).map_err(|_| SpawnError::last())?,
                Phase::Drained => self.relisten_and_open(event)?,
                Phase::Connected => return Err(SpawnError { os_code: ERROR_PIPE_BUSY }),
            };
            self.phase.set(Phase::Connected);
            Ok(client)
        }

        // the low bit of hEvent keeps the connect completion off the completion port the stream gate owns
        fn relisten_and_open(&self, event: HANDLE) -> Result<WindowsOwnedHandle, SpawnError> {
            if unsafe { DisconnectNamedPipe(self.raw()) } == 0 {
                return Err(SpawnError::last());
            }
            unsafe { ResetEvent(event) };
            let mut overlapped = OVERLAPPED {
                hEvent: (event as usize | 1) as HANDLE,
                ..OVERLAPPED::default()
            };
            let connected = unsafe { ConnectNamedPipe(self.raw(), &raw mut overlapped) } != 0;
            let code = unsafe { GetLastError() };
            let waiting = !connected && code == ERROR_IO_PENDING;
            if !connected && !waiting && code != ERROR_PIPE_CONNECTED {
                return Err(SpawnError::from_code(code));
            }
            let opened = open_client(&self.name).map_err(|_| SpawnError::last());
            if waiting {
                if opened.is_err() {
                    unsafe { CancelIoEx(self.raw(), &raw const overlapped) };
                }
                unsafe { WaitForSingleObject(event, CONNECT_WAIT_MILLIS) };
            }
            opened
        }
    }

    fn pipe_name() -> Box<[u16]> {
        let sequence = PIPE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = std::format!("\\\\.\\pipe\\cargo-val-{}-{sequence}-pool", std::process::id());
        name.encode_utf16().chain([0]).collect()
    }

    #[derive(Debug)]
    struct PathCache {
        key: Box<[u8]>,
        key_length: usize,
        units: Box<[u16]>,
        units_length: usize,
    }

    impl PathCache {
        fn new() -> Self {
            Self {
                key: std::vec![0; PATH_UNITS].into_boxed_slice(),
                key_length: 0,
                units: std::vec![0; PATH_UNITS].into_boxed_slice(),
                units_length: 0,
            }
        }

        fn resolve(
            &mut self,
            text: &[u8],
            lower: for<'units> fn(&str, &'units mut [u16]) -> Result<&'units [u16], WindowsArgumentsError>,
        ) -> Result<&[u16], ExactError> {
            if self.key_length > 0 && self.key[..self.key_length] == *text {
                return Ok(&self.units[..self.units_length]);
            }
            let path = core::str::from_utf8(text).map_err(|_| WindowsArgumentsError::NotUnicode)?;
            self.key_length = 0;
            self.units_length = lower(path, &mut self.units)?.len();
            let room = self.key.get_mut(..text.len()).ok_or(ExactError::Arguments(ArgumentsError::StorageTooSmall {
                required: text.len(),
                available: PATH_UNITS,
            }))?;
            room.copy_from_slice(text);
            self.key_length = text.len();
            Ok(&self.units[..self.units_length])
        }
    }

    #[derive(Debug)]
    struct Arena {
        source: Box<[u8]>,
        environment: Box<[u16]>,
        stdin: WindowsOwnedHandle,
        connect_event: WindowsOwnedHandle,
        executable: PathCache,
        directory: PathCache,
    }

    impl Arena {
        fn build(environment: &[&[u8]]) -> Result<Self, ExactError> {
            let mut command = Vec::new();
            let mut block = Vec::new();
            let mut order = Vec::new();
            let lowered = lower_arguments(b"x", &[], environment, &[], &mut command, &mut block, &mut order)?;
            let environment_block = lowered.environment_block().to_vec().into_boxed_slice();
            let event = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
            if event.is_null() || event == INVALID_HANDLE_VALUE {
                return Err(SpawnError::last().into());
            }
            Ok(Self {
                source: join(environment),
                environment: environment_block,
                stdin: open_null_input()?,
                connect_event: WindowsOwnedHandle::new(event),
                executable: PathCache::new(),
                directory: PathCache::new(),
            })
        }

        fn lowered_from(&self, environment: &[&[u8]]) -> bool {
            let mut rest = &self.source[..];
            for entry in environment {
                let Some(tail) = rest.strip_prefix(*entry).and_then(|tail| tail.strip_prefix(&[0])) else {
                    return false;
                };
                rest = tail;
            }
            rest.is_empty()
        }
    }

    fn join(environment: &[&[u8]]) -> Box<[u8]> {
        environment.iter().flat_map(|entry| entry.iter().copied().chain([0])).collect()
    }

    #[derive(Debug)]
    struct Slot {
        stdout: PipeEnd,
        stderr: PipeEnd,
        command_line: Box<[u16]>,
        environment: Box<[u16]>,
        attributes: Box<[MaybeUninit<usize>]>,
    }

    impl Slot {
        fn new(base_units: usize) -> Result<Self, SpawnError> {
            Ok(Self {
                stdout: PooledEnd::create()?,
                stderr: PooledEnd::create()?,
                command_line: std::vec![0; COMMAND_UNITS].into_boxed_slice(),
                environment: std::vec![0; base_units + OVERRIDE_UNITS].into_boxed_slice(),
                attributes: std::vec![MaybeUninit::uninit(); ATTRIBUTE_WORDS].into_boxed_slice(),
            })
        }

        fn free(&self) -> bool {
            Rc::strong_count(&self.stdout) == 1 && Rc::strong_count(&self.stderr) == 1
        }

        fn refresh(&mut self) -> Result<(), SpawnError> {
            for end in [&mut self.stdout, &mut self.stderr] {
                if end.phase() == Phase::Connected {
                    *end = PooledEnd::create()?;
                }
            }
            Ok(())
        }
    }

    /// The run's launch arena and slot pool: nothing here is rebuilt for a launch once its slot has run a child.
    #[derive(Debug)]
    pub struct LaunchStorage {
        arena: Option<Arena>,
        slots: Vec<Slot>,
        creation_flags: u32,
    }

    impl Default for LaunchStorage {
        fn default() -> Self {
            Self::new()
        }
    }

    /// How a launched child is started.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Start {
        /// The child runs as soon as it exists.
        Now,
        /// The child exists in the run job but does not run until [`Child::resume`].
        Ready,
    }

    impl LaunchStorage {
        #[must_use]
        pub fn new() -> Self {
            Self {
                arena: None,
                slots: Vec::with_capacity(POOL_SLOTS),
                creation_flags: windows_creation_flags(),
            }
        }

        /// Creates every child with `CREATE_NO_WINDOW`, which gives each console child its own hidden console host; only the launch-cost measurement wants this.
        #[must_use]
        pub const fn with_hidden_console(mut self) -> Self {
            self.creation_flags |= WINDOWS_CREATE_NO_WINDOW;
            self
        }

        /// Launches a child from this storage inside `job`, reusing the arena and a free slot.
        ///
        /// # Errors
        ///
        /// Returns [`ExactError`] when launch data does not fit a slot or Windows refuses a step.
        pub fn launch<'bytes>(
            &mut self,
            program: &'bytes [u8],
            arguments: impl Iterator<Item = &'bytes [u8]> + Clone,
            (environment, overrides, directory): (&[&[u8]], &[&[u8]], &[u8]),
            (job, start): (&WindowsJob, Start),
        ) -> Result<Child, ExactError> {
            let Self { arena, slots, creation_flags } = self;
            if !arena.as_ref().is_some_and(|arena| arena.lowered_from(environment)) {
                *arena = Some(Arena::build(environment)?);
            }
            let arena = arena.as_mut().ok_or(SpawnError { os_code: ERROR_TOO_MANY_OPEN_FILES })?;
            let slot = acquire(slots, arena.environment.len())?;
            slot.refresh()?;
            if slot.environment.len() < arena.environment.len() + OVERRIDE_UNITS {
                slot.environment = std::vec![0; arena.environment.len() + OVERRIDE_UNITS].into_boxed_slice();
            }
            let resolved: String;
            let executable_text = if program.iter().any(|byte| matches!(byte, b'\\' | b'/' | b':')) {
                program
            } else {
                resolved = resolve_program(program, environment)?;
                resolved.as_bytes()
            };
            let executable = arena.executable.resolve(executable_text, write_executable_path)?;
            let directory = arena.directory.resolve(directory, write_working_directory)?;
            let command_line = lower_command_line(core::iter::once(program).chain(arguments), &mut slot.command_line)?;
            let environment_block: &[u16] = if overrides.is_empty() {
                &arena.environment
            } else {
                merge_environment(&arena.environment, overrides, &mut slot.environment)?
            };
            let event = arena.connect_event.raw();
            let stdout_client = slot.stdout.connect_client(event)?;
            let stderr_client = slot.stderr.connect_client(event)?;
            let handles = [arena.stdin.raw(), stdout_client.raw(), stderr_client.raw()];
            let handle_list = WindowsHandleList::new_in_job(&handles, job, &mut slot.attributes)
                .map_err(|_| SpawnError { os_code: ERROR_INVALID_PARAMETER })?;
            let environment_block = ChildEnvironment::Block(environment_block);
            let directory = ChildWorkingDirectory::Borrowed(directory);
            let process = match start {
                Start::Now => spawn_windows_child_grouped(
                    executable,
                    command_line,
                    &handle_list,
                    &environment_block,
                    &directory,
                    (None, *creation_flags),
                ),
                Start::Ready => spawn_windows_child_suspended(
                    executable,
                    command_line,
                    &handle_list,
                    &environment_block,
                    &directory,
                    *creation_flags,
                ),
            }?;
            drop(handle_list);
            Ok(Child::from_pooled(process, Rc::clone(&slot.stdout), Rc::clone(&slot.stderr)))
        }
    }

    fn acquire(slots: &mut Vec<Slot>, base_units: usize) -> Result<&mut Slot, SpawnError> {
        let index = match slots.iter().position(Slot::free) {
            Some(index) => index,
            None if slots.len() < POOL_SLOTS => {
                slots.push(Slot::new(base_units)?);
                slots.len() - 1
            }
            None => return Err(SpawnError { os_code: ERROR_TOO_MANY_OPEN_FILES }),
        };
        Ok(&mut slots[index])
    }

    #[cfg(test)]
    mod tests {
        use std::env;
        use std::string::String;
        use std::sync::Mutex;
        use std::vec::Vec;

        use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_IO_PENDING, GetLastError, HANDLE, WAIT_OBJECT_0};
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        use windows_sys::Win32::System::Threading::{
            CreateEventW, GetCurrentProcess, GetExitCodeProcess, GetProcessHandleCount, WaitForSingleObject,
        };

        use super::{LaunchStorage, PipeEnd, Start};
        use crate::edge_spawn_windows::handles::{WindowsJob, WindowsOwnedHandle};
        use crate::edge_spawn_windows::{Child, spawn_exact};
        use crate::test_alloc::measure_allocations;

        static HANDLE_COUNT_GUARD: Mutex<()> = Mutex::new(());

        const ALPHA_MARKER: &str = "edge-pool-marker-alpha";
        const BETA_MARKER: &str = "edge-pool-marker-beta";
        const TERMINATED: u32 = 0x0000_1234;

        fn edge_pool_probe_alpha() {
            if env::args().any(|argument| argument == "--exact") {
                std::println!("{ALPHA_MARKER}");
            }
        }

        fn edge_pool_probe_beta() {
            if env::args().any(|argument| argument == "--exact") {
                std::println!("{BETA_MARKER}");
            }
        }

        #[test]
        fn edge_pool_probe_alpha_child_entry() {
            edge_pool_probe_alpha();
        }

        #[test]
        fn edge_pool_probe_beta_child_entry() {
            edge_pool_probe_beta();
        }

        struct Fixture {
            executable: String,
            environment: Vec<Vec<u8>>,
            directory: Vec<u8>,
        }

        impl Fixture {
            fn new() -> Self {
                let executable = env::current_exe().expect("test executable path exists");
                let directory = env::current_dir().expect("working directory exists");
                Self {
                    executable: executable.to_str().expect("executable path is UTF-8").into(),
                    environment: env::vars().map(|(key, value)| std::format!("{key}={value}").into_bytes()).collect(),
                    directory: directory.to_str().expect("directory is UTF-8").as_bytes().to_vec(),
                }
            }

            fn references(&self) -> Vec<&[u8]> {
                self.environment.iter().map(Vec::as_slice).collect()
            }
        }

        fn alpha_name() -> String {
            let _: fn() = edge_pool_probe_alpha_child_entry;
            crate::test_probe::probe_path(module_path!(), "edge_pool_probe_alpha_child_entry")
        }

        fn beta_name() -> String {
            let _: fn() = edge_pool_probe_beta_child_entry;
            crate::test_probe::probe_path(module_path!(), "edge_pool_probe_beta_child_entry")
        }

        fn launch(fixture: &Fixture, storage: &mut LaunchStorage, probe: &str) -> Child {
            let references = fixture.references();
            spawn_exact(
                storage,
                fixture.executable.as_bytes(),
                probe.as_bytes(),
                &[b"--nocapture"],
                &references,
                &[],
                &fixture.directory,
            )
            .expect("the pooled launch starts the exact probe child")
        }

        fn drain(end: &PipeEnd) -> Vec<u8> {
            let event = unsafe { CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()) };
            let event = WindowsOwnedHandle::new(event);
            let mut collected = Vec::new();
            loop {
                let mut buffer = [0_u8; 4096];
                let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
                let mut transferred = 0;
                let immediate = unsafe {
                    ReadFile(end.raw(), buffer.as_mut_ptr(), 4096, &raw mut transferred, &raw mut overlapped)
                };
                let finished = if immediate != 0 {
                    true
                } else {
                    match unsafe { GetLastError() } {
                        ERROR_BROKEN_PIPE => false,
                        ERROR_IO_PENDING => {
                            let waited = unsafe {
                                GetOverlappedResult(end.raw(), &raw const overlapped, &raw mut transferred, 1)
                            };
                            if waited == 0 {
                                assert_eq!(unsafe { GetLastError() }, ERROR_BROKEN_PIPE, "the only read failure is the child closing");
                            }
                            waited != 0
                        }
                        other => panic!("read failed with {other}, expected pending or broken pipe"),
                    }
                };
                if !finished {
                    break;
                }
                collected.extend_from_slice(&buffer[..transferred as usize]);
            }
            end.drained();
            collected
        }

        fn wait_for_exit(child: &Child) -> u32 {
            assert_eq!(unsafe { WaitForSingleObject(child.raw_process_handle(), 30_000) }, WAIT_OBJECT_0);
            let mut code = 0;
            assert_ne!(unsafe { GetExitCodeProcess(child.raw_process_handle(), &raw mut code) }, 0);
            code
        }

        fn run_to_completion(child: Child) -> (Vec<u8>, HANDLE) {
            let (_, stdout, stderr) = child.into_parts();
            let output = drain(&stdout);
            drain(&stderr);
            (output, stdout.raw())
        }

        fn process_handle_count() -> u32 {
            let mut count = 0;
            assert_ne!(unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut count) }, 0);
            count
        }

        #[test]
        fn edge_exact_launch_after_setup_performs_zero_heap_allocations() {
            let fixture = Fixture::new();
            let references = fixture.references();
            let mut storage = LaunchStorage::new();
            let alpha = alpha_name();
            let warm = launch(&fixture, &mut storage, &alpha);
            assert_eq!(wait_for_exit(&warm), 0);
            run_to_completion(warm);

            let (child, allocations) = measure_allocations(|| {
                spawn_exact(
                    &mut storage,
                    fixture.executable.as_bytes(),
                    alpha.as_bytes(),
                    &[b"--nocapture"],
                    &references,
                    &[],
                    &fixture.directory,
                )
            });
            let child = child.expect("the measured launch starts");

            assert_eq!(allocations, 0, "a launch through a warmed slot allocates nothing");
            assert_eq!(wait_for_exit(&child), 0);
            let (output, _) = run_to_completion(child);
            assert!(String::from_utf8_lossy(&output).contains(ALPHA_MARKER));
        }

        #[test]
        fn edge_process_handle_count_is_stable_across_200_sequential_slot_launches() {
            let _serial = HANDLE_COUNT_GUARD.lock().expect("handle count guard is not poisoned");
            let fixture = Fixture::new();
            let mut storage = LaunchStorage::new();
            let alpha = alpha_name();
            let warm = launch(&fixture, &mut storage, &alpha);
            wait_for_exit(&warm);
            run_to_completion(warm);
            let baseline = process_handle_count();

            let mut launches = 0;
            for _ in 0..200 {
                let child = launch(&fixture, &mut storage, &alpha);
                assert_eq!(wait_for_exit(&child), 0);
                let (output, _) = run_to_completion(child);
                assert!(String::from_utf8_lossy(&output).contains(ALPHA_MARKER));
                launches += 1;
            }

            assert_eq!(launches, 200);
            assert_eq!(process_handle_count(), baseline);
            assert_eq!(storage.slots.len(), 1, "sequential launches share one slot");
        }

        #[test]
        fn edge_reused_slot_delivers_only_the_second_childs_stdout() {
            let fixture = Fixture::new();
            let mut storage = LaunchStorage::new();
            let first = launch(&fixture, &mut storage, &alpha_name());
            assert_eq!(wait_for_exit(&first), 0);
            let (first_output, first_handle) = run_to_completion(first);

            let second = launch(&fixture, &mut storage, &beta_name());
            assert_eq!(wait_for_exit(&second), 0);
            let (second_output, second_handle) = run_to_completion(second);

            assert_eq!(first_handle, second_handle, "the second child reuses the first child's server end");
            assert!(String::from_utf8_lossy(&first_output).contains(ALPHA_MARKER));
            let second_text = String::from_utf8_lossy(&second_output);
            assert!(second_text.contains(BETA_MARKER));
            assert!(!second_text.contains(ALPHA_MARKER), "no byte of the first child reaches the second: {second_text}");
        }

        #[test]
        fn edge_ready_child_runs_only_after_resume() {
            let fixture = Fixture::new();
            let references = fixture.references();
            let mut storage = LaunchStorage::new();
            let job = WindowsJob::new().expect("a job object is created");
            let beta = beta_name();
            let child = storage
                .launch(
                    fixture.executable.as_bytes(),
                    [b"--exact".as_slice(), beta.as_bytes(), b"--nocapture"].into_iter(),
                    (&references, &[], &fixture.directory),
                    (&job, Start::Ready),
                )
                .expect("the ready child is created suspended");

            child.resume();

            assert_eq!(wait_for_exit(&child), 0);
            let (output, _) = run_to_completion(child);
            assert!(String::from_utf8_lossy(&output).contains(BETA_MARKER));
        }

        #[test]
        fn edge_ready_child_is_terminated_with_the_run_job_and_never_ran() {
            let fixture = Fixture::new();
            let references = fixture.references();
            let mut storage = LaunchStorage::new();
            let job = WindowsJob::new().expect("a job object is created");
            let alpha = alpha_name();
            let child = storage
                .launch(
                    fixture.executable.as_bytes(),
                    [b"--exact".as_slice(), alpha.as_bytes(), b"--nocapture"].into_iter(),
                    (&references, &[], &fixture.directory),
                    (&job, Start::Ready),
                )
                .expect("the ready child is created suspended");
            let mut code = 0;
            assert_ne!(unsafe { GetExitCodeProcess(child.raw_process_handle(), &raw mut code) }, 0);
            assert_eq!(code, 259, "a ready child is alive but frozen");

            assert_ne!(unsafe { TerminateJobObject(job.raw(), TERMINATED) }, 0);

            assert_eq!(wait_for_exit(&child), TERMINATED, "the job ended the frozen child before it could exit on its own");
            let (output, _) = run_to_completion(child);
            assert!(output.is_empty(), "a frozen child wrote nothing");
        }
    }
}

mod spawn {
    //! Nonblocking ownership of Windows child process handles.

    use core::ffi::c_void;
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicU32, Ordering};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::string::String;
    use std::sync::OnceLock;
    use std::vec::Vec;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, TerminateJobObject};
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessW, PROCESS_INFORMATION, ResumeThread, SetPriorityClass,
        TerminateProcess,
    };

    use crate::ExitSource;
    use crate::edge_args_windows::WindowsArguments;
    use crate::edge_args_windows::{
        WINDOWS_BELOW_NORMAL_PRIORITY, WindowsArgumentsError, lower_arguments, windows_creation_flags,
        write_executable_path, write_working_directory,
    };
    use crate::edge_signal_windows::register_group;
    use crate::edge_spawn_windows::handles::{
        WindowsHandleList, WindowsJob, WindowsJobError, WindowsOwnedHandle,
    };
    use crate::edge_spawn_windows::pipe_pair::WindowsPipePair;
    pub use crate::edge_spawn_windows::pool::{LaunchStorage, PipeEnd, PooledEnd, Start};

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const ATTRIBUTE_WORDS: usize = 64;
    const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

    /// Failures while creating a Windows child process.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowsSpawnError {
        /// `CreateProcessW` rejected the requested executable or launch data.
        CreateFailed { code: u32 },
        /// `CreateProcessW` returned an invalid process or thread handle.
        InvalidProcessInformation,
        /// The child working directory is not a valid NUL-terminated UTF-16 path.
        InvalidWorkingDirectory,
        /// The executable is not a single NUL-terminated UTF-16 path.
        InvalidExecutable,
    }

    /// A launched child whose process handle remains owned until drop.
    #[derive(Debug)]
    pub struct WindowsChild {
        process: WindowsOwnedHandle,
        thread: Option<WindowsOwnedHandle>,
        process_id: u32,
    }

    pub(super) enum ChildEnvironment<'environment> {
        Inherit,
        Block(&'environment [u16]),
    }

    pub(super) enum ChildWorkingDirectory<'directory> {
        Inherit,
        Borrowed(&'directory [u16]),
    }

    impl WindowsChild {
        /// Returns the process identifier assigned by Windows.
        #[must_use]
        pub const fn process_id(&self) -> u32 {
            self.process_id
        }

        /// Returns the borrowed handle used by the Windows readiness adapter.
        #[must_use]
        pub const fn raw_process_handle(&self) -> HANDLE {
            self.process.raw()
        }
    }

    /// Starts one exact-test child and returns without waiting for its completion.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsSpawnError`] when Windows rejects process creation.
    pub fn spawn_windows_child(
        executable: &[u16],
        arguments: &mut WindowsArguments<'_>,
        handle_list: &WindowsHandleList<'_>,
    ) -> Result<WindowsChild, WindowsSpawnError> {
        let command_line = arguments.command_line();
        spawn_windows_child_inner(
            executable,
            command_line,
            handle_list,
            &ChildEnvironment::Inherit,
            &ChildWorkingDirectory::Inherit,
        )
    }

    /// Starts one child with the caller's complete UTF-16 environment block.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsSpawnError`] when Windows rejects process creation.
    pub fn spawn_windows_child_with_environment(
        executable: &[u16],
        arguments: &mut WindowsArguments<'_>,
        handle_list: &WindowsHandleList<'_>,
    ) -> Result<WindowsChild, WindowsSpawnError> {
        let (command_line, environment_block) = arguments.create_process_buffers();
        spawn_windows_child_inner(
            executable,
            command_line,
            handle_list,
            &ChildEnvironment::Block(environment_block),
            &ChildWorkingDirectory::Inherit,
        )
    }

    /// Starts one child with caller-owned UTF-16 environment and working-directory data.
    ///
    /// # Errors
    ///
    /// Returns [`WindowsSpawnError::InvalidWorkingDirectory`] for a malformed path, or another [`WindowsSpawnError`] when Windows rejects process creation.
    pub fn spawn_windows_child_with_environment_and_directory(
        executable: &[u16],
        arguments: &mut WindowsArguments<'_>,
        handle_list: &WindowsHandleList<'_>,
        working_directory: &[u16],
    ) -> Result<WindowsChild, WindowsSpawnError> {
        let (command_line, environment_block) = arguments.create_process_buffers();
        spawn_windows_child_inner(
            executable,
            command_line,
            handle_list,
            &ChildEnvironment::Block(environment_block),
            &ChildWorkingDirectory::Borrowed(working_directory),
        )
    }

    fn spawn_windows_child_inner(
        executable: &[u16],
        command_line: &mut [u16],
        handle_list: &WindowsHandleList<'_>,
        environment: &ChildEnvironment<'_>,
        working_directory: &ChildWorkingDirectory<'_>,
    ) -> Result<WindowsChild, WindowsSpawnError> {
        spawn_windows_child_grouped(
            executable,
            command_line,
            handle_list,
            environment,
            working_directory,
            (None, windows_creation_flags()),
        )
    }

    /// Creates the child suspended when `group` is given, joins it to that nested job, then resumes it, so no grandchild escapes the group.
    pub(super) fn spawn_windows_child_grouped(
        executable: &[u16],
        command_line: &mut [u16],
        handle_list: &WindowsHandleList<'_>,
        environment: &ChildEnvironment<'_>,
        working_directory: &ChildWorkingDirectory<'_>,
        (group, creation_flags): (Option<&WindowsJob>, u32),
    ) -> Result<WindowsChild, WindowsSpawnError> {
        let child = create_process(
            executable,
            command_line,
            handle_list,
            (environment, working_directory),
            (creation_flags, group.is_some()),
        )?;
        if let (Some(group), Some(thread)) = (group, child.thread.as_ref()) {
            join_group_and_resume(&child.process, thread, group)?;
        }
        Ok(WindowsChild { thread: None, ..child })
    }

    /// Creates the child suspended inside the job its attribute list names; it stays frozen until [`Child::resume`].
    pub(super) fn spawn_windows_child_suspended(
        executable: &[u16],
        command_line: &mut [u16],
        handle_list: &WindowsHandleList<'_>,
        environment: &ChildEnvironment<'_>,
        working_directory: &ChildWorkingDirectory<'_>,
        creation_flags: u32,
    ) -> Result<WindowsChild, WindowsSpawnError> {
        create_process(executable, command_line, handle_list, (environment, working_directory), (creation_flags, true))
    }

    fn create_process(
        executable: &[u16],
        command_line: &mut [u16],
        handle_list: &WindowsHandleList<'_>,
        (environment, working_directory): (&ChildEnvironment<'_>, &ChildWorkingDirectory<'_>),
        (creation_flags, suspended): (u32, bool),
    ) -> Result<WindowsChild, WindowsSpawnError> {
        let startup_info = core::ptr::from_ref(handle_list.startup_info()).cast();
        let mut process_information = PROCESS_INFORMATION::default();
        if !is_terminated_path(executable) {
            return Err(WindowsSpawnError::InvalidExecutable);
        }
        let environment_pointer = match environment {
            ChildEnvironment::Inherit => core::ptr::null(),
            ChildEnvironment::Block(environment_block) => {
                environment_block.as_ptr().cast::<c_void>()
            }
        };
        let working_directory_pointer = match working_directory {
            ChildWorkingDirectory::Inherit => core::ptr::null(),
            ChildWorkingDirectory::Borrowed(path) => {
                if !is_terminated_path(path) {
                    return Err(WindowsSpawnError::InvalidWorkingDirectory);
                }
                path.as_ptr()
            }
        };
        let created = unsafe {
            CreateProcessW(
                executable.as_ptr(),
                command_line.as_mut_ptr(),
                core::ptr::null(),
                core::ptr::null(),
                1,
                creation_flags | if suspended { CREATE_SUSPENDED } else { 0 },
                environment_pointer,
                working_directory_pointer,
                startup_info,
                &raw mut process_information,
            )
        };
        if created == 0 {
            return Err(WindowsSpawnError::CreateFailed {
                code: unsafe { GetLastError() },
            });
        }
        if !valid_handle(process_information.hProcess) || !valid_handle(process_information.hThread)
        {
            close_if_valid(process_information.hThread);
            close_if_valid(process_information.hProcess);
            return Err(WindowsSpawnError::InvalidProcessInformation);
        }
        Ok(WindowsChild {
            process: WindowsOwnedHandle::new(process_information.hProcess),
            thread: Some(WindowsOwnedHandle::new(process_information.hThread)),
            process_id: process_information.dwProcessId,
        })
    }

    fn join_group_and_resume(
        process: &WindowsOwnedHandle,
        thread: &WindowsOwnedHandle,
        group: &WindowsJob,
    ) -> Result<(), WindowsSpawnError> {
        let joined = unsafe { AssignProcessToJobObject(group.raw(), process.raw()) } != 0;
        let resumed = joined && unsafe { ResumeThread(thread.raw()) } != u32::MAX;
        if resumed {
            return Ok(());
        }
        let code = unsafe { GetLastError() };
        unsafe { TerminateProcess(process.raw(), 1) };
        Err(WindowsSpawnError::CreateFailed { code })
    }

    fn is_terminated_path(path: &[u16]) -> bool {
        path.len() >= 2 && path.last() == Some(&0) && !path[..path.len() - 1].contains(&0)
    }
    fn valid_handle(handle: HANDLE) -> bool {
        !handle.is_null() && handle != INVALID_HANDLE_VALUE
    }

    fn close_if_valid(handle: HANDLE) {
        if valid_handle(handle) {
            unsafe { CloseHandle(handle) };
        }
    }

    /// A child process whose captured output remains owned by the host driver.
    #[derive(Debug)]
    pub struct Child {
        process: WindowsChild,
        stdout: PipeEnd,
        stderr: PipeEnd,
    }

    /// A spawn failure: the raw Win32 error code of the operation that refused.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SpawnError {
        /// The `GetLastError` value, or `ERROR_INVALID_PARAMETER` for launch data Windows cannot hold.
        pub os_code: i32,
    }

    impl From<WindowsArgumentsError> for SpawnError {
        fn from(_: WindowsArgumentsError) -> Self {
            Self {
                os_code: ERROR_INVALID_PARAMETER,
            }
        }
    }

    impl From<WindowsSpawnError> for SpawnError {
        fn from(error: WindowsSpawnError) -> Self {
            match error {
                WindowsSpawnError::CreateFailed { code } => Self::from_code(code),
                _ => Self {
                    os_code: ERROR_INVALID_PARAMETER,
                },
            }
        }
    }

    impl SpawnError {
        pub(super) fn from_code(code: u32) -> Self {
            Self {
                os_code: code.cast_signed(),
            }
        }

        pub(super) fn last() -> Self {
            Self::from_code(unsafe { GetLastError() })
        }
    }

    impl Child {
        /// Returns the process identifier watched by the host event source.
        #[must_use]
        pub const fn process_id(&self) -> u32 {
            self.process.process_id()
        }

        /// Transfers the two overlapped capture handles to completion-driven host readers.
        #[must_use]
        pub fn into_parts(self) -> (u32, PipeEnd, PipeEnd) {
            (self.process.process_id(), self.stdout, self.stderr)
        }

        /// Shares both capture ends with a reader while this child keeps its own references.
        ///
        /// # Errors
        ///
        /// Never fails on Windows; the signature matches the platforms that duplicate descriptors.
        pub fn duplicate_streams(&self) -> io::Result<(PipeEnd, PipeEnd)> {
            Ok((Rc::clone(&self.stdout), Rc::clone(&self.stderr)))
        }

        /// The borrowed process handle, for waiting on exit.
        #[must_use]
        pub fn raw_process_handle(&self) -> HANDLE {
            self.process.raw_process_handle()
        }

        pub(super) const fn from_pooled(process: WindowsChild, stdout: PipeEnd, stderr: PipeEnd) -> Self {
            Self { process, stdout, stderr }
        }

        /// Lets a child created ready start running; a child already running is unaffected.
        pub fn resume(&self) {
            if let Some(thread) = &self.process.thread {
                unsafe { ResumeThread(thread.raw()) };
            }
        }

        /// The source that reports this child's exit.
        ///
        /// # Errors
        ///
        /// Returns the `DuplicateHandle` error when the process handle cannot be duplicated.
        pub fn exit_source(&self) -> io::Result<ExitSource> {
            ExitSource::new(self.process.process.as_borrowed())
        }

        /// Kills the child outright; a child already gone is not an error.
        pub fn terminate(&self) {
            unsafe { TerminateProcess(self.process.process.raw(), 1) };
        }

        /// Lowers the child's priority class so doc tests yield cores to the build.
        ///
        /// # Errors
        ///
        /// Returns the `SetPriorityClass` code when Windows refuses.
        pub fn lower_priority(&self) -> Result<(), SpawnError> {
            if unsafe { SetPriorityClass(self.process.process.raw(), WINDOWS_BELOW_NORMAL_PRIORITY) } == 0 {
                return Err(SpawnError::last());
            }
            Ok(())
        }
    }

    /// Spawns `program` with `arguments`, the inherited `environment` overlaid by `overrides`, and captures both streams.
    ///
    /// A `program` without a path separator is searched on `PATH`, as `.exe`. `directory` becomes the child's working
    /// directory. The child joins the run's job, so it dies with this process.
    ///
    /// # Errors
    ///
    /// Returns the Win32 code of the failing step, or `ERROR_INVALID_PARAMETER` for launch data that is not Unicode text.
    pub fn spawn(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
    ) -> Result<Child, SpawnError> {
        launch(program, arguments, environment, overrides, directory, false)
    }

    /// Spawns like [`spawn`] but inside a nested job of its own, so one termination reaches the child and everything it spawns.
    ///
    /// # Errors
    ///
    /// Returns the Win32 code of the failing step, or `ERROR_INVALID_PARAMETER` for launch data that is not Unicode text.
    pub fn spawn_in_group(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
    ) -> Result<Child, SpawnError> {
        launch(program, arguments, environment, overrides, directory, true)
    }

    /// Launch data that does not fit the pooled slot storage.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ArgumentsError {
        /// A path, command line or environment block outgrew its fixed buffer.
        StorageTooSmall { required: usize, available: usize },
    }

    impl ArgumentsError {
        /// The refusal as terse text plus, for storage refusals, the required and available sizes.
        #[must_use]
        pub const fn describe(self) -> (&'static [u8], Option<(usize, usize)>) {
            match self {
                Self::StorageTooSmall { required, available } => {
                    (b"pooled launch storage units need", Some((required, available)))
                }
            }
        }
    }

    impl From<WindowsArgumentsError> for ExactError {
        fn from(error: WindowsArgumentsError) -> Self {
            match error {
                WindowsArgumentsError::CommandLineStorageTooSmall { required, available }
                | WindowsArgumentsError::EnvironmentStorageTooSmall { required, available }
                | WindowsArgumentsError::EnvironmentOrderStorageTooSmall { required, available } => {
                    Self::Arguments(ArgumentsError::StorageTooSmall { required, available })
                }
                other => Self::Spawn(other.into()),
            }
        }
    }

    impl From<SpawnError> for ExactError {
        fn from(error: SpawnError) -> Self {
            Self::Spawn(error)
        }
    }

    impl From<WindowsSpawnError> for ExactError {
        fn from(error: WindowsSpawnError) -> Self {
            Self::Spawn(error.into())
        }
    }

    pub type HeldLaunchStorage = LaunchStorage;

    #[must_use]
    pub fn held_launch_storage() -> HeldLaunchStorage {
        LaunchStorage::new()
    }

    /// Why an exact test launch was refused.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ExactError {
        /// Never produced on Windows.
        Arguments(ArgumentsError),
        /// The Win32 operation that refused.
        Spawn(SpawnError),
    }

    /// Spawns one exact test child as `program --exact test_name` followed by `harness_arguments`.
    ///
    /// # Errors
    ///
    /// Returns [`ExactError::Spawn`] with the Win32 code of the failing step.
    pub fn spawn_exact(
        storage: &mut LaunchStorage,
        program: &[u8],
        test_name: &[u8],
        harness_arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        exact(storage, (program, test_name, harness_arguments), (environment, overrides, directory), Start::Now)
    }

    /// Creates one exact test child frozen inside the run job; [`Child::resume`] starts it, and an interrupt ends it with the job.
    ///
    /// # Errors
    ///
    /// Returns [`ExactError`] like [`spawn_exact`].
    pub fn spawn_exact_ready(
        storage: &mut LaunchStorage,
        program: &[u8],
        test_name: &[u8],
        harness_arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        exact(storage, (program, test_name, harness_arguments), (environment, overrides, directory), Start::Ready)
    }

    /// How many children beyond the job limit may wait created and frozen, so process creation overlaps a running child.
    pub const READY_AHEAD: usize = 1;

    /// Starts a child created by [`spawn_exact_ready`].
    pub fn resume_ready(child: &Child) {
        child.resume();
    }

    fn exact(
        storage: &mut LaunchStorage,
        (program, test_name, harness_arguments): (&[u8], &[u8], &[&[u8]]),
        context: (&[&[u8]], &[&[u8]], &[u8]),
        start: Start,
    ) -> Result<Child, ExactError> {
        let arguments = [b"--exact".as_slice(), test_name]
            .into_iter()
            .chain(harness_arguments.iter().copied());
        storage.launch(program, arguments, context, (run_job()?, start))
    }

    /// Spawns one listing child as `program --list`, or `program --list --ignored`.
    ///
    /// # Errors
    ///
    /// Returns [`ExactError::Spawn`] with the Win32 code of the failing step.
    pub fn spawn_listed(
        _storage: &mut LaunchStorage,
        program: &[u8],
        ignored: bool,
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: &[u8],
    ) -> Result<Child, ExactError> {
        let arguments: &[&[u8]] = if ignored { &[b"--list", b"--ignored"] } else { &[b"--list"] };
        spawn(program, arguments, environment, overrides, Some(directory)).map_err(ExactError::Spawn)
    }

    /// Ends every child of this run, as an interrupt does; children share the console, so the job is what guarantees each one ends.
    pub(crate) fn terminate_children() {
        if let Some(Ok(job)) = RUN_JOB.get() {
            unsafe { TerminateJobObject(job.raw(), STATUS_CONTROL_C_EXIT) };
        }
    }

    static RUN_JOB: OnceLock<Result<WindowsJob, SpawnError>> = OnceLock::new();
    static PIPE_SEQUENCE: AtomicU32 = AtomicU32::new(0);

    fn run_job() -> Result<&'static WindowsJob, SpawnError> {
        RUN_JOB
            .get_or_init(|| {
                WindowsJob::new().map_err(job_error)
            })
            .as_ref()
            .map_err(|error| *error)
    }

    fn launch(
        program: &[u8],
        arguments: &[&[u8]],
        environment: &[&[u8]],
        overrides: &[&[u8]],
        directory: Option<&[u8]>,
        in_group: bool,
    ) -> Result<Child, SpawnError> {
        let executable = resolve_program(program, environment)?;
        let mut executable_units = alloc_units(&executable);
        let executable_path = write_executable_path(&executable, &mut executable_units)?;
        let directory_text = directory
            .map(core::str::from_utf8)
            .transpose()
            .map_err(|_| SpawnError {
                os_code: ERROR_INVALID_PARAMETER,
            })?;
        let mut directory_units = directory_text.map(alloc_units);
        let working_directory = match (directory_text, directory_units.as_mut()) {
            (Some(text), Some(units)) => Some(write_working_directory(text, units)?),
            _ => None,
        };
        let (mut command_line, mut environment_block, mut environment_order) =
            (Vec::new(), Vec::new(), Vec::new());
        let mut lowered = lower_arguments(
            program,
            arguments,
            environment,
            overrides,
            &mut command_line,
            &mut environment_block,
            &mut environment_order,
        )?;
        let (stdout_pipe, stderr_pipe, stdin) = open_stdio()?;
        let handles = [
            stdin.raw(),
            stdout_pipe.child_write_handle().ok_or_else(SpawnError::last)?,
            stderr_pipe.child_write_handle().ok_or_else(SpawnError::last)?,
        ];
        let mut storage = [MaybeUninit::<usize>::uninit(); ATTRIBUTE_WORDS];
        let handle_list = WindowsHandleList::new_in_job(&handles, run_job()?, &mut storage)
            .map_err(|_| SpawnError {
                os_code: ERROR_INVALID_PARAMETER,
            })?;
        let (command_line, environment_block) = lowered.create_process_buffers();
        let group = in_group.then(WindowsJob::new).transpose().map_err(job_error)?;
        let process = spawn_windows_child_grouped(
            executable_path,
            command_line,
            &handle_list,
            &ChildEnvironment::Block(environment_block),
            &working_directory.map_or(ChildWorkingDirectory::Inherit, ChildWorkingDirectory::Borrowed),
            (group.as_ref(), windows_creation_flags()),
        )?;
        if let Some(group) = group {
            register_group(process.process_id(), group.into_raw());
        }
        drop(handle_list);
        let (mut stdout_pipe, mut stderr_pipe) = (stdout_pipe, stderr_pipe);
        stdout_pipe.close_child_write_handle();
        stderr_pipe.close_child_write_handle();
        Ok(Child {
            process,
            stdout: PooledEnd::wrap(stdout_pipe.into_parent_reader()),
            stderr: PooledEnd::wrap(stderr_pipe.into_parent_reader()),
        })
    }

    fn job_error(error: WindowsJobError) -> SpawnError {
        match error {
            WindowsJobError::CreateFailed { code } | WindowsJobError::LimitFailed { code } => {
                SpawnError::from_code(code)
            }
            WindowsJobError::SizeOverflow => SpawnError {
                os_code: ERROR_INVALID_PARAMETER,
            },
        }
    }

    fn alloc_units(text: &str) -> Vec<u16> {
        std::vec![0; text.encode_utf16().count() + 1]
    }

    fn open_stdio() -> Result<(WindowsPipePair, WindowsPipePair, WindowsOwnedHandle), SpawnError> {
        let sequence = PIPE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let process_id = std::process::id();
        let pair = |stream: &str| {
            let name = std::format!("\\\\.\\pipe\\cargo-val-{process_id}-{sequence}-{stream}");
            let units: Vec<u16> = name.encode_utf16().chain([0]).collect();
            WindowsPipePair::new(&units).map_err(|_| SpawnError::last())
        };
        let stdout = pair("stdout")?;
        let stderr = pair("stderr")?;
        Ok((stdout, stderr, open_null_input()?))
    }

    pub(super) fn open_null_input() -> Result<WindowsOwnedHandle, SpawnError> {
        let security = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(core::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: core::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let name: Vec<u16> = "NUL".encode_utf16().chain([0]).collect();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &raw const security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(SpawnError::last());
        }
        Ok(WindowsOwnedHandle::new(handle))
    }

    /// An absolute path for `program`: itself when it names a path, else the first `PATH` match, which `CreateProcessW` would not search for an application name.
    pub(super) fn resolve_program(program: &[u8], environment: &[&[u8]]) -> Result<String, SpawnError> {
        let name = core::str::from_utf8(program).map_err(|_| SpawnError {
            os_code: ERROR_INVALID_PARAMETER,
        })?;
        if name.contains(['\\', '/', ':']) {
            return Ok(String::from(name));
        }
        let search = search_path(environment);
        std::env::split_paths(&search)
            .flat_map(|directory| candidates(&directory, name))
            .find(|candidate| candidate.is_file())
            .and_then(|found| std::path::absolute(found).ok())
            .and_then(|found| found.to_str().map(String::from))
            .ok_or(SpawnError {
                os_code: ERROR_FILE_NOT_FOUND,
            })
    }

    fn candidates(directory: &Path, name: &str) -> [PathBuf; 2] {
        [directory.join(name), directory.join(std::format!("{name}.exe"))]
    }

    fn search_path(environment: &[&[u8]]) -> std::ffi::OsString {
        let inherited = environment.iter().find_map(|entry| {
            let (key, value) = entry.split_at(entry.iter().position(|byte| *byte == b'=')?);
            key.eq_ignore_ascii_case(b"PATH").then(|| &value[1..])
        });
        match inherited.map(core::str::from_utf8) {
            Some(Ok(path)) => path.into(),
            _ => std::env::var_os("PATH").unwrap_or_default(),
        }
    }

    #[cfg(test)]
    mod tests {
        use core::mem::{MaybeUninit, size_of};
        use std::env::{self, VarError};
        use std::format;
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Path, PathBuf};
        use std::string::String;
        use std::vec::Vec;

        use windows_sys::Win32::Foundation::{
            GetHandleInformation, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        use windows_sys::Win32::System::JobObjects::{
            IsProcessInJob, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, CREATE_UNICODE_ENVIRONMENT,
            EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
            GetProcessHandleCount, WaitForSingleObject,
        };

        use super::{
            WindowsChild, WindowsSpawnError, spawn_windows_child,
            spawn_windows_child_with_environment,
            spawn_windows_child_with_environment_and_directory,
        };
        use crate::edge_args_windows::{
            WINDOWS_BELOW_NORMAL_PRIORITY, WINDOWS_JOB_DIE_ON_UNHANDLED_EXCEPTION,
            WINDOWS_JOB_KILL_ON_JOB_CLOSE, WINDOWS_JOB_PRIORITY_CLASS, lower_windows_arguments,
            windows_creation_flags, windows_job_limit_flags, write_executable_path,
        };
        use crate::edge_spawn_windows::handles::tests::{
            assert_child_ran, captured_output, create_pipe, exact_test_intent,
        };
        use crate::test_probe::{
            ProbeOutcome, ProbeRunError, probe_name, verify_probe_output,
        };
        use crate::edge_spawn_windows::handles::{WindowsHandleList, WindowsJob};

                const CHILD_VALUE_KEY: &str = "CARGO_VAL_C44C_CHILD_VALUE";
        const EXPECT_PRESENT_KEY: &str = "CARGO_VAL_C44C_EXPECT_PRESENT";
        const EXPECT_DIRECTORY_KEY: &str = "CARGO_VAL_C44D_EXPECT_DIRECTORY";

        fn process_probe_name() -> String {
            probe_name!(child_process_probe)
        }

        fn environment_probe_name() -> String {
            probe_name!(child_environment_probe)
        }

        fn directory_probe_name() -> String {
            probe_name!(child_working_directory_probe)
        }

        fn current_process_handle_count() -> u32 {
            let mut count = 0;
            assert_eq!(
                unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut count) },
                1,
                "process handle count is readable"
            );
            count
        }

        fn wait_for_child_exit(child: &WindowsChild) -> u32 {
            assert_eq!(
                unsafe { WaitForSingleObject(child.raw_process_handle(), u32::MAX) },
                WAIT_OBJECT_0,
                "the exact child exits"
            );
            let mut exit_code = u32::MAX;
            assert_eq!(
                unsafe { GetExitCodeProcess(child.raw_process_handle(), &raw mut exit_code) },
                1
            );
            exit_code
        }

        #[test]
        fn c44b_launches_one_exact_child_and_owns_one_process_handle() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = process_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 2];
            let mut environment_order = [];
            let mut arguments = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the exact child test command");

            let handles_before = current_process_handle_count();
            let child = spawn_windows_child(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            )
            .expect("CreateProcessW starts the exact test child");
            assert_ne!(child.process_id(), 0);
            assert!(!child.raw_process_handle().is_null());
            assert_eq!(current_process_handle_count(), handles_before + 1);
            let exit_code = wait_for_child_exit(&child);
            assert_eq!(exit_code, 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);
            drop(child);
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!(
                "{{\"children_started\":1,\"owned_process_handles\":1,\"owned_handles_after_drop\":0,\"child_exit_code\":{exit_code}}}"
            );
        }

        #[test]
        fn nonexistent_probe_name_runs_zero_tests_and_fails_the_ran_one_test_guard() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let intent = exact_test_intent(executable, "host::windows_spawn::tests::child_process_probe");
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 2];
            let mut environment_order = [];
            let mut arguments = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the exact child test command");

            let child = spawn_windows_child(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            )
            .expect("CreateProcessW starts the stale-name child");
            let exit_code = wait_for_child_exit(&child);
            let stdout = captured_output(&stdout_read);

            assert_eq!(exit_code, 0, "a zero-test run exits cleanly, which is the hazard");
            assert_eq!(
                verify_probe_output(&stdout, ProbeOutcome::Passed),
                Err(ProbeRunError::ZeroTests),
                "child stdout was {stdout:?}"
            );
        }

        #[test]
        fn c44b_missing_executable_returns_without_handle_leaks() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (_stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let directory =
                crate::test_dir::tempdir().expect("missing executable directory is created");
            let executable = directory.path().join("cargo-val-missing-c44b.exe");
            let executable = executable
                .to_str()
                .expect("temporary executable path is valid UTF-8");
            let probe = process_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 2];
            let mut environment_order = [];
            let mut arguments = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the missing executable command");

            // first failed create may open process-wide loader handles; only a second call can show a per-call leak
            let warmup = spawn_windows_child(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            );
            assert!(matches!(
                warmup,
                Err(WindowsSpawnError::CreateFailed { code }) if code != 0
            ));
            let handles_before = current_process_handle_count();
            let result = spawn_windows_child(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            );
            match result {
                Err(WindowsSpawnError::CreateFailed { code }) => assert_ne!(code, 0),
                Err(WindowsSpawnError::InvalidProcessInformation) => {
                    panic!("CreateProcessW failure must retain its operating-system error")
                }
                Err(WindowsSpawnError::InvalidExecutable) => {
                    panic!("absolute executable path must be accepted")
                }
                Err(WindowsSpawnError::InvalidWorkingDirectory) => {
                    panic!("inherited working directory must not be rejected")
                }
                Ok(_) => panic!("missing executable unexpectedly launched"),
            }
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!("{{\"launch_errors\":1,\"owned_process_handles\":0}}");
        }

        #[test]
        fn c44c_child_receives_unicode_environment_override_without_parent_mutation() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = environment_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let system_root = env::var("SystemRoot").expect("Windows provides SystemRoot");
            let inherited_system_root = format!("SystemRoot={system_root}");
            let inherited_environment = [inherited_system_root.as_str()];
            let overrides = [
                "CARGO_VAL_C44C_EXPECT_PRESENT=1",
                "CARGO_VAL_C44C_CHILD_VALUE=λ🚀",
            ];
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 256];
            let mut environment_order = [usize::MAX; 3];
            let mut arguments = lower_windows_arguments(
                &intent,
                &inherited_environment,
                &overrides,
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the child environment block");

            let parent_value = env::var_os(CHILD_VALUE_KEY);
            let handles_before = current_process_handle_count();
            let child = spawn_windows_child_with_environment(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            )
            .expect("CreateProcessW starts the child with UTF-16 environment data");
            assert_eq!(current_process_handle_count(), handles_before + 1);
            let exit_code = wait_for_child_exit(&child);
            assert_eq!(exit_code, 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);
            drop(child);
            assert_eq!(current_process_handle_count(), handles_before);
            assert_eq!(env::var_os(CHILD_VALUE_KEY), parent_value);
            std::println!(
                "{{\"unicode_child_override\":1,\"parent_environment_changes\":0,\"exit_code\":{exit_code}}}"
            );
        }

        #[test]
        fn c44c_child_environment_control_omits_unlisted_values() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = environment_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let system_root = env::var("SystemRoot").expect("Windows provides SystemRoot");
            let inherited_system_root = format!("SystemRoot={system_root}");
            let inherited_environment = [inherited_system_root.as_str()];
            let overrides = ["CARGO_VAL_C44C_EXPECT_PRESENT=0"];
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 128];
            let mut environment_order = [usize::MAX; 2];
            let mut arguments = lower_windows_arguments(
                &intent,
                &inherited_environment,
                &overrides,
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the control environment block");

            let parent_value = env::var_os(CHILD_VALUE_KEY);
            let handles_before = current_process_handle_count();
            let child = spawn_windows_child_with_environment(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            )
            .expect("CreateProcessW starts the environment control child");
            assert_eq!(current_process_handle_count(), handles_before + 1);
            let exit_code = wait_for_child_exit(&child);
            assert_eq!(exit_code, 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);
            drop(child);
            assert_eq!(current_process_handle_count(), handles_before);
            assert_eq!(env::var_os(CHILD_VALUE_KEY), parent_value);
            std::println!(
                "{{\"unlisted_child_value_present\":0,\"parent_environment_changes\":0}}"
            );
        }

        #[test]
        fn c44d_child_uses_requested_working_directory_without_parent_mutation() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = directory_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let child_directory =
                crate::test_dir::tempdir().expect("child working directory exists");
            let child_directory_text = child_directory
                .path()
                .to_str()
                .expect("temporary directory path is valid UTF-8");
            let child_directory_entry = format!("{EXPECT_DIRECTORY_KEY}={child_directory_text}");
            let system_root = env::var("SystemRoot").expect("Windows provides SystemRoot");
            let system_root_entry = format!("SystemRoot={system_root}");
            let inherited_environment =
                [system_root_entry.as_str(), child_directory_entry.as_str()];
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 1024];
            let mut environment_order = [usize::MAX; 2];
            let mut arguments = lower_windows_arguments(
                &intent,
                &inherited_environment,
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the child environment block");
            let child_directory_wide = wide_path(child_directory.path());
            let parent_directory = env::current_dir().expect("parent working directory exists");

            let child = spawn_windows_child_with_environment_and_directory(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
                &child_directory_wide,
            )
            .expect("CreateProcessW accepts the requested child directory");
            assert_eq!(wait_for_child_exit(&child), 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);
            drop(child);
            assert_eq!(
                env::current_dir().expect("parent directory remains readable"),
                parent_directory
            );
            std::println!("{{\"child_directory_matches\":1,\"parent_directory_changes\":0}}");
        }

        #[test]
        fn c44d_wrong_directory_control_is_rejected_by_child_probe() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = directory_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let child_directory =
                crate::test_dir::tempdir().expect("expected child directory exists");
            let child_directory_text = child_directory
                .path()
                .to_str()
                .expect("temporary directory path is valid UTF-8");
            let child_directory_entry = format!("{EXPECT_DIRECTORY_KEY}={child_directory_text}");
            let system_root = env::var("SystemRoot").expect("Windows provides SystemRoot");
            let system_root_entry = format!("SystemRoot={system_root}");
            let inherited_environment =
                [system_root_entry.as_str(), child_directory_entry.as_str()];
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 1024];
            let mut environment_order = [usize::MAX; 2];
            let mut arguments = lower_windows_arguments(
                &intent,
                &inherited_environment,
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the child environment block");
            let parent_directory = env::current_dir().expect("parent working directory exists");
            let parent_directory_wide = wide_path(&parent_directory);

            let child = spawn_windows_child_with_environment_and_directory(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
                &parent_directory_wide,
            )
            .expect("CreateProcessW accepts the parent directory for the control");
            assert_ne!(wait_for_child_exit(&child), 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Failed);
            drop(child);
            assert_eq!(
                env::current_dir().expect("parent directory remains readable"),
                parent_directory
            );
            std::println!(
                "{{\"wrong_child_directory_rejected\":1,\"parent_directory_changes\":0}}"
            );
        }

        fn absolute_executable(path: &str) -> Vec<u16> {
            let mut storage = [0_u16; 1024];
            write_executable_path(path, &mut storage)
                .expect("test executable path is absolute and fits the storage")
                .to_vec()
        }

        fn child_in_job_state(job: &WindowsJob, child: &WindowsChild) -> bool {
            let mut in_job = 0;
            assert_eq!(
                unsafe { IsProcessInJob(child.raw_process_handle(), job.raw(), &raw mut in_job) },
                1,
                "job membership is queryable"
            );
            in_job != 0
        }

        #[test]
        fn c67_job_object_carries_kill_on_close_die_on_exception_and_below_normal_priority() {
            let job = WindowsJob::new().expect("the run job is created");
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();

            assert_eq!(
                unsafe {
                    QueryInformationJobObject(
                        job.raw(),
                        JobObjectExtendedLimitInformation,
                        (&raw mut limits).cast(),
                        u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                            .expect("struct size fits u32"),
                        core::ptr::null_mut(),
                    )
                },
                1,
                "job limits are readable"
            );
            assert_eq!(
                limits.BasicLimitInformation.LimitFlags & windows_job_limit_flags(),
                windows_job_limit_flags()
            );
            assert_eq!(
                limits.BasicLimitInformation.PriorityClass,
                BELOW_NORMAL_PRIORITY_CLASS
            );
            std::println!(
                "{{\"job_limit_flags\":{},\"job_priority_class\":{}}}",
                limits.BasicLimitInformation.LimitFlags,
                limits.BasicLimitInformation.PriorityClass
            );
        }

        #[test]
        fn c67_limit_constants_match_windows_sys_values() {
            assert_eq!(
                WINDOWS_JOB_KILL_ON_JOB_CLOSE,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            );
            assert_eq!(
                WINDOWS_JOB_DIE_ON_UNHANDLED_EXCEPTION,
                JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            );
            assert_eq!(WINDOWS_JOB_PRIORITY_CLASS, JOB_OBJECT_LIMIT_PRIORITY_CLASS);
            assert_eq!(WINDOWS_BELOW_NORMAL_PRIORITY, BELOW_NORMAL_PRIORITY_CLASS);
            assert_eq!(
                windows_creation_flags(),
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT
            );
        }

        #[test]
        fn c67_child_is_created_inside_the_run_job_and_thread_handle_is_closed() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let job = WindowsJob::new().expect("the run job is created");
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new_in_job(&handles, &job, &mut attribute_storage)
                .expect("caller storage holds the handle list and the job list");
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = process_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 2];
            let mut environment_order = [];
            let mut arguments = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the exact child test command");

            let handles_before = current_process_handle_count();
            let child = spawn_windows_child(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            )
            .expect("CreateProcessW starts the child inside the job");
            assert!(child_in_job_state(&job, &child), "child joined at creation");
            assert_eq!(
                current_process_handle_count(),
                handles_before + 1,
                "only the process handle remains; the thread handle is closed"
            );
            assert_eq!(wait_for_child_exit(&child), 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);
            drop(child);
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!("{{\"child_in_job\":1,\"handles_retained_per_child\":1}}");
        }

        #[test]
        fn c67_child_without_job_list_is_outside_the_run_job() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let job = WindowsJob::new().expect("the run job is created");
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = process_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 2];
            let mut environment_order = [];
            let mut arguments = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the exact child test command");

            let child = spawn_windows_child(
                &absolute_executable(executable),
                &mut arguments,
                &handle_list,
            )
            .expect("CreateProcessW starts the control child");
            assert!(
                !child_in_job_state(&job, &child),
                "control child has no job"
            );
            assert_eq!(wait_for_child_exit(&child), 0);
            assert_child_ran(&stdout_read, ProbeOutcome::Passed);
        }

        #[test]
        fn c67_unterminated_executable_is_rejected_before_creation() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (_stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = std::env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let probe = process_probe_name();
            let intent = exact_test_intent(executable, &probe);
            let mut command_line = [0_u16; 2048];
            let mut environment_block = [0_u16; 2];
            let mut environment_order = [];
            let mut arguments = lower_windows_arguments(
                &intent,
                &[],
                &[],
                &mut command_line,
                &mut environment_block,
                &mut environment_order,
            )
            .expect("caller buffers fit the exact child test command");
            let unterminated = executable.encode_utf16().collect::<Vec<u16>>();

            let result = spawn_windows_child(&unterminated, &mut arguments, &handle_list);

            assert!(matches!(result, Err(WindowsSpawnError::InvalidExecutable)));
        }
        #[test]
        fn child_process_probe() {
            if !env::args().any(|argument| argument == "--exact") {
                return;
            }
            for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = unsafe { GetStdHandle(standard_handle) };
                assert!(!handle.is_null());
                assert_ne!(handle, INVALID_HANDLE_VALUE);
                let mut flags = 0;
                assert_eq!(unsafe { GetHandleInformation(handle, &raw mut flags) }, 1);
            }
        }

        #[test]
        fn child_environment_probe() {
            if !env::args().any(|argument| argument == "--exact") {
                return;
            }
            assert!(env::var("SystemRoot").is_ok());
            match env::var(EXPECT_PRESENT_KEY).as_deref() {
                Ok("1") => assert_eq!(env::var(CHILD_VALUE_KEY).as_deref(), Ok("λ🚀")),
                Ok("0") => assert!(matches!(
                    env::var(CHILD_VALUE_KEY),
                    Err(VarError::NotPresent)
                )),
                _ => panic!("child expectation is missing or invalid"),
            }
        }

        #[test]
        fn child_working_directory_probe() {
            if !env::args().any(|argument| argument == "--exact") {
                return;
            }
            let expected_directory = env::var_os(EXPECT_DIRECTORY_KEY)
                .expect("child receives expected directory from its explicit environment");
            let actual_directory = env::current_dir().expect("child current directory is readable");
            assert_eq!(actual_directory, PathBuf::from(expected_directory));
        }

        fn wide_path(path: &Path) -> Vec<u16> {
            path.as_os_str().encode_wide().chain(Some(0)).collect()
        }
    }
}
pub use spawn::*;
