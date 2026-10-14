use core::fmt;

use crate::stack_frames::Format;

/// Why a platform could not produce a stack: always a named reason, never an empty success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The platform has no capture yet; `follow_up` names the slice that adds it.
    Unsupported { follow_up: &'static str },
    /// The stack tool could not be started.
    NoTool { tool: &'static str, os_code: i32 },
    /// The stack tool ran and failed.
    ToolFailed { tool: &'static str, status: i32 },
    /// The kernel refused to show the process's stacks, and no unwinder dependency exists for a ptrace fallback.
    Denied { os_code: i32 },
    /// The tool or the kernel produced no stack text.
    Empty { source: &'static str },
    /// A file or process operation failed.
    Io { operation: &'static str, os_code: i32 },
}

impl fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { follow_up } => write!(formatter, "unavailable: stack capture unsupported on this platform, follow-up {follow_up}"),
            Self::NoTool { tool, os_code } => write!(formatter, "unavailable: no stack tool: {tool} did not start (os error {os_code})"),
            Self::ToolFailed { tool, status } => write!(formatter, "unavailable: {tool} exited with status {status}"),
            Self::Denied { os_code } => write!(formatter, "unavailable: kernel stack read denied (os error {os_code}); ptrace fallback needs an unwinder dependency and Cargo.lock has none"),
            Self::Empty { source } => write!(formatter, "unavailable: {source} produced no stack text"),
            Self::Io { operation, os_code } => write!(formatter, "unavailable: {operation} failed (os error {os_code})"),
        }
    }
}

/// The result of one stack capture into a caller-provided buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Capture {
    Captured {
        format: Format,
        /// Bytes now in the buffer.
        written: usize,
        /// Useful bytes that did not fit the buffer.
        dropped: usize,
        /// Bytes the tool or kernel produced before the useful section was cut out.
        source: usize,
    },
    Unavailable(Unavailable),
}

/// Copies as much of `bytes` as fits after `written` and returns how many bytes did not fit.
pub fn append(buffer: &mut [u8], written: &mut usize, bytes: &[u8]) -> usize {
    let room = buffer.len() - *written;
    let take = bytes.len().min(room);
    buffer[*written..*written + take].copy_from_slice(&bytes[..take]);
    *written += take;
    bytes.len() - take
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use super::{Unavailable, append};

    #[test]
    fn append_reports_the_bytes_that_did_not_fit() {
        let mut buffer = [0_u8; 8];
        let mut written = 0;

        let first = append(&mut buffer, &mut written, b"Thread_1\n");
        let second = append(&mut buffer, &mut written, b"more");

        assert_eq!((first, second, written), (1, 4, 8));
        assert_eq!(&buffer, b"Thread_1");
    }

    #[test]
    fn unavailable_reasons_read_as_the_exact_text() {
        assert_eq!(
            Unavailable::NoTool { tool: "/usr/bin/sample", os_code: 2 }.to_string(),
            "unavailable: no stack tool: /usr/bin/sample did not start (os error 2)"
        );
        assert_eq!(
            Unavailable::Unsupported { follow_up: "S8w" }.to_string(),
            "unavailable: stack capture unsupported on this platform, follow-up S8w"
        );
    }
}
