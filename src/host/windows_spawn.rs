//! Nonblocking ownership of Windows child process handles.

use core::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
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
    let startup_info = handle_list.startup_info() as *const _ as *const _;
    let mut process_information = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            core::ptr::null(),
            command_line.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT,
            core::ptr::null::<c_void>(),
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

    use windows_sys::Win32::Foundation::{
        GetHandleInformation, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, GetProcessHandleCount, WaitForSingleObject,
    };

    use super::{WindowsSpawnError, spawn_windows_child};
    use crate::host::windows_handles::WindowsHandleList;
    use crate::host::windows_handles::tests::{create_pipe, exact_test_intent};
    use crate::windows_arguments::lower_windows_arguments;

    const CHILD_PROCESS_PROBE_NAME: &str = "host::windows_spawn::tests::child_process_probe";

    fn current_process_handle_count() -> u32 {
        let mut count = 0;
        assert_eq!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
            1,
            "process handle count is readable"
        );
        count
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
    fn child_process_probe() {
        if !std::env::args().any(|argument| argument == "--exact") {
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
}
