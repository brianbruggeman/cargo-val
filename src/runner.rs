//! Single-owner exact child launch from one borrowed listing intent.

use core::ffi::{CStr, c_char, c_int};

use crate::child_completion::{ChildCompletion, ChildCompletionEvent, InvalidChildCompletionEvent};
use crate::edge_spawn::{UnixChild, UnixSpawnError, spawn_unix};
use crate::host_event::{HostEvent, StaleHostEvent};
use crate::outcomes::{OutcomeError, OutcomeLedger, StartError, TerminalOutcome};
use crate::reporter::{ReportError, write_report};
use crate::schedule::{SessionSlotId, SessionSlots};
use crate::test_intent::ExactTestIntent;
use crate::edge_args::{UnixArgumentsError, lower_unix_arguments};

/// One owned child and its identity retained while the listing chunk is reused.
#[derive(Debug)]
pub struct RunningUnixChild<const NAME_CAPACITY: usize> {
    child: UnixChild,
    name: [u8; NAME_CAPACITY],
    name_length: usize,
    target: usize,
    completion: ChildCompletion,
    exit_status: Option<i32>,
}

impl<const NAME_CAPACITY: usize> RunningUnixChild<NAME_CAPACITY> {
    /// Ordinal of the test binary this child was listed from.
    #[must_use]
    pub const fn target(&self) -> usize {
        self.target
    }

    /// Returns the child watched by the platform event source.
    #[must_use]
    pub const fn child(&self) -> &UnixChild {
        &self.child
    }

    /// Returns the selected name from fixed session storage.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_length]
    }

    /// Takes ownership when the child is retired from its slot.
    #[must_use]
    pub fn into_child(self) -> UnixChild {
        self.child
    }
}

/// Launch rejection, with a live child returned if insertion cannot complete.
#[derive(Debug)]
pub enum UnixLaunchError<const NAME_CAPACITY: usize> {
    /// No live-child slot is available before the OS launch.
    Full,
    /// The same test name is already live in this session table.
    Duplicate,
    /// Inline identity storage is too short for this test name.
    NameTooLong { required: usize, available: usize },
    /// Caller-owned argv or env storage could not represent the intent.
    Arguments(UnixArgumentsError),
    /// The OS rejected child creation.
    Spawn(UnixSpawnError),
    /// An unexpected slot rejection returns the spawned child to the owner.
    CapacityChanged(RunningUnixChild<NAME_CAPACITY>),
    /// Ledger admission failed without losing ownership of the spawned child.
    Accounting(StartError<RunningUnixChild<NAME_CAPACITY>>),
}

/// The reason an exact launch was refused, without the child it may have returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchCause {
    /// No live-child slot was available before the OS launch.
    Full,
    /// The same test name was already live in the same test binary.
    Duplicate,
    /// Inline identity storage was too short for the test name.
    NameTooLong { required: usize, available: usize },
    /// Argument or environment lowering failed.
    Arguments(UnixArgumentsError),
    /// `posix_spawn` returned this error number.
    Spawn { errno: c_int },
    /// The slot table rejected a child that the capacity check had admitted.
    CapacityChanged,
    /// Outcome accounting refused the spawned child.
    Accounting(OutcomeError),
}

impl<const NAME_CAPACITY: usize> From<&UnixLaunchError<NAME_CAPACITY>> for LaunchCause {
    fn from(error: &UnixLaunchError<NAME_CAPACITY>) -> Self {
        match error {
            UnixLaunchError::Full => Self::Full,
            UnixLaunchError::Duplicate => Self::Duplicate,
            UnixLaunchError::NameTooLong {
                required,
                available,
            } => Self::NameTooLong {
                required: *required,
                available: *available,
            },
            UnixLaunchError::Arguments(error) => Self::Arguments(*error),
            UnixLaunchError::Spawn(error) => Self::Spawn { errno: error.code },
            UnixLaunchError::CapacityChanged(_) => Self::CapacityChanged,
            UnixLaunchError::Accounting(error) => Self::Accounting(error.error),
        }
    }
}

/// One owner of the same live child table during exact launch.
pub enum UnixLaunchOwner<'call, 'ledger, const NAME_CAPACITY: usize, const CAPACITY: usize> {
    /// Direct session ownership before outcome accounting is attached.
    Sessions(&'call mut SessionSlots<RunningUnixChild<NAME_CAPACITY>, CAPACITY>),
    /// Outcome accounting owns the same table and counts this child on admission.
    Ledger(&'call mut OutcomeLedger<'ledger, RunningUnixChild<NAME_CAPACITY>, CAPACITY>),
}

impl<const NAME_CAPACITY: usize, const CAPACITY: usize>
    UnixLaunchOwner<'_, '_, NAME_CAPACITY, CAPACITY>
{
    fn sessions(&self) -> &SessionSlots<RunningUnixChild<NAME_CAPACITY>, CAPACITY> {
        match self {
            Self::Sessions(sessions) => sessions,
            Self::Ledger(ledger) => ledger.sessions(),
        }
    }

    fn admit(
        &mut self,
        child: RunningUnixChild<NAME_CAPACITY>,
    ) -> Result<SessionSlotId, UnixLaunchError<NAME_CAPACITY>> {
        match self {
            Self::Sessions(sessions) => sessions
                .insert(child)
                .map_err(UnixLaunchError::CapacityChanged),
            Self::Ledger(ledger) => ledger.start(child).map_err(UnixLaunchError::Accounting),
        }
    }
}

/// Borrowed launch input and caller-owned OS representation storage.
pub struct UnixLaunchInput<
    'call,
    'ledger,
    'executable,
    'scratch,
    'name,
    const NAME_CAPACITY: usize,
    const CAPACITY: usize,
> {
    /// Ordinal of the test binary this intent was listed from; a test name is live at most once per target.
    pub target: usize,
    /// Single-owner fixed table that receives the child immediately.
    pub owner: UnixLaunchOwner<'call, 'ledger, NAME_CAPACITY, CAPACITY>,
    /// Borrowed test path and name from the listing source.
    pub intent: ExactTestIntent<'executable, 'scratch, 'name>,
    /// Borrowed inherited environment entries in `KEY=VALUE` form.
    pub inherited_environment: &'call [&'call CStr],
    /// Borrowed child-only environment entries that override inherited keys.
    pub child_overrides: &'call [&'call CStr],
    /// Borrowed test-harness flags appended after the exact test name.
    pub harness_arguments: &'call [&'call [u8]],
    /// Directory installed by the host spawn file actions.
    pub current_directory: &'call CStr,
    /// Bytes used once to lower argv to null-terminated OS strings.
    pub argument_bytes: &'call mut [u8],
    /// Pointers into `argument_bytes` including the null sentinel.
    pub argument_pointers: &'call mut [*const c_char],
    /// Pointers into borrowed environment entries including the null sentinel.
    pub environment_pointers: &'call mut [*const c_char],
}

impl<'call, 'ledger: 'call, const NAME_CAPACITY: usize, const CAPACITY: usize>
    UnixLaunchInput<'call, 'ledger, '_, '_, '_, NAME_CAPACITY, CAPACITY>
{
    /// Starts one exact child and places it directly in a slot.
    ///
    /// # Errors
    ///
    /// Returns [`UnixLaunchError`] when the table is full, the name is live or too long, the argument storage is short, the spawn fails, or accounting refuses the child.
    pub fn launch(mut self) -> Result<SessionSlotId, UnixLaunchError<NAME_CAPACITY>> {
        if self.owner.sessions().is_full() {
            return Err(UnixLaunchError::Full);
        }
        let name_bytes = self.intent.test_name().as_bytes();
        if self
            .owner
            .sessions()
            .any(|session| session.target == self.target && session.name_bytes() == name_bytes)
        {
            return Err(UnixLaunchError::Duplicate);
        }
        if name_bytes.len() > NAME_CAPACITY {
            return Err(UnixLaunchError::NameTooLong {
                required: name_bytes.len(),
                available: NAME_CAPACITY,
            });
        }

        let mut name = [0; NAME_CAPACITY];
        name[..name_bytes.len()].copy_from_slice(name_bytes);
        let arguments = lower_unix_arguments(
            &self.intent,
            self.inherited_environment,
            self.child_overrides,
            self.harness_arguments,
            self.argument_bytes,
            self.argument_pointers,
            self.environment_pointers,
        )
        .map_err(UnixLaunchError::Arguments)?;
        let child =
            spawn_unix(&arguments, self.current_directory).map_err(UnixLaunchError::Spawn)?;
        let session = RunningUnixChild {
            child,
            name,
            name_length: name_bytes.len(),
            target: self.target,
            completion: ChildCompletion::new(),
            exit_status: None,
        };
        self.owner.admit(session)
    }
}

/// One accepted child event or one terminal label written to caller storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixOutcomeStep<'bytes> {
    /// Borrowed stdout bytes retain their host buffer address.
    Stdout(&'bytes [u8]),
    /// Borrowed stderr bytes retain their host buffer address.
    Stderr(&'bytes [u8]),
    /// Exit or one stream EOF was accepted while another event is still needed.
    Pending,
    /// Both streams and process exit completed, and one label was written.
    Reported {
        /// The terminal category counted by the ledger.
        outcome: TerminalOutcome,
        /// Bytes written to the caller's report buffer.
        report_bytes: usize,
    },
}

/// Invalid event, lifecycle transition, accounting, or report storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixOutcomeError {
    Stale(StaleHostEvent),
    InvalidCompletion(InvalidChildCompletionEvent),
    Accounting(OutcomeError),
    Report(ReportError),
    MissingExitStatus,
    /// Exit 0 with a libtest result line of 0 passed, 0 failed, 0 ignored: no test body ran.
    NothingRan,
}

/// Classification of an exact-test child by its libtest result line rather than its exit code alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibtestVerdict {
    /// No result line was seen, as from a custom harness; the exit code decides.
    Unreported,
    /// At least one test passed or failed.
    Ran,
    /// 0 passed, 0 failed, at least 1 ignored.
    Skipped,
    /// 0 passed, 0 failed, 0 ignored.
    NothingRan,
}

/// One borrowed host event and the sole mutable outcome owner.
pub struct UnixOutcomeInput<
    'call,
    'ledger,
    'bytes,
    const NAME_CAPACITY: usize,
    const CAPACITY: usize,
> {
    /// Coordinator-owned accounting and live child sessions.
    pub ledger: &'call mut OutcomeLedger<'ledger, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
    /// Readiness payload with its generation-checked slot.
    pub event: HostEvent<'bytes>,
    /// Caller-owned output for at most one terminal label.
    pub report_output: &'call mut [u8],
    /// Whether the terminal label uses ANSI color.
    pub color: bool,
    /// What the child's libtest `test result:` line said about the one exact test.
    pub verdict: LibtestVerdict,
}

impl<'call, 'ledger: 'call, 'bytes, const NAME_CAPACITY: usize, const CAPACITY: usize>
    UnixOutcomeInput<'call, 'ledger, 'bytes, NAME_CAPACITY, CAPACITY>
{
    /// Joins host validation, child completion, accounting, and reporting.
    ///
    /// # Errors
    ///
    /// Returns [`UnixOutcomeError`] when the event is stale or the ledger refuses the transition.
    pub fn apply(self) -> Result<UnixOutcomeStep<'bytes>, UnixOutcomeError> {
        let accepted = self
            .event
            .admit(self.ledger.sessions())
            .map_err(UnixOutcomeError::Stale)?;
        let slot = accepted.slot();
        let (completion_event, exit_status) = match accepted {
            HostEvent::Stdout { bytes, .. } => {
                Self::record_bytes(self.ledger, slot, ChildCompletionEvent::StdoutBytes)?;
                return Ok(UnixOutcomeStep::Stdout(bytes));
            }
            HostEvent::Stderr { bytes, .. } => {
                Self::record_bytes(self.ledger, slot, ChildCompletionEvent::StderrBytes)?;
                return Ok(UnixOutcomeStep::Stderr(bytes));
            }
            HostEvent::StdoutEof { .. } => (ChildCompletionEvent::StdoutEof, None),
            HostEvent::StderrEof { .. } => (ChildCompletionEvent::StderrEof, None),
            HostEvent::Exit { status, .. } => (ChildCompletionEvent::Exit, Some(status)),
        };
        let (previous_completion, previous_status, complete) = {
            let session = self
                .ledger
                .session_mut(slot)
                .ok_or(UnixOutcomeError::Accounting(OutcomeError::UnknownSession))?;
            let previous_completion = session.completion;
            let previous_status = session.exit_status;
            let complete = session
                .completion
                .apply(completion_event)
                .map_err(UnixOutcomeError::InvalidCompletion)?;
            if let Some(status) = exit_status {
                session.exit_status = Some(status);
            }
            (previous_completion, previous_status, complete)
        };
        if !complete {
            return Ok(UnixOutcomeStep::Pending);
        }

        let status = self
            .ledger
            .sessions()
            .get(slot)
            .and_then(|session| session.exit_status)
            .ok_or(UnixOutcomeError::MissingExitStatus)?;
        let outcome = match (status, self.verdict) {
            (0, LibtestVerdict::NothingRan) => return Err(UnixOutcomeError::NothingRan),
            (0, LibtestVerdict::Skipped) => TerminalOutcome::Ignored,
            (0, _) => TerminalOutcome::Passed,
            _ => TerminalOutcome::Failed,
        };
        let report = self
            .ledger
            .can_finish(slot, outcome)
            .map_err(UnixOutcomeError::Accounting)
            .and_then(|()| {
                write_report(&[outcome], self.color, self.report_output)
                    .map_err(UnixOutcomeError::Report)
            });
        let report_bytes = match report {
            Ok(report_bytes) => report_bytes,
            Err(error) => {
                let session = self
                    .ledger
                    .session_mut(slot)
                    .ok_or(UnixOutcomeError::Accounting(OutcomeError::UnknownSession))?;
                session.completion = previous_completion;
                session.exit_status = previous_status;
                return Err(error);
            }
        };
        self.ledger
            .finish(slot, outcome)
            .map_err(UnixOutcomeError::Accounting)?;
        Ok(UnixOutcomeStep::Reported {
            outcome,
            report_bytes,
        })
    }

    fn record_bytes(
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        slot: SessionSlotId,
        event: ChildCompletionEvent,
    ) -> Result<(), UnixOutcomeError> {
        let session = ledger
            .session_mut(slot)
            .ok_or(UnixOutcomeError::Accounting(OutcomeError::UnknownSession))?;
        session
            .completion
            .apply(event)
            .map(|_| ())
            .map_err(UnixOutcomeError::InvalidCompletion)
    }
}

#[cfg(all(
    test,
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-core",
    feature = "edge-os"
))]
mod tests {
    use core::convert::Infallible;
    use core::ffi::{CStr, c_char};
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};

    use proxima_core::io::AsyncRead;

    use super::{
        LibtestVerdict, RunningUnixChild, UnixLaunchError, UnixLaunchInput, UnixLaunchOwner,
        UnixOutcomeError, UnixOutcomeInput, UnixOutcomeStep,
    };
    use crate::ExecutablePath;
    use crate::ExitSource;
    use crate::artifact_stream::ArtifactFramer;
    use crate::discover_listing::{ListingSource, ListingSourceStep};
    use crate::host_event::HostEvent;
    use core::num::NonZeroI32;
    use prime::os::core_shard::run_on_this_thread;
    use proxima_primitives::pipe::UnpinPipe;
    use proxima_runtime::CoreId;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use crate::outcomes::{OutcomeLedger, TerminalOutcome};
    use crate::schedule::{SessionSlotId, SessionSlots};
    use crate::selection::NameFilter;
    use crate::test_intent::ExactTestIntent;

    const ALPHA: &[u8] = b"runner::tests::fixture_alpha: test\n";
    const BETA: &[u8] = b"runner::tests::fixture_beta: test\n";
    const ALPHA_NAME: &str = "runner::tests::fixture_alpha";
    const BETA_NAME: &str = "runner::tests::fixture_beta";
    const FAIL_NAME: &str = "runner::tests::fixture_fail";

    struct HeldListingReader {
        next_chunk: usize,
        read_calls: usize,
        eof_seen: bool,
    }

    impl AsyncRead for HeldListingReader {
        type Error = Infallible;

        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<Result<usize, Self::Error>> {
            let reader = self.get_mut();
            reader.read_calls += 1;
            let chunk = match reader.next_chunk {
                0 => ALPHA,
                1 => BETA,
                _ => return Poll::Pending,
            };
            reader.next_chunk += 1;
            buffer[..chunk.len()].copy_from_slice(chunk);
            Poll::Ready(Ok(chunk.len()))
        }
    }

    fn launch<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        sessions: &mut SessionSlots<RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
    ) -> Result<SessionSlotId, UnixLaunchError<NAME_CAPACITY>> {
        launch_target(sessions, intent, 0)
    }

    fn launch_target<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        sessions: &mut SessionSlots<RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
        target: usize,
    ) -> Result<SessionSlotId, UnixLaunchError<NAME_CAPACITY>> {
        let inherited: [&CStr; 0] = [];
        let overrides = [c"CARGO_VAL_C51A_CHILD=1"];
        let mut argument_bytes = [0; 512];
        let mut argument_pointers: [*const c_char; 5] = [core::ptr::null(); 5];
        let mut environment_pointers: [*const c_char; 2] = [core::ptr::null(); 2];
        UnixLaunchInput {
            target,
            owner: UnixLaunchOwner::Sessions(sessions),
            intent,
            inherited_environment: &inherited,
            child_overrides: &overrides,
            harness_arguments: &[],
            current_directory: c"/",
            argument_bytes: &mut argument_bytes,
            argument_pointers: &mut argument_pointers,
            environment_pointers: &mut environment_pointers,
        }
        .launch()
    }

    fn intent<'executable, 'name>(
        executable: &'executable str,
        name: &'name str,
    ) -> ExactTestIntent<'executable, 'static, 'name> {
        ExactTestIntent {
            executable: ExecutablePath::Borrowed(executable),
            test_name: name,
        }
    }

    fn launch_accounted<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
        child_overrides: &[&CStr],
    ) -> Result<SessionSlotId, UnixLaunchError<NAME_CAPACITY>> {
        let inherited: [&CStr; 0] = [];
        let mut argument_bytes = [0; 512];
        let mut argument_pointers: [*const c_char; 5] = [core::ptr::null(); 5];
        let mut environment_pointers: [*const c_char; 2] = [core::ptr::null(); 2];
        UnixLaunchInput {
            target: 0,
            owner: UnixLaunchOwner::Ledger(ledger),
            intent,
            inherited_environment: &inherited,
            child_overrides,
            harness_arguments: &[],
            current_directory: c"/",
            argument_bytes: &mut argument_bytes,
            argument_pointers: &mut argument_pointers,
            environment_pointers: &mut environment_pointers,
        }
        .launch()
    }

    fn deliver<'bytes, const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        event: HostEvent<'bytes>,
        report_output: &mut [u8],
    ) -> Result<UnixOutcomeStep<'bytes>, UnixOutcomeError> {
        UnixOutcomeInput {
            ledger,
            event,
            report_output,
            color: false,
            verdict: LibtestVerdict::Ran,
        }
        .apply()
    }

    fn consume_stream<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        slot: SessionSlotId,
        descriptor: libc::c_int,
        stdout: bool,
        report_output: &mut [u8],
    ) -> (usize, usize, Option<TerminalOutcome>) {
        let mut buffer = [0_u8; 4096];
        let mut total_bytes = 0;
        let mut borrowed_reads = 0;
        loop {
            let length = read_stream(descriptor, &mut buffer);
            if length > 0 {
                let bytes = &buffer[..length.cast_unsigned()];
                let event = if stdout {
                    HostEvent::Stdout { slot, bytes }
                } else {
                    HostEvent::Stderr { slot, bytes }
                };
                let step =
                    deliver(ledger, event, report_output).expect("borrowed output is accepted");
                match step {
                    UnixOutcomeStep::Stdout(accepted) | UnixOutcomeStep::Stderr(accepted) => {
                        assert_eq!(accepted.as_ptr(), bytes.as_ptr());
                        assert_eq!(accepted, bytes);
                    }
                    _ => panic!("output bytes were not lent"),
                }
                total_bytes += bytes.len();
                borrowed_reads += 1;
                continue;
            }
            assert_eq!(length, 0, "read_stream returns bytes or EOF");
            let event = if stdout {
                HostEvent::StdoutEof { slot }
            } else {
                HostEvent::StderrEof { slot }
            };
            let step = deliver(ledger, event, report_output).expect("stream EOF is accepted");
            return match step {
                UnixOutcomeStep::Pending => (total_bytes, borrowed_reads, None),
                UnixOutcomeStep::Reported {
                    outcome,
                    report_bytes,
                } => {
                    assert_eq!(
                        &report_output[..report_bytes],
                        if outcome == TerminalOutcome::Passed {
                            b"PASS\n"
                        } else {
                            b"FAIL\n"
                        }
                    );
                    (total_bytes, borrowed_reads, Some(outcome))
                }
                _ => panic!("stream EOF returned output bytes"),
            };
        }
    }

    fn launch_watched<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
        child_overrides: &[&CStr],
    ) -> (SessionSlotId, NonZeroI32) {
        let slot =
            launch_accounted(ledger, intent, child_overrides).expect("child starts through ledger");
        let process_id = ledger
            .sessions()
            .get(slot)
            .expect("slot live")
            .child()
            .process_id();
        (slot, watch(process_id))
    }

    fn account_passed<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        passed: SessionSlotId,
        passed_watch: NonZeroI32,
    ) -> (usize, usize) {
        let passed_status = terminal_status(reap(passed_watch));
        assert_eq!(passed_status, 0);
        let mut passed_report = [0_u8; 16];
        assert_eq!(
            deliver(
                ledger,
                HostEvent::Exit {
                    slot: passed,
                    status: passed_status
                },
                &mut passed_report
            )
            .expect("pass exit accepted"),
            UnixOutcomeStep::Pending,
        );
        assert!(ledger.sessions().get(passed).is_some());
        assert_eq!(ledger.unaccounted(), 2);
        let (passed_stdout, passed_stderr) = stream_descriptors(ledger, passed);
        let (stdout_bytes, stdout_reads, first_report) =
            consume_stream(ledger, passed, passed_stdout, true, &mut passed_report);
        assert_eq!(first_report, None);
        let (stderr_bytes, stderr_reads, passed_outcome) =
            consume_stream(ledger, passed, passed_stderr, false, &mut passed_report);
        assert_eq!(passed_outcome, Some(TerminalOutcome::Passed));
        assert!(stdout_bytes + stderr_bytes > 0);
        assert!(ledger.sessions().get(passed).is_none());
        assert_eq!(ledger.snapshot().passed, 1);
        (stdout_bytes + stderr_bytes, stdout_reads + stderr_reads)
    }

    fn stream_descriptors<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, CAPACITY>,
        slot: SessionSlotId,
    ) -> (libc::c_int, libc::c_int) {
        let child = ledger.sessions().get(slot).expect("child live").child();
        (child.stdout().raw(), child.stderr().raw())
    }

    fn terminal_status(status: ExitStatus) -> i32 {
        status
            .code()
            .unwrap_or_else(|| -status.signal().expect("signal exit has a number"))
    }

    fn drain_descriptor(descriptor: libc::c_int) {
        let mut buffer = [0_u8; 256];
        loop {
            let length = read_stream(descriptor, &mut buffer);
            if length == 0 {
                return;
            }
            assert!(length > 0, "read_stream returns bytes or EOF");
        }
    }

    fn read_stream(descriptor: libc::c_int, buffer: &mut [u8]) -> isize {
        loop {
            let length =
                unsafe { libc::read(descriptor, buffer.as_mut_ptr().cast(), buffer.len()) };
            if length >= 0 {
                return length;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => {}
                Some(libc::EAGAIN) => {
                    std::println!(
                        "{{\"event\":\"nonblocking_read_pending\",\"descriptor\":{descriptor},\"errno\":{}}}",
                        libc::EAGAIN
                    );
                    let mut readiness = libc::pollfd {
                        fd: descriptor,
                        events: libc::POLLIN | libc::POLLHUP,
                        revents: 0,
                    };
                    loop {
                        let ready = unsafe { libc::poll(&raw mut readiness, 1, -1) };
                        if ready > 0 {
                            break;
                        }
                        assert_eq!(
                            std::io::Error::last_os_error().raw_os_error(),
                            Some(libc::EINTR),
                            "poll waits for read readiness"
                        );
                    }
                }
                _ => panic!("unexpected nonblocking read error: {error}"),
            }
        }
    }

    fn watch(process_id: libc::pid_t) -> NonZeroI32 {
        NonZeroI32::new(process_id).expect("spawned child has a process id")
    }

    fn reap(watch: NonZeroI32) -> ExitStatus {
        run_on_this_thread(CoreId(0), async move {
            let exit = ExitSource::new(watch);
            UnpinPipe::call(&exit, ())
                .await
                .expect("exit source reports once")
                .expect("child exit is reaped")
        })
        .expect("root completes")
    }

    fn assert_libtest_output(
        stdout_descriptor: libc::c_int,
        stderr_descriptor: libc::c_int,
        expected_name: &str,
    ) {
        let stdout = capture_stream(stdout_descriptor);
        let stderr = capture_stream(stderr_descriptor);
        assert!(
            stdout.contains(expected_name) || stderr.contains(expected_name),
            "exact child name appears in libtest output"
        );
        assert!(
            stdout.contains("cargo-val-c51a-child") || stderr.contains("cargo-val-c51a-child"),
            "selected child body ran"
        );
    }

    fn capture_stream(descriptor: libc::c_int) -> std::string::String {
        let mut buffer = [0_u8; 4096];
        let mut length = 0;
        loop {
            let count = read_stream(descriptor, &mut buffer[length..]);
            if count == 0 {
                break;
            }
            length += count.cast_unsigned();
            assert!(length < buffer.len(), "child stdout fits fixed capture");
        }
        std::string::String::from_utf8(buffer[..length].to_vec()).expect("libtest output is UTF-8")
    }

    #[test]
    fn c51a_launches_exact_child_before_listing_eof_and_stops_at_full_slot() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut source_bytes = [0; 128];
        let mut frame_bytes = [0; 129];
        let mut source =
            ListingSource::new(&mut source_bytes).expect("listing storage is nonempty");
        let mut framer = ArtifactFramer::new(&mut frame_bytes, 128).expect("frame storage fits");
        let mut reader = HeldListingReader {
            next_chunk: 0,
            read_calls: 0,
            eof_seen: false,
        };
        let mut sessions = SessionSlots::<RunningUnixChild<64>, 1>::new();
        let mut context = Context::from_waker(Waker::noop());
        let mut observed_names = [[0_u8; 64]; 2];
        let mut observed_name_lengths = [0; 2];

        for (index, expected_name) in [ALPHA_NAME, BETA_NAME].into_iter().enumerate() {
            let launched = loop {
                match source.poll_next(
                    Pin::new(&mut reader),
                    &mut framer,
                    &sessions,
                    ExecutablePath::Borrowed(executable),
                    NameFilter::Substring(""),
                    &mut context,
                ) {
                    Poll::Ready(Ok(ListingSourceStep::Launch(intent))) => {
                        assert_eq!(intent.test_name(), expected_name);
                        break launch(&mut sessions, intent).expect("exact child starts");
                    }
                    Poll::Ready(Ok(
                        ListingSourceStep::Read { .. }
                        | ListingSourceStep::NeedMore
                        | ListingSourceStep::Ignored
                        | ListingSourceStep::Filtered,
                    )) => {}
                    other => panic!("unexpected listing step: {other:?}"),
                }
            };
            assert!(!reader.eof_seen);
            let live_name = sessions
                .get(launched)
                .expect("slot owns child")
                .name_bytes();
            assert_eq!(live_name, expected_name.as_bytes());
            observed_names[index][..live_name.len()].copy_from_slice(live_name);
            observed_name_lengths[index] = live_name.len();
            let process_id = sessions
                .get(launched)
                .expect("child remains live")
                .child()
                .process_id();
            let child_watch = watch(process_id);
            let reads_at_full = reader.read_calls;
            let chunks_at_full = reader.next_chunk;
            assert!(matches!(
                source.poll_next(
                    Pin::new(&mut reader),
                    &mut framer,
                    &sessions,
                    ExecutablePath::Borrowed(executable),
                    NameFilter::Substring(""),
                    &mut context,
                ),
                Poll::Ready(Ok(ListingSourceStep::Full))
            ));
            assert_eq!(reader.read_calls, reads_at_full);
            assert_eq!(reader.next_chunk, chunks_at_full);
            if index == 0 {
                assert!(matches!(
                    launch(&mut sessions, intent(executable, BETA_NAME)),
                    Err(UnixLaunchError::Full)
                ));
                assert_eq!(sessions.len(), 1);
            }
            let status = reap(child_watch);
            assert_eq!(status.code(), Some(0));
            let session = sessions
                .get(launched)
                .expect("completed child remains live");
            assert_libtest_output(
                session.child().stdout().raw(),
                session.child().stderr().raw(),
                expected_name,
            );
            sessions.remove(launched).expect("both streams reached EOF");
        }
        let first_name = &observed_names[0][..observed_name_lengths[0]];
        let second_name = &observed_names[1][..observed_name_lengths[1]];
        assert_ne!(first_name, second_name);
        assert_eq!(sessions.len(), 0);
        std::println!(
            "{{\"exact_launches\":2,\"duplicate_names\":0,\"full_slot_pulls\":0,\"first_before_eof\":true,\"captured_stream_eofs\":4,\"name_capacity_bytes\":64,\"observed_names\":[{:?},{:?}]}}",
            core::str::from_utf8(first_name).expect("first name remains UTF-8"),
            core::str::from_utf8(second_name).expect("second name remains UTF-8"),
        );
    }

    #[test]
    fn same_test_name_from_two_targets_is_live_twice() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningUnixChild<64>, 3>::new();
        let first = launch_target(&mut sessions, intent(executable, ALPHA_NAME), 0)
            .expect("first target starts alpha");
        let second = launch_target(&mut sessions, intent(executable, ALPHA_NAME), 1)
            .expect("second target starts the same name while the first is live");
        assert_ne!(first, second);
        assert!(matches!(
            launch_target(&mut sessions, intent(executable, ALPHA_NAME), 1),
            Err(UnixLaunchError::Duplicate)
        ));
        assert!(matches!(
            launch_target(&mut sessions, intent(executable, ALPHA_NAME), 0),
            Err(UnixLaunchError::Duplicate)
        ));
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn c51a_duplicate_and_name_capacity_controls_reject_before_spawn() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningUnixChild<64>, 2>::new();
        let alpha = launch(&mut sessions, intent(executable, ALPHA_NAME)).expect("alpha starts");
        let first_process_id = sessions
            .get(alpha)
            .expect("alpha stored")
            .child()
            .process_id();
        let first_watch = watch(first_process_id);
        assert!(matches!(
            launch(&mut sessions, intent(executable, ALPHA_NAME)),
            Err(UnixLaunchError::Duplicate)
        ));
        assert_eq!(sessions.len(), 1);
        let beta =
            launch(&mut sessions, intent(executable, BETA_NAME)).expect("distinct beta starts");
        let second_process_id = sessions
            .get(beta)
            .expect("beta stored")
            .child()
            .process_id();
        let second_watch = watch(second_process_id);
        assert_ne!(first_process_id, second_process_id);
        assert_eq!(sessions.len(), 2);
        for (slot, child_watch) in [(alpha, first_watch), (beta, second_watch)] {
            assert_eq!(reap(child_watch).code(), Some(0));
            let session = sessions.get(slot).expect("exited child remains live");
            let stdout = capture_stream(session.child().stdout().raw());
            let stderr = capture_stream(session.child().stderr().raw());
            assert!(
                stdout.contains("cargo-val-c51a-child") || stderr.contains("cargo-val-c51a-child")
            );
            sessions.remove(slot).expect("both streams reached EOF");
        }
        let mut short_sessions = SessionSlots::<RunningUnixChild<8>, 1>::new();
        assert!(matches!(
            launch(&mut short_sessions, intent(executable, ALPHA_NAME)),
            Err(UnixLaunchError::NameTooLong { required, available: 8 }) if required == ALPHA_NAME.len()
        ));
        assert_eq!(short_sessions.len(), 0);
        std::println!(
            "{{\"duplicate_rejections\":1,\"distinct_launches\":2,\"capacity_rejections\":1,\"post_rejection_slots\":0,\"captured_stream_eofs\":4}}"
        );
    }

    #[test]
    fn c51b_accounts_pass_and_fail_after_exit_and_both_stream_eofs() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningUnixChild<64>, 2>::new();
        let mut ledger = OutcomeLedger::new(&mut sessions).expect("table starts empty");

        let (passed, passed_watch) = launch_watched(
            &mut ledger,
            intent(executable, ALPHA_NAME),
            &[c"CARGO_VAL_C51A_CHILD=1"],
        );
        let (failed, failed_watch) = launch_watched(
            &mut ledger,
            intent(executable, FAIL_NAME),
            &[c"CARGO_VAL_C51B_FAIL=1"],
        );
        assert_eq!(ledger.snapshot().discovered, 2);
        assert_eq!(ledger.unaccounted(), 2);

        let (passed_bytes, passed_reads) =
            account_passed(&mut ledger, passed, passed_watch);

        let failed_status = terminal_status(reap(failed_watch));
        assert_ne!(failed_status, 0);
        let mut failed_report = [0_u8; 16];
        let (failed_stdout, failed_stderr) = stream_descriptors(&ledger, failed);
        let (failed_stdout_bytes, failed_stdout_reads, second_report) =
            consume_stream(&mut ledger, failed, failed_stdout, true, &mut failed_report);
        let (failed_stderr_bytes, failed_stderr_reads, third_report) = consume_stream(
            &mut ledger,
            failed,
            failed_stderr,
            false,
            &mut failed_report,
        );
        assert_eq!(second_report, None);
        assert_eq!(third_report, None);
        assert!(ledger.sessions().get(failed).is_some());
        assert_eq!(ledger.unaccounted(), 1);
        let final_step = deliver(
            &mut ledger,
            HostEvent::Exit {
                slot: failed,
                status: failed_status,
            },
            &mut failed_report,
        )
        .expect("failure exit is accepted after both streams");
        assert_eq!(
            final_step,
            UnixOutcomeStep::Reported {
                outcome: TerminalOutcome::Failed,
                report_bytes: 5
            }
        );
        assert_eq!(&failed_report[..5], b"FAIL\n");
        assert!(failed_stdout_bytes + failed_stderr_bytes > 0);
        assert_eq!(ledger.snapshot().failed, 1);
        assert_eq!(ledger.unaccounted(), 0);
        assert_eq!(ledger.sessions().len(), 0);
        std::println!(
            "{{\"discovered\":2,\"passed\":1,\"failed\":1,\"unaccounted\":0,\"reported_records\":2,\"stream_eofs\":4,\"borrowed_payload_reads\":{},\"pass_exit_first\":1,\"fail_streams_first\":1,\"pass_output_bytes\":{},\"fail_output_bytes\":{}}}",
            passed_reads + failed_stdout_reads + failed_stderr_reads,
            passed_bytes,
            failed_stdout_bytes + failed_stderr_bytes,
        );
    }

    #[test]
    fn c51b_duplicate_exit_is_rejected_while_child_remains_live() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningUnixChild<64>, 1>::new();
        let mut ledger = OutcomeLedger::new(&mut sessions).expect("table starts empty");
        let slot = launch_accounted(
            &mut ledger,
            intent(executable, ALPHA_NAME),
            &[c"CARGO_VAL_C51A_CHILD=1"],
        )
        .expect("control child starts through ledger");
        let process_id = ledger
            .sessions()
            .get(slot)
            .expect("child live")
            .child()
            .process_id();
        let child_watch = watch(process_id);
        let status = terminal_status(reap(child_watch));
        assert_eq!(status, 0);
        let mut report = [0_u8; 16];
        let exit = HostEvent::Exit { slot, status };
        assert_eq!(
            deliver(&mut ledger, exit, &mut report),
            Ok(UnixOutcomeStep::Pending)
        );
        assert_eq!(
            deliver(&mut ledger, exit, &mut report),
            Err(UnixOutcomeError::InvalidCompletion(
                crate::child_completion::InvalidChildCompletionEvent
            )),
        );
        assert_eq!(ledger.unaccounted(), 1);
        assert!(ledger.sessions().get(slot).is_some());
        let stdout = ledger
            .sessions()
            .get(slot)
            .expect("child live")
            .child()
            .stdout()
            .raw();
        let stderr = ledger
            .sessions()
            .get(slot)
            .expect("child live")
            .child()
            .stderr()
            .raw();
        assert_eq!(
            consume_stream(&mut ledger, slot, stdout, true, &mut report).2,
            None
        );
        assert_eq!(
            deliver(
                &mut ledger,
                HostEvent::Stdout {
                    slot,
                    bytes: b"late"
                },
                &mut report
            ),
            Err(UnixOutcomeError::InvalidCompletion(
                crate::child_completion::InvalidChildCompletionEvent
            )),
        );
        drain_descriptor(stderr);
        let mut short_report = [0_u8; 4];
        assert_eq!(
            deliver(
                &mut ledger,
                HostEvent::StderrEof { slot },
                &mut short_report
            ),
            Err(UnixOutcomeError::Report(
                crate::reporter::ReportError::OutputTooSmall
            )),
        );
        assert_eq!(ledger.unaccounted(), 1);
        assert!(ledger.sessions().get(slot).is_some());
        assert_eq!(
            deliver(&mut ledger, HostEvent::StderrEof { slot }, &mut report),
            Ok(UnixOutcomeStep::Reported {
                outcome: TerminalOutcome::Passed,
                report_bytes: 5,
            }),
        );
        assert_eq!(ledger.unaccounted(), 0);
        assert_eq!(ledger.snapshot().passed, 1);
        assert_eq!(ledger.snapshot().failed, 0);
        assert!(matches!(
            deliver(&mut ledger, exit, &mut report),
            Err(UnixOutcomeError::Stale(_))
        ));
        std::println!(
            "{{\"duplicate_live_exit_rejections\":1,\"output_after_eof_rejections\":1,\"short_report_retries\":1,\"stale_exit_rejections\":1,\"reported_records\":1,\"unaccounted\":0}}"
        );
    }

    fn write_marker(marker: &[u8]) {
        let written =
            unsafe { libc::write(libc::STDOUT_FILENO, marker.as_ptr().cast(), marker.len()) };
        assert_eq!(written, marker.len().cast_signed());
    }

    #[test]
    fn fixture_alpha() {
        if std::env::var_os("CARGO_VAL_C51A_CHILD").is_some() {
            write_marker(b"cargo-val-c51a-child");
        }
    }

    #[test]
    fn fixture_beta() {
        if std::env::var_os("CARGO_VAL_C51A_CHILD").is_some() {
            write_marker(b"cargo-val-c51a-child");
        }
    }

    #[test]
    fn fixture_fail() {
        if std::env::var_os("CARGO_VAL_C51B_FAIL").is_some() {
            write_marker(b"cargo-val-c51b-failure");
            panic!("cargo-val-c51b-fixture-failure");
        }
    }
}

#[cfg(all(test, feature = "proxima-core"))]
mod cause_tests {
    use core::ffi::{CStr, c_char};

    use super::{
        LaunchCause, RunningUnixChild, UnixLaunchError, UnixLaunchInput, UnixLaunchOwner,
        UnixSpawnError,
    };
    use crate::ExecutablePath;
    use crate::outcomes::{OutcomeError, StartError};
    use crate::schedule::SessionSlots;
    use crate::test_intent::ExactTestIntent;
    use crate::edge_args::UnixArgumentsError;

    fn live_child() -> RunningUnixChild<64> {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let inherited: [&CStr; 0] = [];
        let mut argument_bytes = [0; 512];
        let mut argument_pointers: [*const c_char; 5] = [core::ptr::null(); 5];
        let mut environment_pointers: [*const c_char; 2] = [core::ptr::null(); 2];
        let mut sessions = SessionSlots::<RunningUnixChild<64>, 1>::new();
        let slot = UnixLaunchInput {
            target: 0,
            owner: UnixLaunchOwner::Sessions(&mut sessions),
            intent: ExactTestIntent::new(
                ExecutablePath::Borrowed(executable),
                "runner::cause_tests::absent_name",
            ),
            inherited_environment: &inherited,
            child_overrides: &[],
            harness_arguments: &[],
            current_directory: c"/",
            argument_bytes: &mut argument_bytes,
            argument_pointers: &mut argument_pointers,
            environment_pointers: &mut environment_pointers,
        }
        .launch()
        .expect("the test executable starts");
        sessions.remove(slot).expect("the child is stored")
    }

    #[test]
    fn every_launch_error_variant_maps_to_its_cause() {
        let cases: [(UnixLaunchError<64>, LaunchCause); 7] = [
            (UnixLaunchError::Full, LaunchCause::Full),
            (UnixLaunchError::Duplicate, LaunchCause::Duplicate),
            (
                UnixLaunchError::NameTooLong {
                    required: 70,
                    available: 64,
                },
                LaunchCause::NameTooLong {
                    required: 70,
                    available: 64,
                },
            ),
            (
                UnixLaunchError::Arguments(UnixArgumentsError::InteriorNul),
                LaunchCause::Arguments(UnixArgumentsError::InteriorNul),
            ),
            (
                UnixLaunchError::Spawn(UnixSpawnError { code: libc::ENOENT }),
                LaunchCause::Spawn {
                    errno: libc::ENOENT,
                },
            ),
            (
                UnixLaunchError::CapacityChanged(live_child()),
                LaunchCause::CapacityChanged,
            ),
            (
                UnixLaunchError::Accounting(StartError {
                    error: OutcomeError::CapacityExhausted,
                    child: live_child(),
                }),
                LaunchCause::Accounting(OutcomeError::CapacityExhausted),
            ),
        ];
        let mut checked = 0;
        for (error, expected) in &cases {
            assert_eq!(LaunchCause::from(error), *expected);
            checked += 1;
        }
        assert_eq!(checked, 7);
    }
}
