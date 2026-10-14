use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::string::ToString;

use memchr::memmem;

use crate::edge_signal_unix::{abandon_slot, release_child, reserve_slot, track_child};
use crate::edge_spawn_unix::{Child, spawn};
use crate::stack_capture::{Unavailable, append};
use crate::stack_frames::Format;

/// The format [`capture`] writes into the buffer.
pub const FORMAT: Format = Format::Sample;

const SAMPLE: &str = "/usr/bin/sample";
const GRAPH_START: &[u8] = b"Call graph:";
const GRAPH_END: &[u8] = b"\nTotal number in stack";
const DRAIN_BYTES: usize = 512;

/// Captures every thread stack of `pid` with `/usr/bin/sample <pid> 1 -mayDie -file <path>` into `buffer`.
///
/// Cold path: it spawns the tool through [`spawn`] (the `edge_spawn` path), blocks until the tool exits, reads its
/// output file, and keeps only the `Call graph:` section. It allocates (path, file bytes) and blocks for the sampling
/// second, so a driver runs it on a blocking worker, never on the event loop. Returns `(written, dropped)`: bytes now in
/// `buffer` and useful bytes that did not fit. The tool is a signal-table child: a slot is reserved before the spawn,
/// filled by the child, and released at reap, so an interrupt reaches `sample` like any other child. Parse the buffer with
/// [`frames`](crate::stack_frames::frames) using [`FORMAT`].
///
/// # Errors
///
/// Returns [`Unavailable`] when `sample` cannot start, fails, or prints no call graph.
pub fn capture(pid: u32, buffer: &mut [u8]) -> Result<(usize, usize), Unavailable> {
    let path = output_path(pid);
    let outcome = run_sample(pid, &path).and_then(|()| fs::read(&path).map_err(|error| io_failure("read sample output", &error)));
    let _ = fs::remove_file(&path);
    outcome.and_then(|bytes| keep_call_graph(&bytes, buffer))
}

fn output_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(std::format!("cargo-val-sample-{pid}-{}.txt", std::process::id()))
}

fn io_failure(operation: &'static str, error: &io::Error) -> Unavailable {
    Unavailable::Io { operation, os_code: error.raw_os_error().unwrap_or(libc::EIO) }
}

fn run_sample(pid: u32, path: &Path) -> Result<(), Unavailable> {
    let target = pid.to_string();
    let file = path.as_os_str().as_encoded_bytes();
    let arguments: [&[u8]; 5] = [target.as_bytes(), b"1", b"-mayDie", b"-file", file];
    reserve_slot().map_err(|os_code| Unavailable::Io { operation: "reserve signal slot", os_code })?;
    let child = spawn(SAMPLE.as_bytes(), &arguments, &[], &[], None).map_err(|error| {
        abandon_slot();
        Unavailable::NoTool { tool: SAMPLE, os_code: error.os_code }
    })?;
    let tracked = track_child(child.process_id());
    drain(&child);
    let status = wait_status(&child);
    release_child(child.process_id());
    tracked.map_err(|os_code| Unavailable::Io { operation: "track sample child", os_code })?;
    match status {
        0 => Ok(()),
        status => Err(Unavailable::ToolFailed { tool: SAMPLE, status }),
    }
}

fn drain(child: &Child) {
    let mut watched = [
        libc::pollfd { fd: child.stdout().raw(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: child.stderr().raw(), events: libc::POLLIN, revents: 0 },
    ];
    let mut scratch = [0_u8; DRAIN_BYTES];
    while watched.iter().any(|entry| entry.fd >= 0) {
        // safety: watched is a live array of two pollfd and the length passed is its length.
        if unsafe { libc::poll(watched.as_mut_ptr(), 2, -1) } < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
        for entry in watched.iter_mut().filter(|entry| entry.fd >= 0 && entry.revents != 0) {
            // safety: the descriptor is an open pipe end owned by the child and scratch is writable for its length.
            let count = unsafe { libc::read(entry.fd, scratch.as_mut_ptr().cast(), scratch.len()) };
            let again = count < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock;
            if count <= 0 && !again {
                entry.fd = -1;
            }
        }
    }
}

fn wait_status(child: &Child) -> i32 {
    let mut status: libc::c_int = 0;
    let process = libc::pid_t::try_from(child.process_id()).unwrap_or(0);
    // safety: waitpid takes a plain pid and a valid status pointer.
    if unsafe { libc::waitpid(process, &raw mut status, 0) } < 0 {
        return -1;
    }
    if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) }
}

fn keep_call_graph(bytes: &[u8], buffer: &mut [u8]) -> Result<(usize, usize), Unavailable> {
    let Some(start) = memmem::find(bytes, GRAPH_START) else {
        return Err(Unavailable::Empty { source: SAMPLE });
    };
    let end = memmem::find(&bytes[start..], GRAPH_END).map_or(bytes.len(), |offset| start + offset);
    let mut written = 0;
    let dropped = append(buffer, &mut written, &bytes[start..end]);
    Ok((written, dropped))
}

