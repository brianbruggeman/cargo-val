#[cfg(all(
    target_os = "windows",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod windows_pipe;
