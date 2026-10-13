use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::num::NonZeroI32;
use core::task::{Context, Poll};
use std::io;
use std::process::ExitStatus;

use prime::os::child;
use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, Pipe, UnpinPipe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitPhase {
    Probing,
    Parked,
    Reported,
    Spent,
}

/// Source pipe (`() -> io::Result<ExitStatus>`) for one child: a prime `EVFILT_PROC` gate, then one reap.
///
/// Yields the exit once, then `Err(Exhausted)`. Gate and phase live here, never in the call future,
/// so dropping a pending call loses nothing. Built on `prime::os::readiness::Readiness` and
/// `prime::os::child`; merge several with `proxima_primitives::pipe::FanIn`.
pub struct MacosExit {
    gate: RefCell<Readiness>,
    pid: NonZeroI32,
    phase: Cell<ExitPhase>,
}

impl MacosExit {
    #[must_use]
    pub fn new(pid: NonZeroI32) -> Self {
        Self {
            gate: RefCell::new(Readiness::process_exit(pid)),
            pid,
            phase: Cell::new(ExitPhase::Probing),
        }
    }

    fn advance(&self, context: &Context<'_>) -> Poll<Result<io::Result<ExitStatus>, Exhausted>> {
        if self.phase.get() == ExitPhase::Spent {
            return Poll::Ready(Err(Exhausted));
        }
        self.poll_gate(context).map(|exit| {
            self.phase.set(ExitPhase::Spent);
            Ok(exit)
        })
    }

    fn poll_gate(&self, context: &Context<'_>) -> Poll<io::Result<ExitStatus>> {
        loop {
            let state = self.gate.borrow_mut().poll(context);
            match state {
                Ok(ReadyState::Retry) => match self.reap() {
                    Ok(Some(status)) => return Poll::Ready(Ok(status)),
                    Ok(None) => {}
                    Err(error) => return Poll::Ready(Err(error)),
                },
                Ok(ReadyState::Parked) => {
                    self.phase.set(ExitPhase::Parked);
                    return Poll::Pending;
                }
                Ok(ReadyState::OffWorker) => {
                    return Poll::Ready(Err(io::Error::other(
                        "exit gate polled off a prime worker",
                    )));
                }
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                    self.phase.set(ExitPhase::Reported);
                    return Poll::Ready(child::reap_reported(self.pid));
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }

    fn reap(&self) -> io::Result<Option<ExitStatus>> {
        match self.phase.get() {
            ExitPhase::Probing => child::try_reap(self.pid),
            ExitPhase::Parked | ExitPhase::Reported | ExitPhase::Spent => {
                self.phase.set(ExitPhase::Reported);
                child::reap_reported(self.pid).map(Some)
            }
        }
    }
}

impl DropSafe for MacosExit {}

impl UnpinPipe for MacosExit {
    type In = ();
    type Out = io::Result<ExitStatus>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> + Unpin {
        poll_fn(move |context| self.advance(context))
    }
}

impl Pipe for MacosExit {
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

    use prime::os::child::reap_reported;
    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::{Exhausted, FanIn, Select, UnpinPipe};
    use proxima_runtime::CoreId;

    use super::MacosExit;
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
            let source = MacosExit::new(pid);
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
        let mut info: libc::proc_taskinfo = unsafe { core::mem::zeroed() };
        let size = i32::try_from(size_of::<libc::proc_taskinfo>()).expect("taskinfo size fits");
        let written = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        assert_eq!(written, size, "proc_pidinfo returns a full proc_taskinfo");
        usize::try_from(info.pti_threadnum).expect("thread count is non-negative")
    }

    fn exited(event: io::Result<ExitStatus>) -> ExitStatus {
        event.expect("exit edge yielded a status, not an io fault")
    }

    #[proxima::test]
    async fn c41_wakes_for_child_exit_and_control_without_worker_threads() {
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

    #[proxima::test]
    async fn c41_registration_race_control_observes_already_exited_child() {
        let finished = fork_child(FAST_CHILD_CODE, false);
        let pid = finished.pid;
        wait_exit_without_reaping(pid);

        let (event, drive) = drive_exit(pid, || {});

        assert_eq!(exited(event).code(), Some(FAST_CHILD_CODE));
        assert_eq!(drive.pendings, 0, "a zombie resolves without parking");
        assert!(drive.exhausted_after_exit, "second call is Exhausted");
        assert!(zombie_is_released(pid));
    }

    #[proxima::test]
    async fn c97_watch_never_returns_an_unregistered_unreaped_child() {
        let held = fork_child(0, true);
        let pid = held.pid;
        assert_eq!(
            unsafe { libc::kill(pid.get(), libc::SIGKILL) },
            0,
            "kill child"
        );
        wait_exit_without_reaping(pid);

        let (event, drive) = drive_exit(pid, || {});

        assert_eq!(exited(event).signal(), Some(libc::SIGKILL));
        assert!(drive.exhausted_after_exit, "second call is Exhausted");
        assert!(zombie_is_released(pid), "the late source leaves no zombie");
    }

    #[proxima::test]
    async fn c97_watch_of_a_pid_that_is_not_our_child_errors_instead_of_parking() {
        let finished = fork_child(0, false);
        let pid = finished.pid;
        reap_reported(pid).expect("reap the child so the pid is no longer ours");

        let (event, drive) = drive_exit(pid, || {});

        let fault = event.expect_err("a pid that is not our child faults");
        assert_eq!(fault.raw_os_error(), Some(libc::ECHILD));
        assert_eq!(drive.pendings, 0, "an unwatchable pid never parks");
        assert!(drive.exhausted_after_exit, "a fault is the one item too");
    }

    #[proxima::test]
    async fn c59_exit_event_carries_signal_status_and_blocking_reap_releases_zombie() {
        let held = fork_child(0, true);
        let pid = held.pid;

        let (event, drive) = drive_exit(pid, move || {
            assert_eq!(
                unsafe { libc::kill(pid.get(), libc::SIGKILL) },
                0,
                "kill child"
            );
        });

        assert_eq!(exited(event).signal(), Some(libc::SIGKILL));
        assert!(drive.exhausted_after_exit, "second call is Exhausted");
        assert!(zombie_is_released(pid));
    }

    #[proxima::test]
    async fn c54_unregistered_exit_uses_bounded_wait_then_reap() {
        let held = fork_child(0, true);
        let pid = held.pid;
        held.release();
        wait_exit_without_reaping(pid);

        let (event, drive) = drive_exit(pid, || {});

        assert_eq!(exited(event).code(), Some(0));
        assert_eq!(drive.polls, 1, "the reap is bounded: one poll, no park");
        assert!(zombie_is_released(pid));
    }

    #[proxima::test]
    async fn exit_gate_never_hangs_on_exiting_child() {
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

    #[proxima::test]
    async fn exit_sources_compose_into_fan_in_and_merge_both_statuses_then_exhaust() {
        let first = fork_child(3, false);
        let second = fork_child(5, false);
        let merged = FanIn::new(
            [MacosExit::new(first.pid), MacosExit::new(second.pid)],
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
