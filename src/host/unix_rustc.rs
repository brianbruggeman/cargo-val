//! Library directory lookup through `rustc --print target-libdir`.

use core::ffi::{CStr, c_char, c_int};

use super::unix_pipe_pair::OwnedDescriptor;
use super::unix_spawn::spawn_raw_unix;

const POLL_TIMEOUT_MILLISECONDS: c_int = 1000;
const DRAIN_CAPACITY: usize = 512;

/// Failures while asking rustc where its libraries live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RustcLibdirError {
    /// The rustc path or target triple contains an interior NUL byte or exceeds storage.
    Arguments,
    /// `posix_spawnp` could not start rustc.
    Spawn,
    /// Waiting on or reading rustc's pipes failed.
    Io,
    /// rustc exited unsuccessfully, wrote nothing, or wrote more than the output storage.
    Output,
}

/// A spawned `rustc --print target-libdir` whose output has not been collected yet.
///
/// Dropping it before [`RustcLibdirQuery::finish`] kills and reaps the child.
pub struct RustcLibdirQuery {
    unreaped: Option<libc::pid_t>,
    stdout: OwnedDescriptor,
    stderr: OwnedDescriptor,
}

impl RustcLibdirQuery {
    /// Spawns `rustc --print target-libdir [--target <triple>]` without waiting for it.
    ///
    /// `rustc` is the `RUSTC` environment value when present, otherwise the name
    /// resolved through `PATH`.
    ///
    /// # Errors
    ///
    /// Returns [`RustcLibdirError::Arguments`] or [`RustcLibdirError::Spawn`].
    pub fn start(
        rustc: Option<&[u8]>,
        triple: Option<&[u8]>,
        envp: &[*const c_char],
    ) -> Result<Self, RustcLibdirError> {
        let mut rustc_bytes = [0_u8; 1024];
        let rustc_path = c_string(rustc.unwrap_or(b"rustc"), &mut rustc_bytes)?;
        let mut triple_bytes = [0_u8; 256];
        let triple_text = match triple {
            Some(triple) => Some(c_string(triple, &mut triple_bytes)?),
            None => None,
        };
        let mut argv = [core::ptr::null(); 6];
        argv[0] = rustc_path.as_ptr();
        argv[1] = c"--print".as_ptr();
        argv[2] = c"target-libdir".as_ptr();
        let argument_count = match triple_text {
            Some(triple_text) => {
                argv[3] = c"--target".as_ptr();
                argv[4] = triple_text.as_ptr();
                5
            }
            None => 3,
        };
        Self::spawn(rustc_path, &argv[..=argument_count], envp)
    }

    fn spawn(
        program: &CStr,
        argv: &[*const c_char],
        envp: &[*const c_char],
    ) -> Result<Self, RustcLibdirError> {
        let child = spawn_raw_unix(program, argv, envp, None, true)
            .map_err(|_| RustcLibdirError::Spawn)?;
        let (process_id, stdout, stderr) = child.into_parts();
        Ok(Self {
            unreaped: Some(process_id),
            stdout,
            stderr,
        })
    }

    /// Reads the printed directory into `output` and reaps rustc.
    ///
    /// # Errors
    ///
    /// Returns [`RustcLibdirError`] naming the stage that failed.
    pub fn finish(mut self, output: &mut [u8]) -> Result<usize, RustcLibdirError> {
        let length = read_until_eof(self.stdout.raw(), self.stderr.raw(), output)?;
        let process_id = self.unreaped.take().ok_or(RustcLibdirError::Io)?;
        let status = wait_for_exit(process_id)?;
        if status != 0 {
            return Err(RustcLibdirError::Output);
        }
        let mut end = length;
        while end > 0 && matches!(output[end - 1], b'\n' | b'\r') {
            end -= 1;
        }
        if end == 0 {
            return Err(RustcLibdirError::Output);
        }
        Ok(end)
    }
}

impl Drop for RustcLibdirQuery {
    fn drop(&mut self) {
        if let Some(process_id) = self.unreaped.take() {
            unsafe { libc::kill(process_id, libc::SIGKILL) };
            let _ = wait_for_exit(process_id);
        }
    }
}

/// Runs `rustc --print target-libdir [--target <triple>]` and copies its stdout into `output`.
///
/// This is how nextest finds the sysroot (`rustc_cli.rs`): the printed
/// directory holds `libstd-*.dylib`, which proc-macro test binaries load.
///
/// # Errors
///
/// Returns [`RustcLibdirError`] naming the stage that failed.
pub fn rustc_target_libdir(
    rustc: Option<&[u8]>,
    triple: Option<&[u8]>,
    envp: &[*const c_char],
    output: &mut [u8],
) -> Result<usize, RustcLibdirError> {
    RustcLibdirQuery::start(rustc, triple, envp)?.finish(output)
}

fn c_string<'storage>(
    bytes: &[u8],
    storage: &'storage mut [u8],
) -> Result<&'storage CStr, RustcLibdirError> {
    let target = storage
        .get_mut(..=bytes.len())
        .ok_or(RustcLibdirError::Arguments)?;
    target[..bytes.len()].copy_from_slice(bytes);
    target[bytes.len()] = 0;
    CStr::from_bytes_with_nul(target).map_err(|_| RustcLibdirError::Arguments)
}

fn read_until_eof(
    stdout: c_int,
    stderr: c_int,
    output: &mut [u8],
) -> Result<usize, RustcLibdirError> {
    let mut length = 0;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut drained = [0_u8; DRAIN_CAPACITY];
    while stdout_open || stderr_open {
        let mut descriptors = [
            libc::pollfd {
                fd: if stdout_open { stdout } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_open { stderr } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, POLL_TIMEOUT_MILLISECONDS) };
        if ready < 0 {
            if last_error() == libc::EINTR {
                continue;
            }
            return Err(RustcLibdirError::Io);
        }
        if descriptors[0].revents != 0 {
            let target = &mut output[length..];
            match read_once(stdout, target)? {
                Some(count) => length += count,
                None => stdout_open = false,
            }
            if stdout_open && length == output.len() {
                return Err(RustcLibdirError::Output);
            }
        }
        if descriptors[1].revents != 0 && read_once(stderr, &mut drained)?.is_none() {
            stderr_open = false;
        }
    }
    Ok(length)
}

fn read_once(descriptor: c_int, target: &mut [u8]) -> Result<Option<usize>, RustcLibdirError> {
    let count = unsafe { libc::read(descriptor, target.as_mut_ptr().cast(), target.len()) };
    match usize::try_from(count) {
        Ok(0) => Ok(None),
        Ok(count) => Ok(Some(count)),
        Err(_) => match last_error() {
            libc::EINTR | libc::EAGAIN => Ok(Some(0)),
            _ => Err(RustcLibdirError::Io),
        },
    }
}

fn wait_for_exit(process_id: libc::pid_t) -> Result<c_int, RustcLibdirError> {
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(process_id, &raw mut status, 0) };
        if waited == process_id {
            return Ok(if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                -1
            });
        }
        if waited < 0 && last_error() != libc::EINTR {
            return Err(RustcLibdirError::Io);
        }
    }
}

#[cfg(target_os = "macos")]
fn last_error() -> c_int {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "linux")]
fn last_error() -> c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{RustcLibdirError, RustcLibdirQuery, rustc_target_libdir};

    fn inherited() -> std::vec::Vec<std::ffi::CString> {
        std::env::vars_os()
            .filter_map(|(key, value)| {
                let mut bytes = key.as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_bytes());
                std::ffi::CString::new(bytes).ok()
            })
            .collect()
    }

    #[test]
    fn host_libdir_is_the_directory_that_holds_libstd() {
        let entries = inherited();
        let mut pointers: std::vec::Vec<*const core::ffi::c_char> =
            entries.iter().map(|entry| entry.as_ptr()).collect();
        pointers.push(core::ptr::null());
        let mut output = [0_u8; 1024];

        let length = rustc_target_libdir(None, None, &pointers, &mut output)
            .expect("rustc prints its target libdir");

        let libdir = std::str::from_utf8(&output[..length]).expect("libdir is utf-8");
        let has_libstd = std::fs::read_dir(Path::new(libdir))
            .expect("printed libdir exists")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with("libstd-"));
        assert!(has_libstd, "no libstd-* in {libdir}");
    }

    #[test]
    fn start_returns_while_rustc_is_still_running_and_finish_collects_its_output() {
        let directory = crate::test_dir::tempdir().expect("scratch directory");
        let gate = directory.path().join("gate");
        let gate_c = std::ffi::CString::new(gate.as_os_str().as_bytes()).expect("gate path");
        assert_eq!(unsafe { libc::mkfifo(gate_c.as_ptr(), 0o600) }, 0, "mkfifo");
        let entries = inherited();
        let mut pointers: std::vec::Vec<*const core::ffi::c_char> =
            entries.iter().map(|entry| entry.as_ptr()).collect();
        pointers.push(core::ptr::null());
        // exec of a freshly written file is held by the OS for 16-67 s under load, so the
        // fake rustc is the installed /bin/sh, which never pays that first-exec assessment.
        let argv = [
            c"sh".as_ptr(),
            c"-c".as_ptr(),
            c"read release < \"$1\"; echo /opt/fake-toolchain/lib".as_ptr(),
            c"sh".as_ptr(),
            gate_c.as_ptr(),
            core::ptr::null(),
        ];

        let query = RustcLibdirQuery::spawn(c"/bin/sh", &argv, &pointers)
            .expect("fake rustc spawns");

        let process_id = query.unreaped.expect("child is unreaped after start");
        let mut status = 0;
        let waited = unsafe { libc::waitpid(process_id, &raw mut status, libc::WNOHANG) };
        assert_eq!(waited, 0, "rustc was still running when start returned");
        let mut release = std::fs::OpenOptions::new()
            .write(true)
            .open(&gate)
            .expect("open gate for release");
        release.write_all(b"go\n").expect("release fake rustc");
        drop(release);
        let mut output = [0_u8; 1024];
        let length = query.finish(&mut output).expect("finish collects output");
        assert_eq!(&output[..length], b"/opt/fake-toolchain/lib");
    }

    #[test]
    fn unknown_target_is_reported_as_an_output_failure() {
        let entries = inherited();
        let mut pointers: std::vec::Vec<*const core::ffi::c_char> =
            entries.iter().map(|entry| entry.as_ptr()).collect();
        pointers.push(core::ptr::null());
        let mut output = [0_u8; 1024];

        let result = rustc_target_libdir(None, Some(b"not-a-real-triple"), &pointers, &mut output);

        assert_eq!(result, Err(RustcLibdirError::Output));
    }
}
