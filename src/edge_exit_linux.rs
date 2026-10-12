use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::num::NonZeroI32;
use core::task::{Context, Poll};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::process::ExitStatus;

use prime::os::child;
use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, Pipe, UnpinPipe};

/// Source pipe (`() -> io::Result<ExitStatus>`) for one child: a prime gate on the child's pidfd, then one reap.
///
/// Yields the exit once, then `Err(Exhausted)`. Gate and spent flag live here, never in the call
/// future, so dropping a pending call loses nothing. Built on `prime::os::readiness::Readiness`
/// and `prime::os::child`; merge several with `proxima_primitives::pipe::FanIn`.
pub struct LinuxExit {
    pid: NonZeroI32,
    gate: RefCell<Option<(Readiness, OwnedFd)>>,
    spent: Cell<bool>,
}

impl LinuxExit {
    #[must_use]
    pub const fn new(pid: NonZeroI32) -> Self {
        Self {
            pid,
            gate: RefCell::new(None),
            spent: Cell::new(false),
        }
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
        let mut slot = self.gate.borrow_mut();
        let opened = match slot.take() {
            Some(opened) => opened,
            None => match child::open_pidfd(self.pid) {
                Ok(pidfd) => (Readiness::new(pidfd.as_raw_fd()), pidfd),
                Err(error) => return Poll::Ready(Err(error)),
            },
        };
        let (gate, pidfd) = slot.insert(opened);
        loop {
            match gate.poll(context) {
                Ok(ReadyState::Retry) => match child::try_reap_pidfd(pidfd.as_fd()) {
                    Ok(Some(status)) => return Poll::Ready(Ok(status)),
                    Ok(None) => {}
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

impl DropSafe for LinuxExit {}

impl UnpinPipe for LinuxExit {
    type In = ();
    type Out = io::Result<ExitStatus>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> + Unpin {
        poll_fn(move |context| self.advance(context))
    }
}

impl Pipe for LinuxExit {
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
    use core::num::NonZeroI32;
    use core::pin::Pin;
    use core::task::Poll;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::{Exhausted, FanIn, Select, UnpinPipe};
    use proxima_runtime::CoreId;

    use super::LinuxExit;
    use crate::edge_exit::fixture::{
        fork_child, in_single_threaded_process, wait_exit_without_reaping, zombie_is_released,
    };

    const FAST_CHILD_CODE: i32 = 7;
    const POLL_BOUND: u32 = 4;

    #[derive(Debug, Default)]
    struct Drive {
        polls: u32,
        pendings: u32,
        exhausted_after_exit: bool,
        threads_before: usize,
        threads_after: usize,
    }

    fn drive_exit(
        pid: NonZeroI32,
        on_first_pending: impl Fn() + 'static,
    ) -> (io::Result<ExitStatus>, Drive) {
        run_on_this_thread(CoreId(0), async move {
            let source = LinuxExit::new(pid);
            let mut drive = Drive {
                threads_before: thread_count(),
                ..Drive::default()
            };
            let event = poll_fn(|context| {
                drive.polls += 1;
                let mut call = UnpinPipe::call(&source, ());
                let polled = Pin::new(&mut call).poll(context);
                match polled {
                    Poll::Pending => {
                        drive.pendings += 1;
                        if drive.pendings == 1 {
                            on_first_pending();
                        }
                        Poll::Pending
                    }
                    Poll::Ready(item) => {
                        let mut again = UnpinPipe::call(&source, ());
                        drive.exhausted_after_exit = matches!(
                            Pin::new(&mut again).poll(context),
                            Poll::Ready(Err(Exhausted))
                        );
                        Poll::Ready(item.expect("first completed call yields the exit"))
                    }
                }
            })
            .await;
            drive.threads_after = thread_count();
            (event, drive)
        })
        .expect("root completes")
    }

    fn thread_count() -> usize {
        std::fs::read_to_string("/proc/self/status")
            .expect("read /proc/self/status")
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .expect("Threads line present")
            .trim()
            .parse()
            .expect("thread count parses")
    }

    fn exited(event: io::Result<ExitStatus>) -> ExitStatus {
        event.expect("exit edge yielded a status, not an io fault")
    }

    #[test]
    fn c39_wakes_for_exit_and_control_without_semaphore_mode() {
        in_single_threaded_process(|| {
            let held = fork_child(0, true);
            let pid = held.pid;

            let (event, drive) = drive_exit(pid, move || held.release());

            assert_eq!(exited(event).code(), Some(0), "held child exits cleanly");
            assert_eq!(drive.pendings, 1, "one park before release");
            assert_eq!(drive.polls, 2, "one parked poll then one exit poll");
            assert!(drive.exhausted_after_exit, "second call is Exhausted");
            assert_eq!(
                drive.threads_before, 1,
                "the forked process starts with one thread"
            );
            assert_eq!(drive.threads_after, 1, "no helper thread is spawned");
        });
    }

    #[test]
    fn c41_registration_race_control_observes_already_exited_child() {
        let finished = fork_child(FAST_CHILD_CODE, false);
        let pid = finished.pid;
        wait_exit_without_reaping(pid);

        let (event, drive) = drive_exit(pid, || {});

        assert_eq!(exited(event).code(), Some(FAST_CHILD_CODE));
        assert_eq!(
            drive.pendings, 0,
            "a readable pidfd resolves without parking"
        );
        assert!(drive.exhausted_after_exit, "second call is Exhausted");
        assert!(zombie_is_released(pid));
    }

    #[test]
    fn c59_exit_event_wakes_for_signal_and_reap_reports_signal_and_releases_zombie() {
        let held = fork_child(0, true);
        let pid = held.pid;

        let (event, drive) = drive_exit(pid, move || {
            assert_eq!(
                unsafe { libc::kill(pid.get(), libc::SIGKILL) },
                0,
                "kill child"
            );
        });

        let status = exited(event);
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(status.code(), None);
        assert_eq!(drive.pendings, 1);
        assert!(drive.exhausted_after_exit, "second call is Exhausted");
        assert!(zombie_is_released(pid));
    }

    #[test]
    fn exit_gate_never_hangs_on_exiting_child() {
        let finished = fork_child(FAST_CHILD_CODE, false);
        let pid = finished.pid;

        let (event, drive) = drive_exit(pid, || {});

        assert_eq!(exited(event).code(), Some(FAST_CHILD_CODE));
        assert!(
            drive.polls <= POLL_BOUND,
            "exit delivered within {POLL_BOUND} polls, took {}",
            drive.polls
        );
    }

    #[test]
    fn exit_sources_compose_into_fan_in_and_merge_both_statuses_then_exhaust() {
        let first = fork_child(3, false);
        let second = fork_child(5, false);
        let merged = FanIn::new(
            [LinuxExit::new(first.pid), LinuxExit::new(second.pid)],
            Select::RoundRobin,
        );

        let (codes, ended) = run_on_this_thread(CoreId(0), async move {
            let mut codes = Vec::new();
            loop {
                let item = poll_fn(|context| {
                    let mut call = UnpinPipe::call(&merged, ());
                    Pin::new(&mut call).poll(context)
                })
                .await;
                match item {
                    Ok(status) => codes.push(status.expect("child status").code()),
                    Err(Exhausted) => break,
                }
            }
            let ended = poll_fn(|context| {
                let mut call = UnpinPipe::call(&merged, ());
                Pin::new(&mut call).poll(context)
            })
            .await;
            (codes, ended)
        })
        .expect("root completes");

        let mut sorted = codes.clone();
        sorted.sort();
        assert_eq!(sorted, [Some(3), Some(5)], "both children's statuses merged");
        assert!(matches!(ended, Err(Exhausted)), "merge stays Exhausted");
        assert!(zombie_is_released(first.pid));
        assert!(zombie_is_released(second.pid));
    }
}
