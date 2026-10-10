//! Overlapped Windows pipe endpoints for child process output.

use core::mem::size_of;

use windows_sys::Win32::Foundation::{GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::host::windows_handles::WindowsOwnedHandle;

/// Failure while creating one overlapped parent-read/child-write pipe pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipePairError {
    /// The supplied name is not a single NUL-terminated UTF-16 string.
    InvalidName,
    /// Windows rejected creation of the overlapped parent endpoint.
    CreateServerFailed { code: u32 },
    /// Windows rejected opening the inheritable child endpoint.
    OpenChildWriterFailed { code: u32 },
}

/// Owns an overlapped parent reader and inheritable child writer.
///
/// The parent endpoint is read by an overlapped completion adapter using
/// stable caller-owned session storage. Its completed byte slice enters the
/// existing Proxima `HostEventPipe` flow. Pass `child_write_handle` into
/// [`super::windows_handles::WindowsHandleList`] for child stdio, then call
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
                0,
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

        let mut child_security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: core::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let child_writer = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_WRITE,
                0,
                &mut child_security,
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
    use crate::host::windows_handles::WindowsOwnedHandle;
    use crate::host::windows_handles::tests::create_pipe;

    static NEXT_PIPE_ID: AtomicU32 = AtomicU32::new(0);

    fn current_process_handle_count() -> u32 {
        let mut count = 0;
        assert_eq!(
            unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) },
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
            unsafe { GetHandleInformation(parent_reader, &mut parent_flags) },
            1
        );
        assert_eq!(
            unsafe { GetHandleInformation(child_writer, &mut child_flags) },
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
                    read_buffer.len() as u32,
                    &mut bytes_read,
                    &mut overlapped,
                )
            },
            0,
            "an empty overlapped read remains pending"
        );
        assert_eq!(unsafe { GetLastError() }, ERROR_IO_PENDING);
        assert_eq!(unsafe { CancelIoEx(parent_reader, &overlapped) }, 1);
        assert_eq!(
            unsafe { WaitForSingleObject(event.raw(), 5_000) },
            WAIT_OBJECT_0,
            "test cleanup observes completion of the canceled read"
        );
        drop(event);

        let (control_reader, control_writer) = create_pipe();
        let control_input = [b'x'];
        let mut control_bytes_written = 0;
        assert_eq!(
            unsafe {
                WriteFile(
                    control_writer.raw(),
                    control_input.as_ptr(),
                    control_input.len() as u32,
                    &mut control_bytes_written,
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
                    control_output.len() as u32,
                    &mut control_bytes_read,
                    &mut control_overlapped,
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
        let pipe_pair =
            WindowsPipePair::new(&pipe_name).expect("the first instance claims its unique name");
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
            Ok(_) => panic!("the first-instance control unexpectedly created another pair"),
        }
        assert_eq!(current_process_handle_count(), handles_with_pair);
        drop(pipe_pair);
        assert_eq!(current_process_handle_count(), handles_before);
        std::println!("{{\"duplicate_creation_failures\":1,\"leaked_handles\":0}}");
    }
}
