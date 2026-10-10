#[cfg(all(unix, feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod unix_pipe_pair;
#[cfg(all(
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod unix_spawn;
