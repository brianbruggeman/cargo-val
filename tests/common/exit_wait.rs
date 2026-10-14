use core::future::Future;
use core::pin::pin;

use cargo_val::edge_stream_unix::ChildStream;
use futures::FutureExt;
use futures::future::{Either, select};
use proxima_primitives::pipe::{Exhausted, Pipe};

#[cfg(target_os = "linux")]
pub use cargo_val::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
pub use cargo_val::edge_exit_macos::MacosExit as ExitSource;

/// Awaits `wait` (a ready line or a fifo read the test itself holds both ends of, so it can never see EOF) but ends it the moment `exit` reports the child gone.
///
/// A line already buffered when the child exits is still returned; otherwise this panics naming `what`, the exit status, and everything the labelled `streams` retained.
///
/// # Panics
///
/// When the child exits before `wait` completes.
pub async fn until_exit<Wait: Future>(wait: Wait, what: &str, exit: &ExitSource, streams: &[(&str, &ChildStream)]) -> Wait::Output {
    let mut wait = pin!(wait);
    let mut gone = pin!(Pipe::call(exit, ()));
    let status = match select(wait.as_mut(), gone.as_mut()).await {
        Either::Left((ready, _)) => return ready,
        Either::Right((status, _)) => status,
    };
    if let Some(ready) = wait.now_or_never() {
        return ready;
    }
    let retained: String = streams
        .iter()
        .fold(String::new(), |report, (name, stream)| {
            read_available(stream);
            report + &format!("\n{name} so far:\n{}", String::from_utf8_lossy(&stream.retained()))
        });
    let status = match status {
        Ok(Ok(status)) => status.to_string(),
        Ok(Err(error)) => format!("unreadable ({error})"),
        Err(Exhausted) => "already collected".to_owned(),
    };
    panic!("{what}: the child exited before it got there, {status}{retained}");
}

// takes only what is buffered now: a grandchild holding the pipe open must not turn the failure report into a second hang
fn read_available(stream: &ChildStream) {
    while let Some(Ok(Ok(_))) = Pipe::call(stream, ()).now_or_never() {}
}
