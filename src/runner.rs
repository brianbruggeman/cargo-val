//! Single-owner exact child launch from one borrowed listing intent.

use crate::ExecutablePath;
use crate::child_completion::{ChildCompletion, ChildCompletionEvent, InvalidChildCompletionEvent};
use crate::edge_spawn::{
    ArgumentsError, Child, ExactError, LaunchStorage, SpawnError, spawn_exact, spawn_exact_ready,
};
use crate::host_event::{HostEvent, StaleHostEvent};
use crate::outcomes::{OutcomeError, OutcomeLedger, StartError, TerminalOutcome};
use crate::reporter::{ReportError, write_report};
use crate::schedule::{SessionSlotId, SessionSlots};
use crate::test_intent::ExactTestIntent;

/// One owned child and its identity retained while the listing chunk is reused.
#[derive(Debug)]
pub struct RunningChild<const NAME_CAPACITY: usize> {
    child: Child,
    name: [u8; NAME_CAPACITY],
    name_length: usize,
    target: usize,
    completion: ChildCompletion,
    exit_status: Option<i32>,
}

impl<const NAME_CAPACITY: usize> RunningChild<NAME_CAPACITY> {
    /// Ordinal of the test binary this child was listed from.
    #[must_use]
    pub const fn target(&self) -> usize {
        self.target
    }

    /// Returns the child watched by the platform event source.
    #[must_use]
    pub const fn child(&self) -> &Child {
        &self.child
    }

    /// Returns the selected name from fixed session storage.
    #[must_use]
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_length]
    }

    /// Takes ownership when the child is retired from its slot.
    #[must_use]
    pub fn into_child(self) -> Child {
        self.child
    }
}

/// Launch rejection, with a live child returned if insertion cannot complete.
#[derive(Debug)]
pub enum LaunchError<const NAME_CAPACITY: usize> {
    /// No live-child slot is available before the OS launch.
    Full,
    /// The same test name is already live in this session table.
    Duplicate,
    /// Inline identity storage is too short for this test name.
    NameTooLong { required: usize, available: usize },
    /// The OS rejected child creation.
    Spawn(SpawnError),
    /// The launch data holds a NUL byte, a malformed environment entry, or more than the fixed storage holds.
    Arguments(ArgumentsError),
    /// An unexpected slot rejection returns the spawned child to the owner.
    CapacityChanged(RunningChild<NAME_CAPACITY>),
    /// Ledger admission failed without losing ownership of the spawned child.
    Accounting(StartError<RunningChild<NAME_CAPACITY>>),
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
    /// The OS refused the launch with this raw error code.
    Spawn { os_code: i32 },
    /// The launch data could not be lowered into the fixed storage.
    Arguments(ArgumentsError),
    /// The slot table rejected a child that the capacity check had admitted.
    CapacityChanged,
    /// Outcome accounting refused the spawned child.
    Accounting(OutcomeError),
}

impl<const NAME_CAPACITY: usize> From<&LaunchError<NAME_CAPACITY>> for LaunchCause {
    fn from(error: &LaunchError<NAME_CAPACITY>) -> Self {
        match error {
            LaunchError::Full => Self::Full,
            LaunchError::Duplicate => Self::Duplicate,
            LaunchError::NameTooLong {
                required,
                available,
            } => Self::NameTooLong {
                required: *required,
                available: *available,
            },
            LaunchError::Spawn(error) => Self::Spawn { os_code: error.os_code },
            LaunchError::Arguments(error) => Self::Arguments(*error),
            LaunchError::CapacityChanged(_) => Self::CapacityChanged,
            LaunchError::Accounting(error) => Self::Accounting(error.error),
        }
    }
}

/// One owner of the same live child table during exact launch.
pub enum LaunchOwner<'call, 'ledger, const NAME_CAPACITY: usize, const CAPACITY: usize> {
    /// Direct session ownership before outcome accounting is attached.
    Sessions(&'call mut SessionSlots<RunningChild<NAME_CAPACITY>, CAPACITY>),
    /// Outcome accounting owns the same table and counts this child on admission.
    Ledger(&'call mut OutcomeLedger<'ledger, RunningChild<NAME_CAPACITY>, CAPACITY>),
}

impl<const NAME_CAPACITY: usize, const CAPACITY: usize>
    LaunchOwner<'_, '_, NAME_CAPACITY, CAPACITY>
{
    fn sessions(&self) -> &SessionSlots<RunningChild<NAME_CAPACITY>, CAPACITY> {
        match self {
            Self::Sessions(sessions) => sessions,
            Self::Ledger(ledger) => ledger.sessions(),
        }
    }

    fn admit(
        &mut self,
        child: RunningChild<NAME_CAPACITY>,
        names: usize,
    ) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        match self {
            Self::Sessions(sessions) => sessions
                .insert(child)
                .map_err(LaunchError::CapacityChanged),
            Self::Ledger(ledger) => ledger.start_many(child, names).map_err(LaunchError::Accounting),
        }
    }
}

/// Borrowed launch input and caller-owned OS representation storage.
pub struct LaunchInput<
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
    pub owner: LaunchOwner<'call, 'ledger, NAME_CAPACITY, CAPACITY>,
    /// Borrowed test path and name from the listing source.
    pub intent: ExactTestIntent<'executable, 'scratch, 'name>,
    /// Borrowed inherited environment entries in `KEY=VALUE` form.
    pub inherited_environment: &'call [&'call [u8]],
    /// Borrowed child-only environment entries that override inherited keys.
    pub child_overrides: &'call [&'call [u8]],
    /// Fixed argv, envp, and string storage reused by every launch of the run.
    pub storage: &'call mut LaunchStorage,
    /// Borrowed test-harness flags appended after the exact test name.
    pub harness_arguments: &'call [&'call [u8]],
    /// Directory the child starts in.
    pub current_directory: &'call [u8],
}

impl<'call, 'ledger: 'call, const NAME_CAPACITY: usize, const CAPACITY: usize>
    LaunchInput<'call, 'ledger, '_, '_, '_, NAME_CAPACITY, CAPACITY>
{
    /// Starts one exact child and places it directly in a slot.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] when the table is full, the name is live or too long, the spawn fails, or accounting refuses the child.
    pub fn launch(self) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        self.launch_as(false)
    }

    /// Starts one exact child like [`Self::launch`], creating it frozen when `ready` so [`crate::edge_spawn::resume_ready`] starts it later.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] when the table is full, the name is live or too long, the spawn fails, or accounting refuses the child.
    pub fn launch_as(mut self, ready: bool) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        if self.owner.sessions().is_full() {
            return Err(LaunchError::Full);
        }
        let name_bytes = self.intent.test_name().as_bytes();
        if self
            .owner
            .sessions()
            .any(|session| session.target == self.target && session.name_bytes() == name_bytes)
        {
            return Err(LaunchError::Duplicate);
        }
        if name_bytes.len() > NAME_CAPACITY {
            return Err(LaunchError::NameTooLong {
                required: name_bytes.len(),
                available: NAME_CAPACITY,
            });
        }

        let mut name = [0; NAME_CAPACITY];
        name[..name_bytes.len()].copy_from_slice(name_bytes);
        let spawn = if ready { spawn_exact_ready } else { spawn_exact };
        let child = spawn(
            self.storage,
            executable_bytes(&self.intent),
            name_bytes,
            self.harness_arguments,
            self.inherited_environment,
            self.child_overrides,
            self.current_directory,
        )
        .map_err(|error| match error {
            ExactError::Arguments(error) => LaunchError::Arguments(error),
            ExactError::Spawn(error) => LaunchError::Spawn(error),
        })?;
        let session = RunningChild {
            child,
            name,
            name_length: name_bytes.len(),
            target: self.target,
            completion: ChildCompletion::new(),
            exit_status: None,
        };
        self.owner.admit(session, 1)
    }

    /// Starts one shard child that runs `count` tests, whose names the intent carries joined by NUL, and places it directly in a slot.
    ///
    /// The shard is accounted as `count` discovered tests; each reaches its terminal count as its result line streams in.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError`] when the table is full, the spawn fails, the names do not fit the launch storage, or accounting refuses the child.
    pub fn launch_shard(mut self, count: usize) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        if self.owner.sessions().is_full() {
            return Err(LaunchError::Full);
        }
        let child = spawn_exact(
            self.storage,
            executable_bytes(&self.intent),
            self.intent.test_name().as_bytes(),
            self.harness_arguments,
            self.inherited_environment,
            self.child_overrides,
            self.current_directory,
        )
        .map_err(|error| match error {
            ExactError::Arguments(error) => LaunchError::Arguments(error),
            ExactError::Spawn(error) => LaunchError::Spawn(error),
        })?;
        let session = RunningChild {
            child,
            name: [0; NAME_CAPACITY],
            name_length: 0,
            target: self.target,
            completion: ChildCompletion::new(),
            exit_status: None,
        };
        self.owner.admit(session, count)
    }
}

fn executable_bytes<'intent>(intent: &ExactTestIntent<'intent, 'intent, '_>) -> &'intent [u8] {
    match intent.executable() {
        ExecutablePath::Borrowed(path) | ExecutablePath::Scratch(path) => path.as_bytes(),
    }
}

/// One accepted child event or one terminal label written to caller storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStep<'bytes> {
    /// Borrowed stdout bytes retain their host buffer address.
    Stdout(&'bytes [u8]),
    /// Borrowed stderr bytes retain their host buffer address.
    Stderr(&'bytes [u8]),
    /// Exit or one stream EOF was accepted while another event is still needed.
    Pending,
    /// A shard child's streams and exit completed; its tests were counted as their result lines arrived, so nothing is counted here.
    Complete {
        /// The shard child's exit status.
        status: i32,
    },
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
pub enum ChildOutcomeError {
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
    /// The child ran many tests and reported each as it finished; completion yields its exit status and counts nothing.
    Shard,
}

/// One borrowed host event and the sole mutable outcome owner.
pub struct OutcomeInput<
    'call,
    'ledger,
    'bytes,
    const NAME_CAPACITY: usize,
    const CAPACITY: usize,
> {
    /// Coordinator-owned accounting and live child sessions.
    pub ledger: &'call mut OutcomeLedger<'ledger, RunningChild<NAME_CAPACITY>, CAPACITY>,
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
    OutcomeInput<'call, 'ledger, 'bytes, NAME_CAPACITY, CAPACITY>
{
    /// Joins host validation, child completion, accounting, and reporting.
    ///
    /// # Errors
    ///
    /// Returns [`ChildOutcomeError`] when the event is stale or the ledger refuses the transition.
    pub fn apply(self) -> Result<OutcomeStep<'bytes>, ChildOutcomeError> {
        let accepted = self
            .event
            .admit(self.ledger.sessions())
            .map_err(ChildOutcomeError::Stale)?;
        let slot = accepted.slot();
        let (completion_event, exit_status) = match accepted {
            HostEvent::Stdout { bytes, .. } => {
                Self::record_bytes(self.ledger, slot, ChildCompletionEvent::StdoutBytes)?;
                return Ok(OutcomeStep::Stdout(bytes));
            }
            HostEvent::Stderr { bytes, .. } => {
                Self::record_bytes(self.ledger, slot, ChildCompletionEvent::StderrBytes)?;
                return Ok(OutcomeStep::Stderr(bytes));
            }
            HostEvent::StdoutEof { .. } => (ChildCompletionEvent::StdoutEof, None),
            HostEvent::StderrEof { .. } => (ChildCompletionEvent::StderrEof, None),
            HostEvent::Exit { status, .. } => (ChildCompletionEvent::Exit, Some(status)),
        };
        let (previous_completion, previous_status, complete) = {
            let session = self
                .ledger
                .session_mut(slot)
                .ok_or(ChildOutcomeError::Accounting(OutcomeError::UnknownSession))?;
            let previous_completion = session.completion;
            let previous_status = session.exit_status;
            let complete = session
                .completion
                .apply(completion_event)
                .map_err(ChildOutcomeError::InvalidCompletion)?;
            if let Some(status) = exit_status {
                session.exit_status = Some(status);
            }
            (previous_completion, previous_status, complete)
        };
        if !complete {
            return Ok(OutcomeStep::Pending);
        }

        let status = self
            .ledger
            .sessions()
            .get(slot)
            .and_then(|session| session.exit_status)
            .ok_or(ChildOutcomeError::MissingExitStatus)?;
        let outcome = match (status, self.verdict) {
            (_, LibtestVerdict::Shard) => return Ok(OutcomeStep::Complete { status }),
            (0, LibtestVerdict::NothingRan) => return Err(ChildOutcomeError::NothingRan),
            (0, LibtestVerdict::Skipped) => TerminalOutcome::Ignored,
            (0, _) => TerminalOutcome::Passed,
            _ => TerminalOutcome::Failed,
        };
        let report = self
            .ledger
            .can_finish(slot, outcome)
            .map_err(ChildOutcomeError::Accounting)
            .and_then(|()| {
                write_report(&[outcome], self.color, self.report_output)
                    .map_err(ChildOutcomeError::Report)
            });
        let report_bytes = match report {
            Ok(report_bytes) => report_bytes,
            Err(error) => {
                let session = self
                    .ledger
                    .session_mut(slot)
                    .ok_or(ChildOutcomeError::Accounting(OutcomeError::UnknownSession))?;
                session.completion = previous_completion;
                session.exit_status = previous_status;
                return Err(error);
            }
        };
        self.ledger
            .finish(slot, outcome)
            .map_err(ChildOutcomeError::Accounting)?;
        Ok(OutcomeStep::Reported {
            outcome,
            report_bytes,
        })
    }

    fn record_bytes(
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
        slot: SessionSlotId,
        event: ChildCompletionEvent,
    ) -> Result<(), ChildOutcomeError> {
        let session = ledger
            .session_mut(slot)
            .ok_or(ChildOutcomeError::Accounting(OutcomeError::UnknownSession))?;
        session
            .completion
            .apply(event)
            .map(|_| ())
            .map_err(ChildOutcomeError::InvalidCompletion)
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
    use core::pin::Pin;
    use core::task::{Context, Poll, Waker};

    use proxima_core::io::AsyncRead;

    use super::{
        LibtestVerdict, RunningChild, LaunchError, LaunchInput, LaunchOwner,
        ChildOutcomeError, OutcomeInput, OutcomeStep,
    };
    use crate::ExecutablePath;
    use crate::edge_spawn::held_launch_storage;
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
        sessions: &mut SessionSlots<RunningChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
    ) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        launch_target(sessions, intent, 0)
    }

    fn launch_target<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        sessions: &mut SessionSlots<RunningChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
        target: usize,
    ) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        let overrides = [b"CARGO_VAL_C51A_CHILD=1".as_slice()];
        LaunchInput {
            target,
            owner: LaunchOwner::Sessions(sessions),
            intent,
            inherited_environment: &[],
            child_overrides: &overrides,
            storage: &mut held_launch_storage(),
            harness_arguments: &[],
            current_directory: b"/",
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
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
        child_overrides: &[&[u8]],
    ) -> Result<SessionSlotId, LaunchError<NAME_CAPACITY>> {
        LaunchInput {
            target: 0,
            owner: LaunchOwner::Ledger(ledger),
            intent,
            inherited_environment: &[],
            child_overrides,
            storage: &mut held_launch_storage(),
            harness_arguments: &[],
            current_directory: b"/",
        }
        .launch()
    }

    fn deliver<'bytes, const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
        event: HostEvent<'bytes>,
        report_output: &mut [u8],
    ) -> Result<OutcomeStep<'bytes>, ChildOutcomeError> {
        OutcomeInput {
            ledger,
            event,
            report_output,
            color: false,
            verdict: LibtestVerdict::Ran,
        }
        .apply()
    }

    fn consume_stream<const NAME_CAPACITY: usize, const CAPACITY: usize>(
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
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
                    OutcomeStep::Stdout(accepted) | OutcomeStep::Stderr(accepted) => {
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
                OutcomeStep::Pending => (total_bytes, borrowed_reads, None),
                OutcomeStep::Reported {
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
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
        intent: ExactTestIntent<'_, '_, '_>,
        child_overrides: &[&[u8]],
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
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
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
            OutcomeStep::Pending,
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
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, CAPACITY>,
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

    fn watch(process_id: u32) -> NonZeroI32 {
        NonZeroI32::new(process_id.cast_signed()).expect("spawned child has a process id")
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

    #[proxima::test]
    async fn c51a_launches_exact_child_before_listing_eof_and_stops_at_full_slot() {
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
        let mut sessions = SessionSlots::<RunningChild<64>, 1>::new();
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
                    Err(LaunchError::Full)
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

    #[proxima::test]
    async fn same_test_name_from_two_targets_is_live_twice() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningChild<64>, 3>::new();
        let first = launch_target(&mut sessions, intent(executable, ALPHA_NAME), 0)
            .expect("first target starts alpha");
        let second = launch_target(&mut sessions, intent(executable, ALPHA_NAME), 1)
            .expect("second target starts the same name while the first is live");
        assert_ne!(first, second);
        assert!(matches!(
            launch_target(&mut sessions, intent(executable, ALPHA_NAME), 1),
            Err(LaunchError::Duplicate)
        ));
        assert!(matches!(
            launch_target(&mut sessions, intent(executable, ALPHA_NAME), 0),
            Err(LaunchError::Duplicate)
        ));
        assert_eq!(sessions.len(), 2);
    }

    #[proxima::test]
    async fn c51a_duplicate_and_name_capacity_controls_reject_before_spawn() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningChild<64>, 2>::new();
        let alpha = launch(&mut sessions, intent(executable, ALPHA_NAME)).expect("alpha starts");
        let first_process_id = sessions
            .get(alpha)
            .expect("alpha stored")
            .child()
            .process_id();
        let first_watch = watch(first_process_id);
        assert!(matches!(
            launch(&mut sessions, intent(executable, ALPHA_NAME)),
            Err(LaunchError::Duplicate)
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
        let mut short_sessions = SessionSlots::<RunningChild<8>, 1>::new();
        assert!(matches!(
            launch(&mut short_sessions, intent(executable, ALPHA_NAME)),
            Err(LaunchError::NameTooLong { required, available: 8 }) if required == ALPHA_NAME.len()
        ));
        assert_eq!(short_sessions.len(), 0);
        std::println!(
            "{{\"duplicate_rejections\":1,\"distinct_launches\":2,\"capacity_rejections\":1,\"post_rejection_slots\":0,\"captured_stream_eofs\":4}}"
        );
    }

    #[proxima::test]
    async fn c51b_accounts_pass_and_fail_after_exit_and_both_stream_eofs() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningChild<64>, 2>::new();
        let mut ledger = OutcomeLedger::new(&mut sessions).expect("table starts empty");

        let (passed, passed_watch) = launch_watched(
            &mut ledger,
            intent(executable, ALPHA_NAME),
            &[b"CARGO_VAL_C51A_CHILD=1"],
        );
        let (failed, failed_watch) = launch_watched(
            &mut ledger,
            intent(executable, FAIL_NAME),
            &[b"CARGO_VAL_C51B_FAIL=1"],
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
            OutcomeStep::Reported {
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

    #[proxima::test]
    async fn c51b_duplicate_exit_is_rejected_while_child_remains_live() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningChild<64>, 1>::new();
        let mut ledger = OutcomeLedger::new(&mut sessions).expect("table starts empty");
        let slot = launch_accounted(
            &mut ledger,
            intent(executable, ALPHA_NAME),
            &[b"CARGO_VAL_C51A_CHILD=1"],
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
            Ok(OutcomeStep::Pending)
        );
        assert_eq!(
            deliver(&mut ledger, exit, &mut report),
            Err(ChildOutcomeError::InvalidCompletion(
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
            Err(ChildOutcomeError::InvalidCompletion(
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
            Err(ChildOutcomeError::Report(
                crate::reporter::ReportError::OutputTooSmall
            )),
        );
        assert_eq!(ledger.unaccounted(), 1);
        assert!(ledger.sessions().get(slot).is_some());
        assert_eq!(
            deliver(&mut ledger, HostEvent::StderrEof { slot }, &mut report),
            Ok(OutcomeStep::Reported {
                outcome: TerminalOutcome::Passed,
                report_bytes: 5,
            }),
        );
        assert_eq!(ledger.unaccounted(), 0);
        assert_eq!(ledger.snapshot().passed, 1);
        assert_eq!(ledger.snapshot().failed, 0);
        assert!(matches!(
            deliver(&mut ledger, exit, &mut report),
            Err(ChildOutcomeError::Stale(_))
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

    #[proxima::test]
    async fn fixture_alpha() {
        if std::env::var_os("CARGO_VAL_C51A_CHILD").is_some() {
            write_marker(b"cargo-val-c51a-child");
        }
    }

    #[proxima::test]
    async fn fixture_beta() {
        if std::env::var_os("CARGO_VAL_C51A_CHILD").is_some() {
            write_marker(b"cargo-val-c51a-child");
        }
    }

    #[proxima::test]
    async fn fixture_fail() {
        if std::env::var_os("CARGO_VAL_C51B_FAIL").is_some() {
            write_marker(b"cargo-val-c51b-failure");
            panic!("cargo-val-c51b-fixture-failure");
        }
    }
}

#[cfg(all(test, feature = "proxima-core"))]
mod cause_tests {
    use super::{
        LaunchCause, RunningChild, LaunchError, LaunchInput, LaunchOwner,
        SpawnError,
    };
    use crate::ExecutablePath;
    use crate::edge_spawn::held_launch_storage;
    use crate::outcomes::{OutcomeError, StartError};
    use crate::schedule::SessionSlots;
    use crate::test_intent::ExactTestIntent;

    fn live_child() -> RunningChild<64> {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let mut sessions = SessionSlots::<RunningChild<64>, 1>::new();
        let slot = LaunchInput {
            target: 0,
            owner: LaunchOwner::Sessions(&mut sessions),
            intent: ExactTestIntent::new(
                ExecutablePath::Borrowed(executable),
                "runner::cause_tests::absent_name",
            ),
            inherited_environment: &[],
            child_overrides: &[],
            storage: &mut held_launch_storage(),
            harness_arguments: &[],
            current_directory: b"/",
        }
        .launch()
        .expect("the test executable starts");
        sessions.remove(slot).expect("the child is stored")
    }

    #[proxima::test]
    async fn every_launch_error_variant_maps_to_its_cause() {
        let cases: [(LaunchError<64>, LaunchCause); 6] = [
            (LaunchError::Full, LaunchCause::Full),
            (LaunchError::Duplicate, LaunchCause::Duplicate),
            (
                LaunchError::NameTooLong {
                    required: 70,
                    available: 64,
                },
                LaunchCause::NameTooLong {
                    required: 70,
                    available: 64,
                },
            ),
            (
                LaunchError::Spawn(SpawnError { os_code: 2 }),
                LaunchCause::Spawn { os_code: 2 },
            ),
            (
                LaunchError::CapacityChanged(live_child()),
                LaunchCause::CapacityChanged,
            ),
            (
                LaunchError::Accounting(StartError {
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
        assert_eq!(checked, 6);
    }
}

#[cfg(all(test, unix, feature = "edge-os", feature = "proxima-core"))]
mod arguments_cause_tests {
    use super::{LaunchCause, LaunchError};
    use crate::edge_spawn::ArgumentsError;

    #[proxima::test]
    async fn an_arguments_refusal_keeps_its_required_and_available_sizes() {
        let error = LaunchError::<64>::Arguments(ArgumentsError::StringBytesTooSmall {
            required: 140_000,
            available: 65_536,
        });

        assert_eq!(
            LaunchCause::from(&error),
            LaunchCause::Arguments(ArgumentsError::StringBytesTooSmall {
                required: 140_000,
                available: 65_536,
            })
        );
    }
}

#[cfg(all(test, unix, feature = "edge-os", feature = "proxima-core"))]
mod launch_allocation_tests {
    use super::{LaunchInput, LaunchOwner, RunningChild};
    use crate::ExecutablePath;
    use crate::edge_spawn::held_launch_storage;
    use crate::schedule::SessionSlots;
    use crate::test_intent::ExactTestIntent;

    #[proxima::test]
    async fn launching_an_exact_test_child_makes_zero_heap_allocations() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let inherited_storage = std::env::vars()
            .map(|(key, value)| std::format!("{key}={value}"))
            .collect::<std::vec::Vec<_>>();
        let inherited = inherited_storage
            .iter()
            .map(std::string::String::as_bytes)
            .collect::<std::vec::Vec<_>>();
        let mut sessions = SessionSlots::<RunningChild<64>, 1>::new();
        let mut storage = held_launch_storage();

        let (slot, allocations) = crate::test_alloc::measure_allocations(|| {
            LaunchInput {
                target: 0,
                owner: LaunchOwner::Sessions(&mut sessions),
                intent: ExactTestIntent::new(
                    ExecutablePath::Borrowed(executable),
                    "runner::cause_tests::absent_name",
                ),
                inherited_environment: &inherited,
                child_overrides: &[b"CARGO_VAL_CHILD_MODE=1"],
                storage: &mut storage,
                harness_arguments: &[b"--ignored"],
                current_directory: b"/",
            }
            .launch()
        });

        let child = sessions
            .remove(slot.expect("the test executable starts"))
            .expect("the child is stored")
            .into_child();
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child.process_id().cast_signed(), &raw mut status, 0) },
            child.process_id().cast_signed()
        );
        std::println!("{{\"launch_input_allocations\":{allocations},\"inherited_entries\":{}}}", inherited.len());
        assert_eq!(allocations, 0);
    }
}
