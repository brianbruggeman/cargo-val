use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::task::{Context, Poll};
use std::io;
use std::os::windows::io::{AsHandle, BorrowedHandle, OwnedHandle};
use std::process::ExitStatus;

use prime::os::child;
use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, Pipe, UnpinPipe};

/// Source pipe (`() -> io::Result<ExitStatus>`) for one child: a prime gate over the process handle, then one exit-code read.
///
/// Yields the exit once, then `Err(Exhausted)`. The status carries the child's own exit code: a crash
/// reports its NTSTATUS (for example `0xC0000005`) through `ExitStatus::code`, which wraps to a negative
/// `i32`, so it never equals an ordinary exit code. Gate and spent flag live here, never in the call future,
/// so dropping a pending call loses nothing. Built on `prime::os::readiness::Readiness::signaled` and
/// `prime::os::child::exit_status`; merge several with `proxima_primitives::pipe::FanIn`.
pub struct WindowsExit {
    gate: RefCell<Readiness>,
    process: OwnedHandle,
    spent: Cell<bool>,
}

impl WindowsExit {
    /// Gates `process`, which stays open here until the exit is read.
    ///
    /// # Errors
    ///
    /// Returns the `DuplicateHandle` error when the process handle cannot be duplicated.
    pub fn new(process: BorrowedHandle<'_>) -> io::Result<Self> {
        let process = process.try_clone_to_owned()?;
        Ok(Self {
            gate: RefCell::new(Readiness::signaled(&process)?),
            process,
            spent: Cell::new(false),
        })
    }

    fn advance(&self, context: &Context<'_>) -> Poll<Result<io::Result<ExitStatus>, Exhausted>> {
        if self.spent.get() {
            return Poll::Ready(Err(Exhausted));
        }
        self.poll_gate(context).map(|exit| {
            self.spent.set(true);
            Ok(exit)
        })
    }

    fn poll_gate(&self, context: &Context<'_>) -> Poll<io::Result<ExitStatus>> {
        loop {
            let state = self.gate.borrow_mut().poll(context);
            match state {
                Ok(ReadyState::Retry) => match child::exit_status(self.process.as_handle()) {
                    Ok(status) => return Poll::Ready(Ok(status)),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Poll::Ready(Err(error)),
                },
                Ok(ReadyState::Parked) => return Poll::Pending,
                Ok(ReadyState::OffWorker) => {
                    return Poll::Ready(Err(io::Error::other(
                        "exit gate polled off a prime worker",
                    )));
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

impl DropSafe for WindowsExit {}

impl UnpinPipe for WindowsExit {
    type In = ();
    type Out = io::Result<ExitStatus>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> + Unpin {
        poll_fn(move |context| self.advance(context))
    }
}

impl Pipe for WindowsExit {
    type In = ();
    type Out = io::Result<ExitStatus>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> {
        UnpinPipe::call(self, ())
    }
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::pin::Pin;
    use core::task::Poll;
    use std::os::windows::io::AsHandle;
    use std::process::Command;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::{Exhausted, UnpinPipe};
    use proxima_runtime::CoreId;

    use super::WindowsExit;

    fn exit_code_of(command: &mut Command) -> Option<i32> {
        let mut child = command.spawn().expect("fixture command spawns");
        let exit = WindowsExit::new(child.as_handle()).expect("process handle gates");
        let status = run_on_this_thread(CoreId(0), async move {
            let mut call = UnpinPipe::call(&exit, ());
            let status = poll_fn(|context| Pin::new(&mut call).poll(context))
                .await
                .expect("exit source reports once")
                .expect("exit status reads");
            let second = poll_fn(|context| {
                let mut again = UnpinPipe::call(&exit, ());
                Poll::Ready(Pin::new(&mut again).poll(context))
            })
            .await;
            assert!(matches!(second, Poll::Ready(Err(Exhausted))), "the exit is yielded once");
            status
        })
        .expect("root completes");
        child.wait().expect("child is reaped");
        status.code()
    }

    #[test]
    fn exit_code_seven_is_reported_through_the_process_gate() {
        let code = exit_code_of(Command::new("cmd").args(["/C", "exit 7"]));

        assert_eq!(code, Some(7));
    }

    #[test]
    fn a_crash_ntstatus_stays_distinct_from_an_ordinary_exit_code() {
        let crash = exit_code_of(Command::new("powershell").args([
            "-NoProfile",
            "-Command",
            "exit -1073741819",
        ]));

        assert_eq!(crash, Some(0xC000_0005_u32.cast_signed()));
        assert_ne!(crash, Some(1));
    }
}
