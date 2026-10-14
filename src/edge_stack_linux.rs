use std::fs;
use std::io;
use std::path::Path;
use std::vec::Vec;

use crate::stack_capture::{Unavailable, append};
use crate::stack_frames::Format;

/// The format [`capture`] writes into the buffer.
pub const FORMAT: Format = Format::KernelStack;

/// Captures the kernel stack of every thread of `pid` into `buffer`, as `(written, dropped)`: bytes now in `buffer`
/// and useful bytes that did not fit.
///
/// Each thread becomes a `Thread_<tid>` header line followed by its `/proc/<pid>/task/<tid>/stack` lines
/// (`[<0>] symbol+offset/size`), which [`frames`](crate::stack_frames::frames) parses with [`FORMAT`]. The kernel
/// allows the stack read only to privileged callers. For a thread whose stack is refused, the world-readable
/// `wchan` file supplies the one symbol the thread is sleeping in, written as a single `[<0>] symbol` frame: on
/// versailles an unprivileged read of a child blocked in `open` on a fifo gave `wait_for_partner`, while `syscall`
/// answered EPERM, so no fd number is available without privilege. When no thread gives a stack or a wchan symbol the
/// answer is [`Unavailable::Denied`], because the ptrace-plus-unwind fallback needs a dependency that Cargo.lock
/// does not hold. Cold path: it allocates one string per thread directory entry.
///
/// # Errors
///
/// Returns [`Unavailable`] naming why no thread produced a stack.
pub fn capture(pid: u32, buffer: &mut [u8]) -> Result<(usize, usize), Unavailable> {
    let task_directory = std::format!("/proc/{pid}/task");
    let entries = fs::read_dir(&task_directory)
        .map_err(|error| Unavailable::Io { operation: "read /proc task directory", os_code: os_code(&error) })?;
    let mut written = 0;
    let mut dropped = 0;
    let mut denied = None;
    for entry in entries.flatten() {
        let thread = entry.file_name().to_string_lossy().into_owned();
        let Ok(stack) = fs::read(entry.path().join("stack")).or_else(|error| wchan_frame(&entry.path(), error, &mut denied)) else {
            continue;
        };
        let bytes = stack.trim_ascii();
        if !bytes.is_empty() {
            dropped += append(buffer, &mut written, std::format!("Thread_{thread}\n").as_bytes());
            dropped += append(buffer, &mut written, bytes);
            dropped += append(buffer, &mut written, b"\n");
        }
    }
    match (written + dropped, denied) {
        (0, Some(os_code)) => Err(Unavailable::Denied { os_code }),
        (0, None) => Err(Unavailable::Empty { source: "/proc/<pid>/task/*/stack" }),
        _ => Ok((written, dropped)),
    }
}

fn wchan_frame(task: &Path, stack_error: io::Error, denied: &mut Option<i32>) -> io::Result<Vec<u8>> {
    if stack_error.kind() == io::ErrorKind::PermissionDenied {
        *denied = Some(os_code(&stack_error));
    }
    let symbol = fs::read(task.join("wchan"))?;
    match symbol.as_slice() {
        b"" | b"0" => Err(stack_error),
        name => Ok([b"[<0>] ", name].concat()),
    }
}

fn os_code(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}
