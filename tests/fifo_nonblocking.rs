#![cfg(unix)]

use core::future::poll_fn;
use core::pin::pin;
use core::task::Poll;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};

use cargo_val::edge_stream_unix::ChildStream;
use proxima_primitives::pipe::Pipe;

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

#[proxima::test(timeout = "310s")]
async fn first_line_returns_the_pid_while_the_writer_is_still_open() {
    let (reader, mut writer) = nonblocking_pipe();
    writer.write_all(PID_LINE).expect("the writer writes the pid line");
    let stream = ChildStream::new(OwnedFd::from(reader));

    let line = fifo::first_line(&stream, "the pid line never arrived").await;

    assert_eq!(line.trim().parse::<u32>().expect("the line is a pid"), 1234);
    drop(writer);
}

#[proxima::test(timeout = "310s")]
async fn the_stream_stays_pending_while_the_writer_is_open_and_ends_when_it_closes() {
    let (reader, mut writer) = nonblocking_pipe();
    writer.write_all(PID_LINE).expect("the writer writes the pid line");
    let stream = ChildStream::new(OwnedFd::from(reader));
    fifo::first_line(&stream, "the pid line never arrived").await;

    let mut next = pin!(stream.call(()));
    let polled = poll_fn(|context| Poll::Ready(next.as_mut().poll(context))).await;
    assert!(polled.is_pending(), "the drained stream with an open writer must park, not fail with WouldBlock");

    drop(writer);
    fifo::until_eof(&stream, "the stream must end once the writer closes").await;
}

#[test]
fn read_to_string_fails_with_would_block_while_the_writer_is_still_open() {
    let (mut reader, mut writer) = nonblocking_pipe();
    writer.write_all(PID_LINE).expect("the writer writes the pid line");

    let mut text = String::new();
    let failure = reader.read_to_string(&mut text).expect_err("the open writer leaves nothing more to read");

    assert_eq!(failure.kind(), ErrorKind::WouldBlock);
    drop(writer);
}
