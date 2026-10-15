// RUSTC_WRAPPER for fixture builds: cargo creates each child pipe and sets CLOEXEC in two steps, so a rustc forked in between inherits a sibling's pipe and holds its EOF open; this wrapper closes every inherited fd above stderr before the real rustc runs
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::process::{Command, ExitCode};

#[cfg(unix)]
const FIRST_INHERITED_FD: i32 = 3;
#[cfg(unix)]
const JOBSERVER_FLAGS: [&str; 2] = ["--jobserver-auth=", "--jobserver-fds="];

#[cfg(unix)]
fn jobserver_fds(makeflags: &str) -> Vec<i32> {
    makeflags
        .split_whitespace()
        .filter_map(|word| JOBSERVER_FLAGS.iter().find_map(|flag| word.strip_prefix(flag)))
        .filter_map(|value| value.split_once(','))
        .filter_map(|(read, write)| Some([read.parse().ok()?, write.parse().ok()?]))
        .flatten()
        .collect()
}

#[cfg(unix)]
fn kept_fds() -> Vec<i32> {
    ["CARGO_MAKEFLAGS", "MAKEFLAGS"].iter().filter_map(|name| std::env::var(name).ok()).flat_map(|flags| jobserver_fds(&flags)).collect()
}

#[cfg(unix)]
fn open_fds() -> Vec<i32> {
    std::fs::read_dir("/dev/fd")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

#[cfg(unix)]
fn close_inherited(keep: &[i32]) {
    for descriptor in open_fds().into_iter().filter(|descriptor| *descriptor >= FIRST_INHERITED_FD && !keep.contains(descriptor)) {
        // the snapshot already dropped the read_dir handle, so its own fd number is closed twice at worst and close returns EBADF, which is ignored; a raw close avoids OwnedFd's debug abort on EBADF
        unsafe { libc::close(descriptor) };
    }
}

#[cfg(unix)]
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some((rustc, rest)) = arguments.split_first() else {
        eprintln!("fd-scrub-rustc: no rustc path given");
        return ExitCode::from(2);
    };
    close_inherited(&kept_fds());
    let failure = Command::new(rustc).args(rest).exec();
    eprintln!("fd-scrub-rustc: exec {rustc} failed: {failure}");
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("fd-scrub-rustc: unix only");
    std::process::ExitCode::from(2)
}

#[cfg(all(test, unix))]
mod tests {
    use super::jobserver_fds;

    #[proxima::test]
    async fn jobserver_fds_parse_the_pipe_forms_cargo_and_make_pass() {
        let cases: [(&str, Vec<i32>); 5] = [
            ("-j --jobserver-fds=7,8 --jobserver-auth=7,8", vec![7, 8, 7, 8]),
            ("-j4 --jobserver-auth=12,13", vec![12, 13]),
            ("--jobserver-auth=fifo:/tmp/GMfifo1234", vec![]),
            ("-j --jobserver-auth=7", vec![]),
            ("", vec![]),
        ];
        for (flags, expected) in cases {
            assert_eq!(jobserver_fds(flags), expected, "flags {flags:?}");
        }
    }
}
