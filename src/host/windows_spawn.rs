//! Nonblocking ownership of Windows child process handles.

use core::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
};

use crate::host::windows_handles::WindowsHandleList;
use crate::windows_arguments::WindowsArguments;

/// Failures while creating a Windows child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSpawnError {
    /// `CreateProcessW` rejected the requested executable or launch data.
    CreateFailed { code: u32 },
    /// `CreateProcessW` returned an invalid process or thread handle.
    InvalidProcessInformation,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A launched child whose process handle remains owned until drop.
pub struct WindowsChild {
    process: OwnedHandle,
    process_id: u32,
}

enum ChildEnvironment<'environment> {
    Inherit,
    Block(&'environment [u16]),
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
        self.process.0
    }
}

/// Starts one exact-test child and returns without waiting for its completion.
pub fn spawn_windows_child(
    arguments: &mut WindowsArguments<'_>,
    handle_list: &WindowsHandleList<'_>,
) -> Result<WindowsChild, WindowsSpawnError> {
    let command_line = arguments.command_line();
    spawn_windows_child_inner(command_line, handle_list, ChildEnvironment::Inherit)
}

/// Starts one child with the caller's complete UTF-16 environment block.
pub fn spawn_windows_child_with_environment(
    arguments: &mut WindowsArguments<'_>,
    handle_list: &WindowsHandleList<'_>,
) -> Result<WindowsChild, WindowsSpawnError> {
    let (command_line, environment_block) = arguments.create_process_buffers();
    spawn_windows_child_inner(
        command_line,
        handle_list,
        ChildEnvironment::Block(environment_block),
    )
}

fn spawn_windows_child_inner(
    command_line: &mut [u16],
    handle_list: &WindowsHandleList<'_>,
    environment: ChildEnvironment<'_>,
) -> Result<WindowsChild, WindowsSpawnError> {
    let startup_info = handle_list.startup_info() as *const _ as *const _;
    let mut process_information = PROCESS_INFORMATION::default();
    let (environment_pointer, creation_flags) = match environment {
        ChildEnvironment::Inherit => (core::ptr::null(), EXTENDED_STARTUPINFO_PRESENT),
        ChildEnvironment::Block(environment_block) => (
            environment_block.as_ptr().cast::<c_void>(),
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
        ),
    };
    let created = unsafe {
        CreateProcessW(
            core::ptr::null(),
            command_line.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            1,
            creation_flags,
            environment_pointer,
            core::ptr::null(),
            startup_info,
            &mut process_information,
        )
    };
    if created == 0 {
        return Err(WindowsSpawnError::CreateFailed {
            code: unsafe { GetLastError() },
        });
    }
    if !valid_handle(process_information.hProcess) || !valid_handle(process_information.hThread) {
        close_if_valid(process_information.hThread);
        close_if_valid(process_information.hProcess);
        return Err(WindowsSpawnError::InvalidProcessInformation);
    }

    let process = OwnedHandle(process_information.hProcess);
    drop(OwnedHandle(process_information.hThread));
    Ok(WindowsChild {
        process,
        process_id: process_information.dwProcessId,
    })
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
    use core::mem::MaybeUninit;
    use std::env::{self, VarError};
    use std::format;

    use windows_sys::Win32::Foundation::{
        GetHandleInformation, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, GetProcessHandleCount, WaitForSingleObject,
    };

    use super::{
        WindowsChild, WindowsSpawnError, spawn_windows_child, spawn_windows_child_with_environment,
    };
    use crate::host::windows_handles::WindowsHandleList;
    use crate::host::windows_handles::tests::{create_pipe, exact_test_intent};
    use crate::windows_arguments::lower_windows_arguments;

    const CHILD_PROCESS_PROBE_NAME: &str = "host::windows_spawn::tests::child_process_probe";
    const CHILD_ENVIRONMENT_PROBE_NAME: &str =
        "host::windows_spawn::tests::child_environment_probe";
    const CHILD_VALUE_KEY: &str = "CARGO_VAL_C44C_CHILD_VALUE";
    const EXPECT_PRESENT_KEY: &str = "CARGO_VAL_C44C_EXPECT_PRESENT";

    fn current_process_handle_count() -> u32 {
        let mut count = 0;
        assert_eq!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
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
            unsafe { GetExitCodeProcess(child.raw_process_handle(), &mut exit_code) },
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
        let child = spawn_windows_child(&mut arguments, &handle_list)
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
        let directory = tempfile::tempdir().expect("missing executable directory is created");
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

        let handles_before = current_process_handle_count();
        let result = spawn_windows_child(&mut arguments, &handle_list);
        match result {
            Err(WindowsSpawnError::CreateFailed { code }) => assert_ne!(code, 0),
            Err(WindowsSpawnError::InvalidProcessInformation) => {
                panic!("CreateProcessW failure must retain its operating-system error")
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
        let mut environment_order = [usize::MAX; 2];
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
        let child = spawn_windows_child_with_environment(&mut arguments, &handle_list)
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
        let mut environment_order = [usize::MAX; 1];
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
        let child = spawn_windows_child_with_environment(&mut arguments, &handle_list)
            .expect("CreateProcessW starts the environment control child");
        assert_eq!(current_process_handle_count(), handles_before + 1);
        let exit_code = wait_for_child_exit(&child);
        assert_eq!(exit_code, 0);
        drop(child);
        assert_eq!(current_process_handle_count(), handles_before);
        assert_eq!(env::var_os(CHILD_VALUE_KEY), parent_value);
        std::println!("{{\"unlisted_child_value_present\":0,\"parent_environment_changes\":0}}");
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
            assert_eq!(unsafe { GetHandleInformation(handle, &mut flags) }, 1);
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
}
