use std::num::NonZeroI32;
use std::process::{Child, ExitStatus};

#[cfg(target_os = "linux")]
use cargo_val::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
use cargo_val::edge_exit_macos::MacosExit as ExitSource;
use proxima_primitives::pipe::{Exhausted, Pipe};

use crate::group::{self, settings};

/// Waits for `child` (which must lead its own group, see [`crate::lead::lead_group`]) through its exit source; past the `CARGO_VAL_TEST_<setting>` bound its whole group is killed and the wait fails.
///
/// # Errors
///
/// Returns the wait failure, or the expiry report when the group had to be killed.
pub async fn wait_within(child: &Child, setting: &str) -> Result<ExitStatus, String> {
    let pid = child.id();
    let target = NonZeroI32::new(pid.cast_signed()).ok_or("a spawned child has a nonzero pid")?;
    let exit = ExitSource::new(target);
    match group::bounded(pid, settings::bound(setting), setting, None, exit.call(())).await {
        Ok(waited) => waited.map_err(|Exhausted| format!("pid {pid}: the exit source was already spent"))?.map_err(|error| format!("wait for pid {pid} failed: {error}")),
        Err((report, _)) => Err(report),
    }
}
