use core::future::Future;
use core::time::Duration;
use std::process::{Command, Output};

use prime::os::core_shard::run_on_this_thread_as;
use prime::os::reactor::Hangup;
use proxima_runtime::CoreId;

use crate::common::output_within;

/// Drives `root` to completion on the calling thread as a prime root, with the reactor waking on child-pipe hangup; the examples are sync `main`s, tests run under `#[proxima::test]` instead.
pub fn block_on<Root>(root: Root) -> Root::Output
where
    Root: Future + 'static,
    Root::Output: 'static,
{
    run_on_this_thread_as::<Hangup, Root>(CoreId(0), root)
        .expect("the prime root runs to completion")
}

/// [`output_within`] for a sync `main`: the command moves into the root future, and `setting` names the `CARGO_VAL_TEST_<setting>` bound that `deadline` came from.
///
/// # Errors
///
/// As [`output_within`].
pub fn output_blocking(
    mut command: Command,
    deadline: Duration,
    setting: &'static str,
) -> Result<Output, String> {
    block_on(async move { output_within(&mut command, deadline, setting).await })
}
