#[cfg(all(
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "edge-os",
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod unix_driver;
#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
pub mod unix_env;
#[cfg(all(unix, feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod unix_pipe_pair;
#[cfg(all(
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod unix_spawn;
#[cfg(all(
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod unix_rustc;

#[cfg(all(
    target_os = "linux",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod linux_exit;

#[cfg(all(
    target_os = "macos",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod macos_exit;

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
