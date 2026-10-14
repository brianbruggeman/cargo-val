use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd};

pub const COLUMNS: u16 = 80;
pub const ROWS: u16 = 40;

pub struct Pty {
    pub master: File,
    pub slave: OwnedFd,
}

// a ChildStream parks on WouldBlock, so a blocking master would stall the whole reactor thread until the child next writes
pub fn open_pty() -> Pty {
    let mut size = libc::winsize { ws_row: ROWS, ws_col: COLUMNS, ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master, mut slave) = (0, 0);
    let opened = unsafe { libc::openpty(&raw mut master, &raw mut slave, core::ptr::null_mut(), core::ptr::null_mut(), &raw mut size) };
    assert_eq!(opened, 0, "openpty: {}", std::io::Error::last_os_error());
    let flagged = unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) };
    assert_eq!(flagged, 0, "the pty master turns nonblocking: {}", std::io::Error::last_os_error());
    unsafe { Pty { master: File::from(OwnedFd::from_raw_fd(master)), slave: OwnedFd::from_raw_fd(slave) } }
}
