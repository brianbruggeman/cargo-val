#[cfg(target_os = "windows")]
pub mod windows_handles;

#[cfg(target_os = "windows")]
pub mod windows_pipe_pair;

#[cfg(all(
    target_os = "windows",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod windows_pipe;

#[cfg(target_os = "windows")]
pub mod windows_spawn;
