use std::io;
use std::os::unix::process::CommandExt;
use std::panic::{self, AssertUnwindSafe};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use crate::common::{INTERRUPT_GRACE, interrupt, kill_group};

/// Spawns `command` as the leader of its own process group, lends the caller the child's pid, its piped stdout and stderr (if the command piped them) and a receiver that yields the exit status once, then guarantees the group is gone before returning or unwinding.
///
/// Where [`crate::common::output_until`] owns the whole run, this is for tests that must watch the child while it runs. A caller panic, a missed deadline inside `interact`, or a child left running all end the same way: SIGINT to the group, `INTERRUPT_GRACE` for cargo-val to forward it, then SIGKILL, so no cargo or cargo-val child outlives the test and holds cargo's build lock.
pub fn supervised<Outcome>(
    command: &mut Command,
    interact: impl FnOnce(u32, Option<ChildStdout>, Option<ChildStderr>, &Receiver<io::Result<ExitStatus>>) -> Outcome,
) -> Outcome {
    command.process_group(0);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("{command:?} did not spawn: {error}"));
    let leader = child.id();
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || sender.send(child.wait()));
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| interact(leader, stdout, stderr, &receiver)));
    let failures = settle(leader, &receiver);
    match outcome {
        Ok(value) => {
            assert!(failures.is_empty(), "{command:?} (pid {leader}) group cleanup failed: {failures:?}");
            value
        }
        Err(payload) => panic::resume_unwind(payload),
    }
}

// Disconnected means the exit status was already taken; Empty means the child is still running
fn settle(leader: u32, receiver: &Receiver<io::Result<ExitStatus>>) -> Vec<String> {
    let mut failures = Vec::new();
    if matches!(receiver.try_recv(), Err(TryRecvError::Empty)) {
        failures.extend(interrupt(leader).err());
        if receiver.recv_timeout(INTERRUPT_GRACE).is_err() {
            failures.extend(kill_group(leader).err());
            let _ = receiver.recv();
        }
    }
    failures.extend(kill_group(leader).err());
    failures
}
