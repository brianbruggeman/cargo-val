#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "common/group.rs"]
mod group;
#[path = "common/output.rs"]
mod output;

// the child fills its stderr far past any pipe buffer, then replaces the shell with a process that never exits; exec keeps the leader pid
const STDERR_THEN_BLOCK: &str = r#"echo $$ > "$1"; head -c 1048576 /dev/zero >&2; exec sleep 600"#;

fn bounded_wait() -> Duration {
    group::settings::bound("BOUNDED_WAIT")
}

fn gone(target: libc::pid_t) -> bool {
    // safety: kill with signal 0 only probes for existence
    unsafe { libc::kill(target, 0) == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) }
}

fn blocked_child(pid_file: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", STDERR_THEN_BLOCK, "sh"]).arg(pid_file);
    command
}

#[test]
fn a_child_blocked_forever_behind_a_full_stderr_pipe_fails_within_its_bound_and_its_group_is_gone() {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("bounded-wait-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("the scratch directory is creatable");
    let pid_file = directory.join("leader.pid");
    let started = Instant::now();

    let failure = output::output_bounded(&mut blocked_child(&pid_file), bounded_wait()).expect_err("a child that never exits must not return output");

    let leader: libc::pid_t = fs::read_to_string(&pid_file).expect("the child recorded its pid").trim().parse().expect("the pid is a number");
    assert!(failure.contains("process group was killed"), "{failure}");
    assert!(started.elapsed() < bounded_wait() * 10, "the bound fired after {:?}", started.elapsed());
    assert!(gone(leader), "the leader {leader} still exists after the bound fired");
    assert!(gone(-leader), "the group {leader} still has members after the bound fired");
}

#[test]
fn a_child_that_exits_with_a_full_stderr_pipe_is_captured_whole() {
    let mut command = Command::new("sh");
    command.args(["-c", "head -c 1048576 /dev/zero >&2; echo done"]);

    let output = output::output_bounded(&mut command, group::sync_deadline()).expect("a child that exits within its bound returns its output");

    assert_eq!(output.stderr.len(), 1_048_576);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");
}
