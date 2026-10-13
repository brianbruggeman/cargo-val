#![cfg(unix)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{FromRawFd, RawFd};

#[path = "common/fifo.rs"]
mod fifo;

const PID_LINE: &[u8] = b"1234\n";

fn nonblocking_pipe() -> (File, File) {
    let mut ends: [libc::c_int; 2] = [0; 2];
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "pipe: {}", std::io::Error::last_os_error());
    let flags = unsafe { libc::fcntl(ends[0], libc::F_GETFL) };
    assert_eq!(unsafe { libc::fcntl(ends[0], libc::F_SETFL, flags | libc::O_NONBLOCK) }, 0, "set O_NONBLOCK");
    unsafe { (File::from_raw_fd(ends[0]), File::from_raw_fd(ends[1])) }
}

fn descriptor(file: &File) -> RawFd {
    std::os::fd::AsRawFd::as_raw_fd(file)
}

#[test]
fn read_line_returns_the_pid_while_the_writer_is_still_open() {
    let (reader, mut writer) = nonblocking_pipe();
    writer.write_all(PID_LINE).expect("the writer writes the pid line");

    let line = fifo::read_line(descriptor(&reader), "the pid line never arrived");

    assert_eq!(line.trim().parse::<u32>().expect("the line is a pid"), 1234);
    drop(writer);
}

#[test]
fn read_to_string_fails_with_would_block_while_the_writer_is_still_open() {
    let (mut reader, mut writer) = nonblocking_pipe();
    writer.write_all(PID_LINE).expect("the writer writes the pid line");
    fifo::await_readable(descriptor(&reader), "the pid line never arrived");

    let mut text = String::new();
    let failure = reader.read_to_string(&mut text).expect_err("the open writer leaves nothing more to read");

    assert_eq!(failure.kind(), ErrorKind::WouldBlock);
    drop(writer);
}
