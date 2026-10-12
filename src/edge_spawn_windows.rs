//! Windows edge for spawning a child with captured stdout and stderr.

pub mod handles {
    //! Caller-storage `STARTUPINFOEXW` handle lists for isolated child inheritance.

    use core::ffi::c_void;
    use core::mem::{MaybeUninit, size_of};

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
    pub(crate) struct WindowsOwnedHandle(HANDLE);

    impl WindowsOwnedHandle {
        pub(crate) const fn new(handle: HANDLE) -> Self {
            Self(handle)
        }

        pub(crate) const fn raw(&self) -> HANDLE {
            self.0
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
        use windows_sys::Win32::System::Pipes::CreatePipe;
        use windows_sys::Win32::System::Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
            GetCurrentProcess, GetExitCodeProcess, GetProcessHandleCount, STARTF_USESTDHANDLES,
            STARTUPINFOEXW, STARTUPINFOW, WaitForSingleObject,
        };

        use super::{WindowsHandleList, WindowsHandleListError};
        use crate::ExecutablePath;
        use crate::edge_args_windows::lower_windows_arguments;

        pub(crate) const CHILD_PROBE_NAME: &str =
            "host::windows_handles::tests::child_handle_probe";

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

        pub(crate) fn exact_test_intent<'executable, 'name>(
            executable: &'executable str,
            test_name: &'name str,
        ) -> crate::test_intent::ExactTestIntent<'executable, 'static, 'name> {
            crate::test_intent::ExactTestIntent {
                executable: ExecutablePath::Borrowed(executable),
                test_name,
            }
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
            let stdio_handles = [stdin_read.0, stdout_write.0, stderr_write.0];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&stdio_handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            assert_eq!(handle_list.handles(), &stdio_handles);
            assert_eq!(
                handle_list.startup_info().StartupInfo.dwFlags,
                STARTF_USESTDHANDLES
            );
            let sentinel_id = sentinel_read.0 as usize;
            let sentinel_environment = std::format!("CARGO_VAL_SENTINEL_HANDLE={sentinel_id}");
            let expected_environment = "CARGO_VAL_EXPECT_SENTINEL=0";
            let overrides = [sentinel_environment.as_str(), expected_environment];
            let executable_path = std::env::current_exe().expect("test executable path exists");
            let executable_path = executable_path
                .to_str()
                .expect("Windows test executable path is representable as UTF-8");
            let intent = exact_test_intent(executable_path, CHILD_PROBE_NAME);
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

            let unrestricted_environment = "CARGO_VAL_EXPECT_SENTINEL=1";
            let overrides = [sentinel_environment.as_str(), unrestricted_environment];
            let intent = exact_test_intent(executable_path, CHILD_PROBE_NAME);
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
            basic_startup.StartupInfo.hStdOutput = stdout_write.0;
            basic_startup.StartupInfo.hStdError = stderr_write.0;
            basic_startup.lpAttributeList = core::ptr::null_mut();
            let control_exit = launch_probe(command_line, environment_block, &basic_startup, false);
            assert_eq!(
                control_exit, 0,
                "unrestricted control observes inherited sentinel"
            );
            std::println!(
                "{{\"stdio_handles\":{},\"restricted_sentinel_inherited\":0,\"unrestricted_control_sentinel_inherited\":1,\"restricted_exit\":{restricted_exit},\"control_exit\":{control_exit}}}",
                handle_list.handles().len()
            );
            drop((stdin_write, stdout_read, stderr_read, sentinel_write));
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
            let sentinel_inherited = unsafe { GetHandleInformation(sentinel, &raw mut flags) } != 0;
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
            let parent_reader = WindowsOwnedHandle::new(parent_reader);

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

            Ok(Self {
                parent_reader,
                child_writer: Some(WindowsOwnedHandle::new(child_writer)),
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

mod spawn {
    //! Nonblocking ownership of Windows child process handles.

    use core::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION};

    use crate::edge_args_windows::WindowsArguments;
    use crate::edge_args_windows::windows_creation_flags;
    use crate::edge_spawn_windows::handles::WindowsHandleList;
    use crate::edge_spawn_windows::handles::WindowsOwnedHandle;

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
    pub struct WindowsChild {
        process: WindowsOwnedHandle,
        process_id: u32,
    }

    enum ChildEnvironment<'environment> {
        Inherit,
        Block(&'environment [u16]),
    }

    enum ChildWorkingDirectory<'directory> {
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
                windows_creation_flags(),
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

        let process = WindowsOwnedHandle::new(process_information.hProcess);
        drop(WindowsOwnedHandle::new(process_information.hThread));
        Ok(WindowsChild {
            process,
            process_id: process_information.dwProcessId,
        })
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

    #[cfg(test)]
    mod tests {
        use core::mem::{MaybeUninit, size_of};
        use std::env::{self, VarError};
        use std::format;
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Path, PathBuf};
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
            BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
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
        use crate::edge_spawn_windows::handles::tests::{create_pipe, exact_test_intent};
        use crate::edge_spawn_windows::handles::{WindowsHandleList, WindowsJob};

        const CHILD_PROCESS_PROBE_NAME: &str = "host::windows_spawn::tests::child_process_probe";
        const CHILD_ENVIRONMENT_PROBE_NAME: &str =
            "host::windows_spawn::tests::child_environment_probe";
        const CHILD_VALUE_KEY: &str = "CARGO_VAL_C44C_CHILD_VALUE";
        const EXPECT_PRESENT_KEY: &str = "CARGO_VAL_C44C_EXPECT_PRESENT";
        const EXPECT_DIRECTORY_KEY: &str = "CARGO_VAL_C44D_EXPECT_DIRECTORY";
        const CHILD_DIRECTORY_PROBE_NAME: &str =
            "host::windows_spawn::tests::child_working_directory_probe";

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
            let intent = exact_test_intent(executable, CHILD_PROCESS_PROBE_NAME);
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
            drop(child);
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!(
                "{{\"children_started\":1,\"owned_process_handles\":1,\"owned_handles_after_drop\":0,\"child_exit_code\":{exit_code}}}"
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
            let intent = exact_test_intent(executable, CHILD_PROCESS_PROBE_NAME);
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
            let (_stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let intent = exact_test_intent(executable, CHILD_ENVIRONMENT_PROBE_NAME);
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
            let (_stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let intent = exact_test_intent(executable, CHILD_ENVIRONMENT_PROBE_NAME);
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
            let (_stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let intent = exact_test_intent(executable, CHILD_DIRECTORY_PROBE_NAME);
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
            let (_stdout_read, stdout_write) = create_pipe();
            let (_stderr_read, stderr_write) = create_pipe();
            let handles = [stdin_read.raw(), stdout_write.raw(), stderr_write.raw()];
            let mut attribute_storage = [MaybeUninit::<usize>::uninit(); 64];
            let handle_list = WindowsHandleList::new(&handles, &mut attribute_storage)
                .expect("caller storage builds the stdio handle whitelist");
            let executable = env::current_exe().expect("test executable path exists");
            let executable = executable
                .to_str()
                .expect("Windows test executable path is valid UTF-8");
            let intent = exact_test_intent(executable, CHILD_DIRECTORY_PROBE_NAME);
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
                EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT
            );
        }

        #[test]
        fn c67_child_is_created_inside_the_run_job_and_thread_handle_is_closed() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (_stdout_read, stdout_write) = create_pipe();
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
            let intent = exact_test_intent(executable, CHILD_PROCESS_PROBE_NAME);
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
            drop(child);
            assert_eq!(current_process_handle_count(), handles_before);
            std::println!("{{\"child_in_job\":1,\"handles_retained_per_child\":1}}");
        }

        #[test]
        fn c67_child_without_job_list_is_outside_the_run_job() {
            let (stdin_read, _stdin_write) = create_pipe();
            let (_stdout_read, stdout_write) = create_pipe();
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
            let intent = exact_test_intent(executable, CHILD_PROCESS_PROBE_NAME);
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
            let intent = exact_test_intent(executable, CHILD_PROCESS_PROBE_NAME);
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
