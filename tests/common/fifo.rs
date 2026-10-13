use std::io;
use std::os::fd::RawFd;

const SAFETY_BOUND_SECONDS: libc::time_t = 240;

// select(2) reports a fifo whose last writer left on macOS where poll(2) does not; the bound only fails the test, it never decides it
pub fn await_readable(descriptor: RawFd, what: &str) {
    let mut readable: libc::fd_set = unsafe { core::mem::zeroed() };
    unsafe {
        libc::FD_ZERO(&raw mut readable);
        libc::FD_SET(descriptor, &raw mut readable);
    }
    let mut timeout = libc::timeval { tv_sec: SAFETY_BOUND_SECONDS, tv_usec: 0 };
    let ready = unsafe {
        libc::select(descriptor + 1, &raw mut readable, core::ptr::null_mut(), core::ptr::null_mut(), &raw mut timeout)
    };
    assert_eq!(ready, 1, "{what}");
}

/// Reads one line from a nonblocking descriptor whose writer may still be open, waiting for readiness and absorbing `WouldBlock` until the newline or EOF.
///
/// `read_to_string` on such a descriptor fails with `WouldBlock` as soon as the written bytes are drained, because the writer has not closed.
pub fn read_line(descriptor: RawFd, what: &str) -> String {
    let mut line = Vec::new();
    let mut chunk = [0_u8; 64];
    while !line.ends_with(b"\n") {
        await_readable(descriptor, what);
        let count = unsafe { libc::read(descriptor, chunk.as_mut_ptr().cast(), chunk.len()) };
        match usize::try_from(count) {
            Ok(0) => break,
            Ok(count) => line.extend_from_slice(&chunk[..count]),
            Err(_) => absorb_would_block(&io::Error::last_os_error()),
        }
    }
    String::from_utf8(line).expect("the fifo carries utf-8")
}

fn absorb_would_block(error: &io::Error) {
    assert!(
        matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted),
        "the fifo failed to read: {error}"
    );
}
