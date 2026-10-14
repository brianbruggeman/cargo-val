use std::process::{Child, ExitStatus};
use std::time::Duration;

use super::group::watchdog;

/// Waits for `child` (which must lead its own group, see [`lead_group`]); past `deadline` its whole group is killed and the wait fails.
///
/// # Errors
///
/// Returns the wait failure, or the deadline report when the group had to be killed.
pub fn wait_bounded(child: &mut Child, deadline: Duration) -> Result<ExitStatus, String> {
    let pid = child.id();
    let (cancel, fired) = watchdog(pid, deadline);
    let waited = child.wait();
    drop(cancel);
    if fired.join().unwrap_or(false) {
        return Err(format!("pid {pid} outlived {deadline:?} and its process group was killed"));
    }
    waited.map_err(|error| format!("wait for pid {pid} failed: {error}"))
}
