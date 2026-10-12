//! Spawning `rustc --print target-libdir` without waiting for it.

use core::ffi::{CStr, c_char};

use super::unix_spawn::{UnixChild, spawn_raw_unix};

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

/// Spawns `rustc --print target-libdir [--target <triple>]` without waiting for it.
///
/// `rustc` is the `RUSTC` environment value when present, otherwise the name
/// resolved through `PATH`. The printed directory holds `libstd-*.dylib`, which
/// proc-macro test binaries load.
///
/// # Errors
///
/// Returns [`RustcLibdirError::Arguments`] or [`RustcLibdirError::Spawn`].
pub fn spawn_libdir_query(
    rustc: Option<&[u8]>,
    triple: Option<&[u8]>,
    envp: &[*const c_char],
) -> Result<UnixChild, RustcLibdirError> {
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
    spawn_raw_unix(rustc_path, &argv[..=argument_count], envp, None, true)
        .map_err(|_| RustcLibdirError::Spawn)
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
