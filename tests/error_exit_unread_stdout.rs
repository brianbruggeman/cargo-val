#![cfg(unix)]

use core::num::NonZeroI32;
use std::fs;
use std::io::PipeWriter;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;

#[path = "common/binary.rs"]
mod binary;
#[path = "common/group.rs"]
mod group;

#[cfg(target_os = "linux")]
use cargo_val::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
use cargo_val::edge_exit_macos::MacosExit as ExitSource;
use proxima_primitives::pipe::Pipe;

const FILL_CHUNK: [u8; 4096] = [b'x'; 4096];

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from)
}

fn set_flags(descriptor: libc::c_int, nonblocking: bool) {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    let wanted = if nonblocking { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_SETFL, wanted) }, 0, "descriptor flags change");
}

// leaves the pipe with no room, then restores blocking mode so the child's write waits as it would on any unread pipe
fn fill_until_full(writer: &PipeWriter) {
    set_flags(writer.as_raw_fd(), true);
    while unsafe { libc::write(writer.as_raw_fd(), FILL_CHUNK.as_ptr().cast(), FILL_CHUNK.len()) } >= 0 {}
    set_flags(writer.as_raw_fd(), false);
}

#[proxima::test(timeout = "310s")]
async fn an_error_exit_with_stdout_on_an_unread_pipe_exits_within_the_bound_and_leaves_no_process_group() {
    let directory = target_directory().join("error-exit").join(format!("run-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("run directory is creatable");
    let (_unread, writer) = std::io::pipe().expect("stdout pipe is creatable");
    fill_until_full(&writer);
    let stderr_path = directory.join("stderr.log");
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build_error/Cargo.toml");
    let spawned = binary::command()
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--lib")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-fixture"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(fs::File::create(&stderr_path).expect("stderr log is creatable"))
        .process_group(0)
        .spawn()
        .expect("cargo-val is spawnable")
        .id();
    let leader = libc::pid_t::try_from(spawned).expect("a pid fits pid_t");
    let exit = ExitSource::new(NonZeroI32::new(leader).expect("a spawned child has a nonzero pid"));

    let status = group::bounded(spawned, group::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE", None, Pipe::call(&exit, ()))
        .await
        .unwrap_or_else(|(report, _)| panic!("cargo-val still ran after an error exit with stdout on an unread pipe: {report}"))
        .expect("the exit source is unspent")
        .expect("the exit status is readable");
    assert_eq!(status.code(), Some(cargo_val::driver::BUILD_FAILED_EXIT_CODE), "the build failure ends the run");
    let stderr = fs::read_to_string(&stderr_path).expect("stderr log is readable");
    assert!(stderr.contains("cargo-val: abandoned "), "the dropped status records are counted on stderr:\n{stderr}");
    let probe = unsafe { libc::killpg(leader, 0) };
    assert_eq!((probe, std::io::Error::last_os_error().raw_os_error()), (-1, Some(libc::ESRCH)), "no process of the run's group remains");
}
