use std::fs;
use std::io;
use std::string::ToString;

use crate::stack_capture::{Capture, Unavailable, append};
use crate::stack_frames::Format;

/// Captures the kernel stack of every thread of `pid` from `/proc/<pid>/task/<tid>/stack` into `buffer`.
///
/// Each thread becomes a `Thread_<tid>` header line followed by the file's `[<0>] symbol+offset/size` lines, which
/// [`Rows`](crate::stack_frames::Rows) parses with [`Format::KernelStack`]. The kernel allows the read only to
/// privileged callers; otherwise the answer is [`Unavailable::Denied`], because the ptrace-plus-unwind fallback needs
/// a dependency that Cargo.lock does not hold. Cold path: it allocates one string per thread directory entry.
pub fn capture(pid: u32, buffer: &mut [u8]) -> Capture {
    let task_directory = std::format!("/proc/{pid}/task");
    let entries = match fs::read_dir(&task_directory) {
        Ok(entries) => entries,
        Err(error) => return Capture::Unavailable(Unavailable::Io { operation: "read /proc task directory", os_code: os_code(&error) }),
    };
    let mut written = 0;
    let mut dropped = 0;
    let mut source = 0;
    let mut denied = None;
    for entry in entries.flatten() {
        let thread = entry.file_name().to_string_lossy().into_owned();
        match fs::read(entry.path().join("stack")) {
            Ok(bytes) if bytes.is_empty() => {}
            Ok(bytes) => {
                source += bytes.len();
                dropped += append(buffer, &mut written, std::format!("Thread_{thread}\n").as_bytes());
                dropped += append(buffer, &mut written, &bytes);
            }
            Err(error) if matches!(error.kind(), io::ErrorKind::PermissionDenied) => denied = Some(os_code(&error)),
            Err(_) => {}
        }
    }
    outcome(written, dropped, source, denied)
}

fn outcome(written: usize, dropped: usize, source: usize, denied: Option<i32>) -> Capture {
    match (written + dropped, denied) {
        (0, Some(os_code)) => Capture::Unavailable(Unavailable::Denied { os_code }),
        (0, None) => Capture::Unavailable(Unavailable::Empty { source: "/proc/<pid>/task/*/stack" }),
        _ => Capture::Captured { format: Format::KernelStack, written, dropped, source },
    }
}

fn os_code(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}
