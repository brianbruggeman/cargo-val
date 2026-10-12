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

use crate::windows_arguments::{
    WINDOWS_BELOW_NORMAL_PRIORITY, windows_attribute_count, windows_attribute_storage_words,
    windows_job_limit_flags,
};

/// Owns one Win32 handle shared by child launch and host I/O adapters.
pub(super) struct WindowsOwnedHandle(HANDLE);

impl WindowsOwnedHandle {
    pub(super) const fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub(super) const fn raw(&self) -> HANDLE {
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
        if required_bytes == 0 || storage.len() < windows_attribute_storage_words(required_bytes) {
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
    use crate::windows_arguments::lower_windows_arguments;

    pub(crate) const CHILD_PROBE_NAME: &str = "host::windows_handles::tests::child_handle_probe";

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
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("struct size fits u32"),
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
