use core::time::Duration;
use std::panic::{self, AssertUnwindSafe};
use std::process::Command;

use futures::FutureExt;
use proxima_core::time::timeout;
use proxima_primitives::pipe::UnpinPipe;

use crate::common::{ChildStream, ExitSource, interrupt_then_kill, kill_group, launch, release_group};

/// Spawns `command` as the leader of its own process group and lends `interact` the child's pid, its stdout and stderr streams and its exit source, all driven on the calling prime worker; then guarantees the group is gone before returning or unwinding.
///
/// Where [`crate::common::output_until`] owns the whole run, this is for tests that must watch the child while it runs. A caller panic, a `deadline` missed by `interact`, or a child left running all end the same way: SIGINT to the group, `INTERRUPT_GRACE` for cargo-val to forward it, then SIGKILL, so no cargo or cargo-val child outlives the test and holds cargo's build lock.
pub async fn supervised<Outcome>(
    command: &mut Command,
    deadline: Duration,
    interact: impl AsyncFnOnce(u32, &ChildStream, &ChildStream, &ExitSource) -> Outcome,
) -> Outcome {
    let (leader, stdout, stderr, exit) = launch(command).unwrap_or_else(|failure| panic!("{failure}"));
    let body = AssertUnwindSafe(interact(leader, &stdout, &stderr, &exit)).catch_unwind();
    let outcome = timeout(deadline, body).await;
    let failures = settle(leader, &exit).await;
    release_group(leader);
    match outcome {
        Ok(Ok(value)) => {
            assert!(failures.is_empty(), "{command:?} (pid {leader}) group cleanup failed: {failures:?}");
            value
        }
        Ok(Err(payload)) => panic::resume_unwind(payload),
        Err(elapsed) => panic!(
            "{command:?} (pid {leader}) outlived {deadline:?} ({elapsed:?}); group cleanup: {failures:?}\nstdout so far:\n{}\nstderr so far:\n{}",
            String::from_utf8_lossy(&stdout.retained()),
            String::from_utf8_lossy(&stderr.retained()),
        ),
    }
}

// a spent source or a ready status means the exit was already taken; only a pending one means the child still runs
async fn settle(leader: u32, exit: &ExitSource) -> Vec<String> {
    if UnpinPipe::call(exit, ()).now_or_never().is_some() {
        return kill_group(leader).err().into_iter().collect();
    }
    interrupt_then_kill(leader, UnpinPipe::call(exit, ())).await.1
}
