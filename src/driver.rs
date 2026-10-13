//! One poll-driven owner of Cargo discovery, libtest listings, and child outcomes.

use core::cell::{Cell, RefCell};
use core::ops::Range;
use core::task::{Context, Poll, Waker};

use conflaguration::{BorrowedSettings, SettingsSource, resolve_color};

use crate::ExecutablePath;
use crate::artifact_stream::{ArtifactFramer, RecordStep};
use crate::cargo_message::TestArtifact;
use crate::cargo_source::{CargoSource, CargoSourceStep};
use crate::cli::{CliParser, DocSelection};
use crate::config::{
    FailFastError, FailFastSources, FiltersetSources, HarnessFault, HarnessRequest, IgnoreError,
    IgnoreMode, IgnoreSources, NoTestsError, NoTestsPolicy, NoTestsSources, ProcessError, ProcessMode,
    ProcessSources, ThreadSources,
    TargetDirError, TargetDirMode, TargetDirSources, ThreadsError, ThreadsFault, ThreadsSource,
};
use crate::discover_listing::{
    DocEvent, DocOutcome, DocParser, ListingLine, ListingSource, ListingSourceStep,
    parse_listing_line,
};
use crate::discover_cargo::{BinGate, DiscoveryFsm};
use crate::filterset::{FilterError, Filterset, Prepared};
use crate::ignored_set::{IgnoredSet, IgnoredSetFull};
use crate::isolated_target::{BuildDirectories, ChildDirectory, LockKind, LockWatch};
use crate::line::Line;
use crate::run::{
    CARGO_LANE, JOB_BASE, LISTING_LANE, Lanes, attach, attach_session, collect_child, collect_libdir,
    observe_cargo, observe_doc,
};
use crate::metadata_scan::{BinCounts, MetadataFacts, MetadataScan};
use prime::os::readiness::Readiness;
use proxima_primitives::pipe::SignalSource;
use std::boxed::Box;
use std::sync::OnceLock;
use std::time::Instant;
use std::vec::Vec;
use crate::parked_artifacts::{ParkFault, ParkedArtifacts};
use crate::status_queue::{self, EVENT_RESERVE};
use crate::summary::{Counts, Tally, TargetTable, render as render_counts};
use crate::edge_signal::{
    SignalPipe, conventional_status, interrupt_group, release_child, release_group, track_child, track_group,
};
use crate::edge_args::{DYLIB_PATH_KEY, Environment};
use crate::edge_spawn::{
    ArgumentsError, Child, HeldLaunchStorage, LaunchStorage, READY_AHEAD, SpawnError, held_launch_storage, resume_ready, spawn,
    spawn_in_group, spawn_listed,
};
use crate::host_event::HostEvent;
use crate::outcomes::{OutcomeError, OutcomeLedger, TerminalOutcome};
use crate::reporter::write_report;
use crate::runner::{
    LaunchCause, LibtestVerdict, RunningChild, LaunchInput, LaunchOwner,
    ChildOutcomeError, OutcomeInput, OutcomeStep,
};
use crate::schedule::{SessionSlotId, SessionSlots};
use crate::sched_binary::{GROUP_CAPACITY, RECORD_BYTES};
use crate::shard_arena::{DECLARED_BYTES, ShardArena};
use crate::shard_sched::{SchedTable, SpecialPlan};

mod shard;
use shard::ShardWatch;
use crate::selection::{NameFilter, Selection};
use crate::stop::{self, DocAtTrip, Observation, StopPolicy, StopReport};
use crate::test_environment::{
    BuildScriptOutput, BuildScriptOutputs, LinkedDirectories, MAX_ENTRIES,
    TestEnvironmentInput, TestEnvironmentLayout,
};
use crate::test_intent::ExactTestIntent;

pub(crate) const LIVE_CAPACITY: usize = 64;
const IGNORED_ARGUMENTS: [&[u8]; 1] = [b"--ignored"];
const INCLUDE_ARGUMENTS: [&[u8]; 1] = [b"--include-ignored"];
const SHARD_ARGUMENTS: [&[u8]; 1] = [b"--test-threads=1"];
const SHARD_ONLY_ARGUMENTS: [&[u8]; 2] = [b"--ignored", b"--test-threads=1"];
const SHARD_INCLUDE_ARGUMENTS: [&[u8]; 2] = [b"--include-ignored", b"--test-threads=1"];
const IDENTITY_CAPACITY: usize = 128;
pub(crate) const NAME_CAPACITY: usize = 1024;
const ENV_CAPACITY: usize = 1024;
const PATH_CAPACITY: usize = 4096;
const SCRATCH_CAPACITY: usize = 16384;
const LINKED_CAPACITY: usize = 32768;
const BUILD_SCRIPT_CAPACITY: usize = 65536;
const ENVIRONMENT_CAPACITY: usize = LINKED_CAPACITY + 16384;
const LISTING_SLOTS: usize = 2;
const LIBDIR_CAPACITY: usize = 1024;
const SOURCE_CAPACITY: usize = 8192;
const FRAME_CAPACITY: usize = 131_072;
pub(crate) const CAPTURE_CAPACITY: usize = 4096;
const RESULT_LINE_CAPACITY: usize = NAME_CAPACITY + 64;

struct Identity {
    package: [u8; IDENTITY_CAPACITY],
    package_length: usize,
    binary: [u8; IDENTITY_CAPACITY],
    binary_length: usize,
}

impl Identity {
    fn new(package: &str, binary: &str) -> Option<Self> {
        if package.len() > IDENTITY_CAPACITY || binary.len() > IDENTITY_CAPACITY {
            return None;
        }
        let mut identity = Self {
            package: [0; IDENTITY_CAPACITY],
            package_length: package.len(),
            binary: [0; IDENTITY_CAPACITY],
            binary_length: binary.len(),
        };
        identity.package[..package.len()].copy_from_slice(package.as_bytes());
        identity.binary[..binary.len()].copy_from_slice(binary.as_bytes());
        Some(identity)
    }

    fn package(&self) -> &str {
        core::str::from_utf8(&self.package[..self.package_length]).unwrap_or("")
    }

    fn binary(&self) -> &str {
        core::str::from_utf8(&self.binary[..self.binary_length]).unwrap_or("")
    }
}

/// How far a listing child's stdout has been read: still open, closed with its last lines undrained, or fully consumed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StdoutState {
    Open,
    Ended,
    Drained,
}

struct PendingListing {
    target: usize,
    child: Option<Child>,
    ignored_child: Option<Child>,
    identity: Identity,
    path: [u8; PATH_CAPACITY],
    path_length: usize,
    slot: usize,
    layout: TestEnvironmentLayout,
    ignoring: bool,
    lane_pid: u32,
    stdout: StdoutState,
    stderr_done: bool,
    exit: Option<i32>,
    backlog: Option<(Range<u32>, usize)>,
}

/// Run-mode failure category that the `no_std` binary can report without allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunError {
    Arguments,
    Environment,
    TestEnvironment,
    RustcLibdir,
    Settings,
    Ignore(IgnoreError),
    TargetDir(TargetDirError),
    Filterset(FilterError),
    IgnoredSet(IgnoredSetFull),
    NothingRan,
    SummaryMismatch,
    TargetTable,
    Parked(ParkFault),
    TestThreads(ThreadsError),
    FailFast(FailFastError),
    NoTests(NoTestsError),
    Process(ProcessError),
    DocTests,
    Spawn,
    Reactor,
    CargoStream,
    ListingStream,
    Launch(LaunchCause),
    Outcome,
    CargoFailed,
    Interrupted(i32),
    ListingFailed,
    DeclaredCapacity,
}

impl RunError {
    /// 2 for every refusal, `128 + signal` when a forwarded SIGINT or SIGTERM ended the run.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Interrupted(signal) => conventional_status(signal),
            _ => 2,
        }
    }

    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        match self {
            Self::Arguments => b"cargo-val: invalid arguments\n",
            Self::Environment => b"cargo-val: environment exceeds caller storage\n",
            Self::TestEnvironment => {
                b"cargo-val: test environment is malformed or exceeds caller storage\n"
            }
            Self::RustcLibdir => b"cargo-val: cannot read the rustc library directory\n",
            Self::Settings => b"cargo-val: invalid runtime settings\n",
            Self::Ignore(error) => error.message(),
            Self::TargetDir(error) => error.message(),
            Self::IgnoredSet(_) => b"cargo-val: a test binary lists more ignored tests than the fixed set holds\n",
            Self::NothingRan => b"cargo-val: an exact test launch ran no test body\n",
            Self::SummaryMismatch => b"cargo-val: test counts do not balance\n",
            Self::Parked(_) => b"cargo-val: too many test executables are waiting for their package bin targets, or a path is too long to hold\n",
            Self::TargetTable => b"cargo-val: more test binaries than the fixed table or a binary id is too long\n",
            Self::TestThreads(error) => error.message(),
            Self::FailFast(error) => error.message(),
            Self::NoTests(error) => error.message(),
            Self::Process(error) => error.message(),
            Self::DocTests => b"cargo-val: cargo test --doc failed before reporting results\n",
            Self::Spawn => b"cargo-val: child spawn failed\n",
            Self::Reactor => b"cargo-val: reactor operation failed\n",
            Self::CargoStream => b"cargo-val: cargo output could not be parsed\n",
            Self::ListingStream => b"cargo-val: libtest listing could not be parsed\n",
            Self::Launch(_) => b"cargo-val: exact test launch failed\n",
            Self::Outcome => b"cargo-val: child completion failed\n",
            Self::CargoFailed => b"cargo-val: cargo test build failed\n",
            Self::ListingFailed => b"cargo-val: libtest listing failed\n",
            Self::DeclaredCapacity => {
                b"cargo-val: declared test scheduling records exceed the build's capacity; raise CARGO_VAL_BUILD_SCHED_RECORD_BYTES\n"
            }
            Self::Filterset(_) | Self::Interrupted(_) => b"",
        }
    }
}

impl RunError {
    /// The refusal as one line, carrying the cause detail that the static message cannot.
    #[must_use]
    pub fn render(self) -> ErrorLine {
        let mut line = ErrorLine::new();
        match self {
            Self::Launch(cause) => {
                line.text(b"cargo-val: exact test launch failed: ");
                cause.write(&mut line);
                line.text(b"\n");
            }
            other => line.text(other.message()),
        }
        line
    }
}

impl LaunchCause {
    fn write<const CAPACITY: usize>(self, line: &mut Line<CAPACITY>) {
        match self {
            Self::Full => line.text(b"no live-test slot is free"),
            Self::Duplicate => line.text(b"the same test name is already live in this test binary"),
            Self::NameTooLong {
                required,
                available,
            } => {
                line.text(b"test name needs ");
                line.number(required);
                line.text(b" bytes and ");
                line.number(available);
                line.text(b" are available");
            }
            Self::Arguments(error) => write_arguments_error(line, error),
            Self::Spawn { os_code } => {
                line.text(b"spawn failed with os error ");
                line.number(usize::try_from(os_code.unsigned_abs()).unwrap_or(usize::MAX));
            }
            Self::CapacityChanged => {
                line.text(b"the live-test table rejected the child after its capacity check");
            }
            Self::Accounting(error) => {
                line.text(b"outcome accounting refused the child: ");
                line.text(accounting_text(error));
            }
        }
    }
}

fn write_arguments_error<const CAPACITY: usize>(line: &mut Line<CAPACITY>, error: ArgumentsError) {
    line.text(b"argument lowering failed: ");
    let (reason, sizes) = error.describe();
    line.text(reason);
    if let Some((required, available)) = sizes {
        line.text(b" ");
        line.number(required);
        line.text(b" and ");
        line.number(available);
        line.text(b" are available");
    }
}

const fn accounting_text(error: OutcomeError) -> &'static [u8] {
    match error {
        OutcomeError::CountOverflow => b"a counter reached its maximum",
        OutcomeError::UnknownSession => b"the session is not live",
        OutcomeError::CapacityExhausted => b"the live-session table is full",
        OutcomeError::ActiveSessions => b"sessions were already live",
    }
}

/// Room for the longest launch failure line the driver renders.
pub const ERROR_LINE_CAPACITY: usize = 512;

/// One rendered run refusal.
pub type ErrorLine = Line<ERROR_LINE_CAPACITY>;

const CONTEXT_CAPACITY: usize = NAME_CAPACITY + PATH_CAPACITY + 64;

fn executable_text<'path>(executable: ExecutablePath<'path, 'path>) -> &'path str {
    match executable {
        ExecutablePath::Borrowed(path) | ExecutablePath::Scratch(path) => path,
    }
}

#[inline(never)]
fn report_launch_failure(cause: LaunchCause, name: &str, executable: &str) -> RunError {
    let mut line = Line::<CONTEXT_CAPACITY>::new();
    line.text(b"cargo-val: launch refused for test ");
    line.text(name.as_bytes());
    line.text(b" in ");
    line.text(executable.as_bytes());
    line.text(b"\n");
    let _ = write_fd(2, line.bytes());
    RunError::Launch(cause)
}

/// Aggregated test process results from one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSummary {
    pub passed: usize,
    pub failed: usize,
    /// Present when `--max-fail` failures were reached before the selection finished.
    pub stop: Option<StopReport>,
    /// Present when the selection ran nothing, carrying the policy that applies.
    pub no_tests: Option<NoTestsPolicy>,
    /// Present when a doc-test child was part of the selection; its counts are already in the totals.
    pub doc_tests: Option<DocTotals>,
}

/// Exit status when the selection ran no tests and the policy is `fail`.
pub const NO_TESTS_EXIT_CODE: i32 = 4;

impl RunSummary {
    /// 1 when any test failed, `NO_TESTS_EXIT_CODE` when nothing ran under the `fail` policy, else 0.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.failed != 0 {
            1
        } else if matches!(self.no_tests, Some(NoTestsPolicy::Fail)) {
            NO_TESTS_EXIT_CODE
        } else {
            0
        }
    }
}

pub(crate) struct TestWatch {
    slot: SessionSlotId,
    process_id: u32,
    output: ChildOutput,
    shard: Option<ShardWatch>,
    group: [u8; GROUP_CAPACITY],
    group_length: usize,
}

impl TestWatch {
    fn in_group(&self, group: &[u8]) -> bool {
        self.group_length != 0 && &self.group[..self.group_length] == group
    }

    fn verdict(&self) -> LibtestVerdict {
        if self.shard.is_some() {
            LibtestVerdict::Shard
        } else {
            self.output.verdict()
        }
    }
}

struct Truncation {
    stdout: bool,
    stderr: bool,
}

struct ChildOutput {
    stdout: [u8; CAPTURE_CAPACITY],
    stderr: [u8; CAPTURE_CAPACITY],
    stdout_length: usize,
    stderr_length: usize,
    truncation: Truncation,
    result_line: [u8; RESULT_LINE_CAPACITY],
    result_line_length: usize,
    result_line_overflow: bool,
    result: Option<(usize, usize, usize)>,
}

impl ChildOutput {
    const fn new() -> Self {
        Self {
            stdout: [0; CAPTURE_CAPACITY],
            stderr: [0; CAPTURE_CAPACITY],
            stdout_length: 0,
            stderr_length: 0,
            truncation: Truncation {
                stdout: false,
                stderr: false,
            },
            result_line: [0; RESULT_LINE_CAPACITY],
            result_line_length: 0,
            result_line_overflow: false,
            result: None,
        }
    }

    fn observe_stdout(&mut self, bytes: &[u8]) {
        let available = CAPTURE_CAPACITY - self.stdout_length;
        let retained = bytes.len().min(available);
        self.stdout[self.stdout_length..self.stdout_length + retained]
            .copy_from_slice(&bytes[..retained]);
        self.stdout_length += retained;
        self.truncation.stdout |= retained < bytes.len();

        for &byte in bytes {
            if byte == b'\n' {
                if !self.result_line_overflow {
                    let mut line = &self.result_line[..self.result_line_length];
                    if let Some(without_cr) = line.strip_suffix(b"\r") {
                        line = without_cr;
                    }
                    if let Some(counts) = parse_result_line(line) {
                        self.result = Some(counts);
                    }
                }
                self.result_line_length = 0;
                self.result_line_overflow = false;
            } else if !self.result_line_overflow {
                if self.result_line_length == RESULT_LINE_CAPACITY {
                    self.result_line_overflow = true;
                } else {
                    self.result_line[self.result_line_length] = byte;
                    self.result_line_length += 1;
                }
            }
        }
    }

    fn verdict(&self) -> LibtestVerdict {
        match self.result {
            None => LibtestVerdict::Unreported,
            Some((passed, failed, _)) if passed + failed > 0 => LibtestVerdict::Ran,
            Some((_, _, ignored)) if ignored > 0 => LibtestVerdict::Skipped,
            Some(_) => LibtestVerdict::NothingRan,
        }
    }

    fn observe_stderr(&mut self, bytes: &[u8]) {
        let available = CAPTURE_CAPACITY - self.stderr_length;
        let retained = bytes.len().min(available);
        self.stderr[self.stderr_length..self.stderr_length + retained]
            .copy_from_slice(&bytes[..retained]);
        self.stderr_length += retained;
        self.truncation.stderr |= retained < bytes.len();
    }

    fn write_failure(&self) -> Result<(), RunError> {
        if self.stdout_length != 0 {
            write_fd(2, b"stdout:\n")?;
            write_fd(2, &self.stdout[..self.stdout_length])?;
            if self.truncation.stdout {
                write_fd(2, b"\n[stdout truncated]\n")?;
            }
        }
        if self.stderr_length != 0 {
            write_fd(2, b"stderr:\n")?;
            write_fd(2, &self.stderr[..self.stderr_length])?;
            if self.truncation.stderr {
                write_fd(2, b"\n[stderr truncated]\n")?;
            }
        }
        Ok(())
    }
}

// `test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 716 filtered out; finished in 0.00s`
fn parse_result_line(line: &[u8]) -> Option<(usize, usize, usize)> {
    let body = core::str::from_utf8(line.strip_prefix(b"test result: ")?).ok()?;
    let counted = |label: &str| -> Option<usize> {
        body.split("; ")
            .find_map(|part| part.trim_start_matches("ok. ").trim_start_matches("FAILED. ").strip_suffix(label))
            .and_then(|number| number.trim().parse().ok())
    };
    Some((counted(" passed")?, counted(" failed")?, counted(" ignored")?))
}

struct Settings {
    bin_gate: bool,
    debug: bool,
    ignore: IgnoreMode,
    filterset: Option<Filterset>,
    jobs: usize,
    max_fail: Option<usize>,
    no_tests: NoTestsPolicy,
    process: ProcessMode,
    color: bool,
}

fn runtime_settings(
    environment: &Environment<'_>,
    harness: &HarnessRequest<'_>,
    parser: &CliParser<'_>,
) -> Result<Settings, RunError> {
    let mut pairs = [("", ""); 12];
    let mut count = 0;
    for key in [
        "RUST_TEST_THREADS",
        "CARGO_VAL_JOBS",
        "CARGO_VAL_MAX_FAIL",
        "CARGO_VAL_FAIL_FAST",
        "CARGO_VAL_NO_TESTS",
        "CARGO_VAL_PROCESS",
        "CARGO_VAL_COLOR",
        "NO_COLOR",
        "CARGO_VAL_IGNORED",
        "CARGO_VAL_FILTERSET",
        "CARGO_VAL_BIN_GATE",
        "CARGO_VAL_LOG",
    ] {
        if let Some(value) = environment.get(key.as_bytes()) {
            pairs[count] = (
                key,
                core::str::from_utf8(value).map_err(|_| RunError::Settings)?,
            );
            count += 1;
        }
    }
    let source = BorrowedSettings::new(&pairs[..count]);
    let process = ProcessSources::from_settings(&source, parser.process(), crate::build_defaults::PROCESS)
        .resolve()
        .map_err(RunError::Process)?
        .setting
        .mode(cfg!(windows));
    // a shard child holds many tests, so shard mode keeps about twice as many children as cores in flight
    let default_jobs = match process {
        ProcessMode::Shard => {
            let cores = std::thread::available_parallelism().map_or(1, core::num::NonZero::get);
            crate::config::shard_jobs(crate::build_defaults::SHARD_JOBS, cores)
                .map(|count| count.min(LIVE_CAPACITY))
                .ok_or(RunError::TestThreads(ThreadsError {
                    source: ThreadsSource::BuildDefault,
                    fault: ThreadsFault::Malformed,
                }))?
        }
        ProcessMode::Test => crate::build_defaults::JOBS,
    };
    let jobs = ThreadSources::from_settings(&source, harness.test_threads, default_jobs)
        .resolve(LIVE_CAPACITY)
        .map_err(RunError::TestThreads)?
        .value;
    let max_fail = FailFastSources::from_settings(&source, parser.fail_fast())
        .resolve()
        .map_err(RunError::FailFast)?
        .max_fail;
    let no_tests = NoTestsSources::from_settings(&source, parser.no_tests())
        .resolve()
        .map_err(RunError::NoTests)?
        .policy;
    let color =
        resolve_color(&source, crate::build_defaults::COLOR).map_err(|_| RunError::Settings)?;
    let (ignore, _) = IgnoreSources::from_settings(&source, harness.ignore)
        .resolve()
        .map_err(RunError::Ignore)?;
    let filterset = FiltersetSources::from_settings(&source, parser.filterset())
        .resolve()
        .map(|text| {
            Filterset::parse(text).map_err(|error| {
                report_filterset_error(text, error);
                RunError::Filterset(error)
            })
        })
        .transpose()?;
    let bin_gate = match source.get("CARGO_VAL_BIN_GATE") {
        None | Some("on") => true,
        Some("off") => false,
        Some(_) => return Err(RunError::Settings),
    };
    let debug = match source.get("CARGO_VAL_LOG") {
        None | Some("off") => false,
        Some("debug") => true,
        Some(_) => return Err(RunError::Settings),
    };
    Ok(Settings {
        bin_gate,
        debug,
        ignore,
        filterset,
        jobs,
        max_fail,
        no_tests,
        process,
        color,
    })
}

#[inline(never)]
fn report_filterset_error(text: &[u8], error: FilterError) {
    let mut line = Line::<2048>::new();
    line.text(b"cargo-val: invalid filterset: ");
    line.text(error.kind.message());
    line.text(b"\n  ");
    line.text(text);
    line.text(b"\n  ");
    for _ in 0..error.offset.min(text.len()) {
        line.text(b" ");
    }
    line.text(b"^\n");
    let _ = write_fd(2, line.bytes());
}

pub(crate) fn write_fd(descriptor: i32, bytes: &[u8]) -> Result<(), RunError> {
    crate::status_queue::write(descriptor, bytes)
}

fn track(target: u32) -> Result<(), RunError> {
    track_child(target).map_err(|_| RunError::Reactor)
}

// a session's child runs on its own prime lane; its streams and exit arrive as items shaped like the events the outcome ledger already takes
fn start_session(
    ledger: &Ledger<'_>,
    watches: &mut Watches,
    lanes: &Lanes,
    slot: SessionSlotId,
    shard: Option<ShardWatch>,
) -> Result<(), RunError> {
    let child = ledger
        .sessions()
        .get(slot)
        .ok_or(RunError::Outcome)?
        .child();
    let process_id = child.process_id();
    track(process_id)?;
    attach_session(&lanes[JOB_BASE + slot.index()], child)?;
    watches[slot.index()] = Some(TestWatch {
        slot,
        process_id,
        output: ChildOutput::new(),
        shard,
        group: [0; GROUP_CAPACITY],
        group_length: 0,
    });
    Ok(())
}

pub(crate) fn session_output(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
    stdout: bool,
    (window, bytes): (Range<u32>, &[u8]),
) -> Result<(), RunError> {
    for (piece_index, piece) in bytes.chunks(SOURCE_CAPACITY).enumerate() {
        let Some(watch) = watches[index].as_mut() else {
            return Ok(());
        };
        let slot = watch.slot;
        let mut shard_tripped = false;
        let event = if stdout {
            watch.output.observe_stdout(piece);
            let start = piece_index * SOURCE_CAPACITY;
            let consumed = if shard::stalled(watch) {
                0
            } else {
                let observed = shard::observe(run, ledger, watch, piece)?;
                shard_tripped = observed.stop;
                observed.consumed
            };
            if consumed < piece.len() {
                shard::stall(watch, window.clone(), start + consumed);
            }
            HostEvent::Stdout { slot, bytes: piece }
        } else {
            watch.output.observe_stderr(piece);
            HostEvent::Stderr { slot, bytes: piece }
        };
        let step = deliver(ledger, event, run.reporting(), watch.verdict())?;
        if shard_tripped {
            shard::stop_all(run, ledger, watches);
        }
        if settled(run, ledger, watches, index, step)? {
            return Ok(());
        }
    }
    Ok(())
}

pub(crate) fn shard_stalled(watch: &TestWatch) -> bool {
    shard::stalled(watch)
}

/// Feeds the unframed remainder of a window whose framing paused, once the status queue has room again.
/// Returns whether the window finished, so the caller reopens the lane's gate.
pub(crate) fn session_resume(
    lanes: &Lanes,
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
) -> Result<bool, RunError> {
    let Some(watch) = watches[index].as_mut() else {
        return Ok(true);
    };
    if !shard::stalled(watch) || !status_queue::has_room() {
        return Ok(!shard::stalled(watch));
    }
    let Some((window, offset)) = shard::take_backlog(watch) else {
        return Ok(true);
    };
    let slot = lanes[JOB_BASE + index].0.borrow();
    let stream = slot.as_deref().ok_or(RunError::Reactor)?;
    let bytes = stream.window(window.clone()).ok_or(RunError::Reactor)?;
    let observed = shard::observe(run, ledger, watch, &bytes[offset..])?;
    if offset + observed.consumed < bytes.len() {
        shard::stall(watch, window, offset + observed.consumed);
    }
    if observed.stop {
        shard::stop_all(run, ledger, watches);
    }
    Ok(watches[index].as_ref().is_none_or(|watch| !shard::stalled(watch)))
}

pub(crate) fn session_ended(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
    stdout: bool,
) -> Result<(), RunError> {
    let Some(watch) = watches[index].as_ref() else {
        return Ok(());
    };
    let slot = watch.slot;
    let event = if stdout {
        HostEvent::StdoutEof { slot }
    } else {
        HostEvent::StderrEof { slot }
    };
    let step = deliver(ledger, event, run.reporting(), watch.verdict())?;
    settled(run, ledger, watches, index, step).map(|_| ())
}

pub(crate) fn session_exited(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
    code: i32,
) -> Result<(), RunError> {
    let Some(watch) = watches[index].as_ref() else {
        return Ok(());
    };
    release_child(watch.process_id);
    let stopped = run.stop_sent.get() && code == shard::STOP_STATUS;
    let never_started = run.withdrawn_ready.get() == Some(watch.slot);
    if run.settings.process == ProcessMode::Test && (stopped || never_started) {
        return withdraw_terminated(run, ledger, watches, index);
    }
    let event = HostEvent::Exit {
        slot: watch.slot,
        status: code,
    };
    let step = deliver(ledger, event, run.reporting(), watch.verdict())?;
    settled(run, ledger, watches, index, step).map(|_| ())
}

// a child cargo-val killed on interrupt has no verdict of its own, so it leaves the ledger uncounted and its name is withdrawn once
fn withdraw_terminated(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
) -> Result<(), RunError> {
    let Some(watch) = watches[index].take() else {
        return Ok(());
    };
    let target = ledger.sessions().get(watch.slot).map(RunningChild::target);
    ledger.retire(watch.slot).ok_or(RunError::Outcome)?;
    shard::withdraw_solo(run, ledger, target)
}

// an exact child retires with its one outcome; a shard child's tests were counted as they streamed, so the shard module settles its exit
fn settled(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
    step: OutcomeStep<'_>,
) -> Result<bool, RunError> {
    match step {
        OutcomeStep::Reported { outcome, .. } => {
            if outcome == TerminalOutcome::Failed
                && let Some(watch) = watches[index].as_ref()
            {
                watch.output.write_failure()?;
            }
            watches[index] = None;
            Ok(true)
        }
        OutcomeStep::Complete { status } => {
            shard::finish(run, ledger, watches, index, status)?;
            Ok(true)
        }
        OutcomeStep::Stdout(_) | OutcomeStep::Stderr(_) | OutcomeStep::Pending => Ok(false),
    }
}

const LOCK_LINE_CAPACITY: usize = 768;

// one line per child, so a wait on another cargo process is never silent
fn announce_lock_wait(watch: &mut LockWatch, bytes: &[u8]) -> Result<(), RunError> {
    let Some(kind) = watch.observe(bytes) else {
        return Ok(());
    };
    let mut line = Line::<LOCK_LINE_CAPACITY>::new();
    line.text(b"cargo-val: waiting for ");
    match kind {
        LockKind::PackageCache => {
            line.text(b"package cache held by another cargo process (global ~/.cargo lock, cannot be isolated)");
        }
        LockKind::BuildDirectory | LockKind::Other => {
            line.text(watch.which());
            line.text(b" held by another cargo process (");
            line.text(if watch.directory().is_empty() {
                &b"target directory unresolved"[..]
            } else {
                watch.directory()
            });
            line.text(b")");
        }
    }
    line.text(b"\n");
    write_fd(2, line.bytes())
}

/// Where a finished test's counters and color choice live.
#[derive(Clone, Copy)]
struct Reporting<'run> {
    color: bool,
    targets: &'run RefCell<TargetTable>,
    ready: &'run Cell<Option<SessionSlotId>>,
    withdrawn_ready: &'run Cell<Option<SessionSlotId>>,
    policy: &'run RefCell<StopPolicy>,
    doc_failures: &'run Cell<usize>,
    jobs: usize,
}

impl Reporting<'_> {
    // a frozen child counts as a live session but not as running, so it starts the moment a job slot frees
    // once the stop policy has tripped it is terminated instead: resuming it would launch a test after the trip
    fn settle_ready(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) {
        let Some(slot) = self.ready.get() else {
            return;
        };
        let Some(session) = ledger.sessions().get(slot) else {
            self.ready.set(None);
            return;
        };
        let failures = ledger.snapshot().failed + self.doc_failures.get();
        if self.policy.borrow().observe_failures(failures) != Observation::Clear {
            session.child().terminate();
            self.withdrawn_ready.set(Some(slot));
            self.ready.set(None);
            return;
        }
        if ledger.sessions().len().saturating_sub(1) < self.jobs {
            resume_ready(session.child());
            self.ready.set(None);
        }
    }
}

const fn event_slot(event: &HostEvent<'_>) -> SessionSlotId {
    match event {
        HostEvent::Stdout { slot, .. }
        | HostEvent::Stderr { slot, .. }
        | HostEvent::StdoutEof { slot }
        | HostEvent::StderrEof { slot }
        | HostEvent::Exit { slot, .. } => *slot,
    }
}

#[inline(never)]
fn report_nothing_ran(targets: &RefCell<TargetTable>, target: Option<usize>, name: &[u8]) -> RunError {
    let mut line = Line::<CONTEXT_CAPACITY>::new();
    line.text(b"cargo-val: test ");
    line.text(name);
    line.text(b" in ");
    if let Some(target) = target {
        line.text(targets.borrow().id(target));
    }
    line.text(b" exited 0 but libtest reported 0 passed, 0 failed, 0 ignored\n");
    let _ = write_fd(2, line.bytes());
    RunError::NothingRan
}

fn deliver<'bytes>(
    ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    event: HostEvent<'bytes>,
    reporting: Reporting<'_>,
    verdict: LibtestVerdict,
) -> Result<OutcomeStep<'bytes>, RunError> {
    let mut report = [0; NAME_CAPACITY + 34];
    let name_length = match event {
        HostEvent::Stdout { .. } | HostEvent::Stderr { .. } => 0,
        HostEvent::StdoutEof { slot }
        | HostEvent::StderrEof { slot }
        | HostEvent::Exit { slot, .. } => {
            let name = ledger
                .sessions()
                .get(slot)
                .ok_or(RunError::Outcome)?
                .name_bytes();
            report[..name.len()].copy_from_slice(name);
            report[name.len()] = b' ';
            name.len() + 1
        }
    };
    let slot = event_slot(&event);
    let target = ledger.sessions().get(slot).map(RunningChild::target);
    let step = OutcomeInput {
        ledger: &mut *ledger,
        event,
        report_output: &mut report[name_length..],
        color: reporting.color,
        verdict,
    }
    .apply()
    .map_err(|error| match error {
        ChildOutcomeError::NothingRan => {
            report_nothing_ran(reporting.targets, target, &report[..name_length.saturating_sub(1)])
        }
        _ => RunError::Outcome,
    })?;
    match step {
        OutcomeStep::Reported {
            outcome,
            report_bytes,
        } => {
            reporting.settle_ready(ledger);
            write_fd(1, &report[..name_length + report_bytes])?;
            if let Some(target) = target {
                reporting.targets.borrow_mut().tally(
                    target,
                    match outcome {
                        TerminalOutcome::Passed => Tally::Passed,
                        TerminalOutcome::Failed => Tally::Failed,
                        TerminalOutcome::Ignored => Tally::Skipped,
                    },
                );
            }
            Ok(step)
        }
        OutcomeStep::Complete { .. }
        | OutcomeStep::Pending
        | OutcomeStep::Stdout(_)
        | OutcomeStep::Stderr(_) => Ok(step),
    }
}

fn override_entries<'storage>(
    layout: &TestEnvironmentLayout,
    environment: &'storage [u8],
) -> Result<([&'storage [u8]; MAX_ENTRIES], usize), RunError> {
    let mut entries: [&[u8]; MAX_ENTRIES] = [b""; MAX_ENTRIES];
    for (index, entry) in entries.iter_mut().enumerate().take(layout.len()) {
        *entry = layout
            .entry(environment, index)
            .ok_or(RunError::TestEnvironment)?
            .as_bytes();
    }
    Ok((entries, layout.len()))
}

fn spawn_list(
    storage: &mut LaunchStorage,
    executable: &str,
    ignored: bool,
    run: &ListingRun<'_>,
    overrides: &[&[u8]],
    directory: &str,
) -> Result<Child, RunError> {
    let child = spawn_listed(
        storage,
        executable.as_bytes(),
        ignored,
        run.environment,
        overrides,
        directory.as_bytes(),
    )
    .map_err(|_| RunError::Spawn)?;
    track(child.process_id())?;
    Ok(child)
}

fn slot_range(slot: usize) -> Range<usize> {
    slot * ENVIRONMENT_CAPACITY..(slot + 1) * ENVIRONMENT_CAPACITY
}

fn free_slot(active: Option<&PendingListing>) -> usize {
    usize::from(active.is_some_and(|listing| listing.slot == 0))
}

fn spawn_listing(
    artifact: &TestArtifact<'_>,
    run: &ListingRun<'_>,
    linked_directories: &str,
    build_script: Option<BuildScriptOutput<'_>>,
    slot: usize,
    environments: &mut [u8],
) -> Result<PendingListing, RunError> {
    let target = run.policy.borrow_mut().target_listed();
    let binary_id = (!artifact.is_unit_test_target()).then_some(artifact.target_name);
    if !run
        .targets
        .borrow_mut()
        .begin(artifact.package_name(), binary_id)
    {
        return Err(RunError::TargetTable);
    }
    let identity = Identity::new(artifact.package_name(), artifact.target_name)
        .ok_or(RunError::TargetTable)?;
    let executable = artifact.executable;
    if executable.len() + 1 > PATH_CAPACITY || executable.as_bytes().contains(&0) {
        return Err(RunError::Arguments);
    }
    let environment = &mut environments[slot_range(slot)];
    let layout = TestEnvironmentLayout::build(
        &TestEnvironmentInput {
            artifact: *artifact,
            linked_directories,
            library_directories: run.library_directories,
            inherited_dylib_path: run.inherited_dylib_path,
            home: run.home,
            build_script,
        },
        environment,
    )
    .map_err(|_| RunError::TestEnvironment)?;
    let mut path = [0; PATH_CAPACITY];
    path[..executable.len()].copy_from_slice(executable.as_bytes());
    let (overrides, override_count) = override_entries(&layout, environment)?;
    let overrides = &overrides[..override_count];
    let directory = layout.current_directory(environment);
    let mut storage = run.launch_storage.borrow_mut();
    let child = spawn_list(&mut storage, executable, false, run, overrides, directory)?;
    let ignored_child = if run.settings.ignore == IgnoreMode::Include {
        None
    } else {
        Some(spawn_list(&mut storage, executable, true, run, overrides, directory)?)
    };
    drop(storage);
    Ok(PendingListing {
        target,
        child: Some(child),
        ignored_child,
        identity,
        path,
        path_length: executable.len(),
        slot,
        layout,
        ignoring: false,
        lane_pid: 0,
        stdout: StdoutState::Open,
        stderr_done: false,
        exit: None,
        backlog: None,
    })
}

fn launch_listed(
    scan: &ListingScan<'_, '_>,
    ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut Watches,
    intent: ExactTestIntent<'_, '_, '_>,
) -> Result<(), RunError> {
    let (run, target, environment, layout) = (scan.run, scan.target, scan.environment, scan.layout);
    let (overrides, override_count) = override_entries(layout, environment)?;
    if run.tripped(ledger) {
        run.policy.borrow_mut().launched();
    }
    let mut storage = run.launch_storage.borrow_mut();
    let park = run.parks_next(ledger);
    let slot = LaunchInput {
        target,
        owner: LaunchOwner::Ledger(ledger),
        intent,
        inherited_environment: run.environment,
        child_overrides: &overrides[..override_count],
        storage: &mut storage,
        harness_arguments: run.harness_arguments,
        current_directory: layout.current_directory(environment).as_bytes(),
    }
    .launch_as(park)
    .map_err(|error| {
        report_launch_failure(
            LaunchCause::from(&error),
            intent.test_name(),
            executable_text(intent.executable()),
        )
    })?;
    if park {
        run.ready.set(Some(slot));
    }
    run.targets.borrow_mut().tally(target, Tally::Launched);
    start_session(ledger, watches, scan.lanes, slot, None)?;
    shard::note_group(run, watches, slot.index(), intent.test_name());
    Ok(())
}

pub(crate) struct ListingRun<'run> {
    signals: &'run SignalPipe,
    filter: &'run str,
    exact: bool,
    targets: RefCell<TargetTable>,
    started_millis: u64,
    harness_arguments: &'run [&'run [u8]],
    environment: &'run [&'run [u8]],
    library_directories: &'run [&'run str],
    inherited_dylib_path: Option<&'run str>,
    home: Option<&'run str>,
    settings: &'run Settings,
    prepared: Option<&'run Prepared<'run>>,
    docs: DocObservation,
    policy: RefCell<StopPolicy>,
    doc: RefCell<Option<Box<DocRun>>>,
    launch_storage: RefCell<HeldLaunchStorage>,
    ready: Cell<Option<SessionSlotId>>,
    withdrawn_ready: Cell<Option<SessionSlotId>>,
    shards: RefCell<ShardArena<'run>>,
    sched: RefCell<SchedTable<'run>>,
    specials: RefCell<SpecialPlan<'run>>,
    sched_target: Cell<Option<usize>>,
    stop_sent: Cell<bool>,
}

#[derive(Default)]
pub(crate) struct DocObservation {
    passed: Cell<usize>,
    failed: Cell<usize>,
    ignored: Cell<usize>,
    blocks: Cell<usize>,
    empty_blocks: Cell<usize>,
}

impl ListingRun<'_> {
    pub(crate) fn signal_gate(&self) -> Result<Readiness, RunError> {
        self.signals.gate().map_err(|_| RunError::Reactor)
    }

    pub(crate) fn interrupt_source(&self) -> SignalSource {
        SignalSource::new(self.signals.interrupt())
    }

    pub(crate) fn drain_signals(&self) -> Result<(), RunError> {
        self.signals.drain().map_err(|_| RunError::Reactor)
    }

    // a forwarded signal only reaches children that honor it, so a per-test child that ignores it is terminated here
    pub(crate) fn interrupt_sessions(&self, ledger: &Ledger<'_>, watches: &Watches) {
        if self.settings.process == ProcessMode::Test && self.interrupted() && !self.stop_sent.get() {
            shard::stop_all(self, ledger, watches);
        }
    }

    fn interrupted(&self) -> bool {
        self.signals.received().is_some()
    }

    fn reporting(&self) -> Reporting<'_> {
        Reporting {
            color: self.settings.color,
            targets: &self.targets,
            ready: &self.ready,
            withdrawn_ready: &self.withdrawn_ready,
            policy: &self.policy,
            doc_failures: &self.docs.failed,
            jobs: self.settings.jobs,
        }
    }

    // sessions admitted at once: the jobs that run plus the frozen children this platform creates ahead of a free slot
    fn admit_limit(&self) -> usize {
        (self.settings.jobs + READY_AHEAD).min(LIVE_CAPACITY)
    }

    fn parks_next(&self, ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>) -> bool {
        self.admit_limit() > self.settings.jobs
            && self.ready.get().is_none()
            && ledger.sessions().len() >= self.settings.jobs
    }

    fn tripped(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> bool {
        let failures = ledger.snapshot().failed + self.docs.failed.get();
        let observation = self.policy.borrow().observe_failures(failures);
        if observation == Observation::FirstTrip {
            let doc = self.interrupt_doc_tests();
            self.policy
                .borrow_mut()
                .record_trip(self.started_sessions(ledger), doc);
        }
        observation != Observation::Clear
    }

    // a child terminated while still frozen never ran, so it is not in flight even while its exit is pending
    fn started_sessions(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> usize {
        let frozen = self
            .withdrawn_ready
            .get()
            .is_some_and(|slot| ledger.sessions().get(slot).is_some());
        ledger.sessions().len() - usize::from(frozen)
    }

    fn interrupt_doc_tests(&self) -> DocAtTrip {
        self.doc
            .try_borrow_mut()
            .ok()
            .and_then(|mut doc| doc.as_mut().map(|doc| doc.state_for_trip()))
            .unwrap_or(DocAtTrip::NotSpawned)
    }

    pub(crate) fn take_doc_child(&self) -> Option<Child> {
        self.doc.borrow_mut().as_mut().and_then(|doc| doc.take_child())
    }

    pub(crate) fn with_doc(
        &self,
        act: impl FnOnce(&mut DocRun, bool, &DocObservation) -> Result<(), RunError>,
    ) -> Result<(), RunError> {
        match self.doc.borrow_mut().as_mut() {
            Some(doc) => act(doc, self.settings.color, &self.docs),
            None => Ok(()),
        }
    }

    fn doc_tests_finished(&self) -> bool {
        self.doc.borrow().as_ref().is_none_or(|doc| doc.finished())
    }
}

enum Flow {
    Next,
    Pause,
    Eof,
}

fn listing_wanted(
    run: &ListingRun<'_>,
    ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
) -> bool {
    let stopped = run.tripped(ledger);
    if run.settings.process == ProcessMode::Shard {
        return stopped || !run.shards.borrow().sealed();
    }
    stop::listing_wanted(stopped, ledger.sessions().len(), run.admit_limit())
}

struct ListingScan<'scan, 'run> {
    run: &'scan ListingRun<'run>,
    lanes: &'scan Lanes,
    executable: &'scan str,
    target: usize,
    environment: &'scan [u8],
    layout: &'scan TestEnvironmentLayout,
}

impl ListingScan<'_, '_> {
    fn apply(
        &self,
        step: ListingSourceStep<'_, '_, '_>,
        stopped: bool,
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut Watches,
    ) -> Result<Flow, RunError> {
        let run = self.run;
        match step {
            ListingSourceStep::Launch(intent) => {
                if !run.policy.borrow_mut().admit_listed(run.interrupted(), stopped) {
                    return Ok(Flow::Next);
                }
                if run.settings.process == ProcessMode::Shard {
                    return Ok(if shard::collect(run, (self.executable, self.target), intent.test_name())? { Flow::Pause } else { Flow::Next });
                }
                launch_listed(self, ledger, watches, intent)?;
                if ledger.sessions().len() >= run.admit_limit() {
                    return Ok(Flow::Pause);
                }
            }
            ListingSourceStep::Filtered => {
                ledger.record_filtered().map_err(|_| RunError::Outcome)?;
                run.targets.borrow_mut().tally(self.target, Tally::Filtered);
            }
            ListingSourceStep::IgnoredTest(name) => {
                report_skipped(name, run.settings.color)?;
                ledger.record_ignored().map_err(|_| RunError::Outcome)?;
                run.targets.borrow_mut().tally(self.target, Tally::Ignored);
            }
            ListingSourceStep::Read { .. } | ListingSourceStep::NeedMore | ListingSourceStep::Ignored => {}
            ListingSourceStep::Eof => return Ok(Flow::Eof),
            ListingSourceStep::Full => return Ok(Flow::Pause),
        }
        Ok(Flow::Next)
    }
}

fn report_skipped(name: &str, color: bool) -> Result<(), RunError> {
    let mut report = [0; NAME_CAPACITY + 34];
    let name_length = name.len().min(NAME_CAPACITY);
    report[..name_length].copy_from_slice(&name.as_bytes()[..name_length]);
    report[name_length] = b' ';
    let label = write_report(&[TerminalOutcome::Ignored], color, &mut report[name_length + 1..])
        .map_err(|_| RunError::Outcome)?;
    write_fd(1, &report[..name_length + 1 + label])
}

fn feed_ignored_lines(
    bytes: &[u8],
    framer: &mut ArtifactFramer<'_>,
    set: &mut IgnoredSet,
) -> Result<(), RunError> {
    let mut remaining = bytes;
    while !remaining.is_empty() || framer.frame_ready().map_err(|_| RunError::ListingStream)? {
        let (parsed, consumed_bytes) = match framer
            .next_record(remaining, parse_listing_line)
            .map_err(|_| RunError::ListingStream)?
        {
            RecordStep::Parsed { parsed, consumed_bytes } => (Some(parsed), consumed_bytes),
            RecordStep::NeedMore { consumed_bytes } => (None, consumed_bytes),
        };
        if let Some(ListingLine::Test(name)) = parsed {
            set.insert(name.as_bytes()).map_err(RunError::IgnoredSet)?;
        }
        remaining = &remaining[consumed_bytes..];
    }
    Ok(())
}

fn target_triple<'argument>(arguments: &[&'argument [u8]]) -> Option<&'argument [u8]> {
    let mut remaining = arguments.iter().copied();
    while let Some(argument) = remaining.next() {
        if argument == b"--target" {
            return remaining.next();
        }
        if let Some(triple) = argument.strip_prefix(b"--target=") {
            return Some(triple);
        }
    }
    None
}

fn validated_harness<'argument>(
    parser: &CliParser<'argument>,
) -> Result<HarnessRequest<'argument>, RunError> {
    let request =
        HarnessRequest::parse(parser.harness_arguments()).map_err(|fault| match fault {
            HarnessFault::Unsupported => RunError::Arguments,
            HarnessFault::MissingThreadsValue => RunError::TestThreads(ThreadsError {
                source: ThreadsSource::HarnessArgument,
                fault: ThreadsFault::Malformed,
            }),
        })?;
    if parser.has_invalid_manifest_path()
        || parser.has_invalid_cargo_option_value()
        || parser.has_invalid_positional_argument()
        || parser.has_invalid_filterset()
        || parser.has_argument_storage_overflow()
    {
        return Err(RunError::Arguments);
    }
    Ok(request)
}

fn spawn_cargo(
    parser: &CliParser<'_>,
    environment: &[&[u8]],
    directory: Option<&ChildDirectory>,
) -> Result<Child, RunError> {
    let forwarded = parser.cargo_arguments();
    if forwarded.iter().any(|argument| argument.contains(&0)) {
        return Err(RunError::Arguments);
    }
    let mut arguments: Vec<&[u8]> = Vec::from([
        b"test".as_slice(),
        b"--no-run".as_slice(),
        b"--message-format=json".as_slice(),
    ]);
    if let Some(directory) = directory {
        arguments.push(directory.argument());
    }
    arguments.extend_from_slice(forwarded);
    spawn(b"cargo", &arguments, environment, &[], None).map_err(|_| RunError::Spawn)
}

const METADATA_VALUE_FLAGS: [&[u8]; 4] = [b"--manifest-path", b"-m", b"--config", b"-Z"];
const METADATA_BOOLEAN_FLAGS: [&[u8]; 3] = [b"--locked", b"--frozen", b"--offline"];

// the bin set comes from one `cargo metadata --no-deps`; a spawn failure just leaves build-finished as the signal
fn spawn_metadata(parser: &CliParser<'_>, environment: &[&[u8]]) -> Result<Child, RunError> {
    let mut arguments: Vec<&[u8]> = Vec::from([
        b"metadata".as_slice(),
        b"--no-deps".as_slice(),
        b"--format-version=1".as_slice(),
    ]);
    let forwarded = parser.cargo_arguments();
    let mut locked = false;
    let mut index = 0;
    while index < forwarded.len() {
        let argument = forwarded[index];
        let takes_value = METADATA_VALUE_FLAGS.contains(&argument);
        let joined = METADATA_VALUE_FLAGS
            .iter()
            .any(|flag| argument.strip_prefix(*flag).is_some_and(|rest| rest.first() == Some(&b'=')));
        let boolean = METADATA_BOOLEAN_FLAGS.contains(&argument);
        locked |= argument == b"--locked";
        let width = if takes_value { 2 } else { 1 };
        if takes_value || joined || boolean {
            let end = (index + width).min(forwarded.len());
            for part in &forwarded[index..end] {
                if part.contains(&0) {
                    return Err(RunError::Arguments);
                }
                arguments.push(part);
            }
            index = end;
        } else {
            index += 1;
        }
    }
    if !locked {
        arguments.push(b"--locked");
    }
    spawn(b"cargo", &arguments, environment, &[], None).map_err(|_| RunError::Spawn)
}

// the short-lived metadata child is awaited alone; none of the run's own children are polled meanwhile
async fn read_metadata(child: Child) -> Option<Box<MetadataFacts>> {
    // the scanner's tens of KiB of text storage would otherwise sit in every enclosing future
    let mut scan = Box::new(MetadataScan::new());
    let mut usable = true;
    let code = collect_child(child, |bytes| {
        usable = usable && scan.feed(bytes).is_ok();
    })
    .await
    .ok()?;
    (usable && code == 0).then(|| Box::new((*scan).finish()))
}

pub(crate) struct CargoDriver<'buffer> {
    process_id: u32,
    framer: ArtifactFramer<'buffer>,
    scratch: &'buffer mut [u8],
    linked: LinkedDirectories<'buffer>,
    build_scripts: BuildScriptOutputs<'buffer>,
    listing_framer: ArtifactFramer<'buffer>,
    ignored_framer: ArtifactFramer<'buffer>,
    ignored: &'buffer mut IgnoredSet,
    environments: &'buffer mut [u8],
    active: Option<PendingListing>,
    pending_listing: Option<PendingListing>,
    cargo_backlog: Option<(Range<u32>, usize)>,
    cargo_eof: bool,
    finishing: bool,
    reason: &'static [u8],
    discovery: DiscoveryFsm,
    parked: ParkedArtifacts,
    dropped: usize,
    lock_watch: LockWatch,
}

fn gate_debug(enabled: bool, describe: impl FnOnce(&mut Line<1024>)) {
    if !enabled {
        return;
    }
    let mut line = Line::<1024>::new();
    line.text(b"cargo-val: debug: gate");
    describe(&mut line);
    line.text(b"\n");
    let _ = line.write(2);
}

fn exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

fn debug_artifact(enabled: bool, gated: bool, gate: BinGate, artifact: &TestArtifact<'_>) {
    gate_debug(enabled, |line| {
        line.text(if gated { b" park" } else { b" launch" });
        line.text(b" kind=");
        line.text(artifact.target_kind.as_bytes());
        line.text(b" target=");
        line.text(artifact.target_name.as_bytes());
        line.text(b" gate=");
        line.text(gate.name());
        line.text(b" package=");
        line.text(artifact.package_id.as_bytes());
    });
}

fn debug_bin_built(enabled: bool, executable: &str, package_id: &str) {
    gate_debug(enabled, |line| {
        line.text(b" bin-built on-disk=");
        line.text(if exists(executable) { b"yes" } else { b"no" });
        line.text(b" executable=");
        line.text(executable.as_bytes());
        line.text(b" package=");
        line.text(package_id.as_bytes());
    });
}

fn debug_build_end(enabled: bool, event: &[u8], parked: usize) {
    gate_debug(enabled, |line| {
        line.text(event);
        line.text(b" parked=");
        line.number(parked);
    });
}

fn debug_gate_action(enabled: bool, action: &[u8], reason: &[u8], artifact: &TestArtifact<'_>) {
    gate_debug(enabled, |line| {
        line.text(action);
        line.text(b" reason=");
        line.text(reason);
        line.text(b" target=");
        line.text(artifact.target_name.as_bytes());
        line.text(b" package=");
        line.text(artifact.package_id.as_bytes());
    });
}

fn resolve_bin_gate(settings: &Settings, spawned: bool, facts: Option<Box<MetadataFacts>>) -> (BinCounts, BinGate) {
    let counts = facts.map(|found| found.bins);
    gate_debug(settings.debug, |line| {
        line.text(b" metadata spawned=");
        line.text(if spawned { b"yes" } else { b"no" });
        line.text(b" bin-packages=");
        match &counts {
            Some(found) => line.number(found.packages()),
            None => line.text(b"unavailable"),
        }
    });
    match (settings.bin_gate, counts) {
        (false, _) => (BinCounts::new(), BinGate::Off),
        (true, Some(found)) => (found, BinGate::Metadata),
        (true, None) => (BinCounts::new(), BinGate::BuildFinished),
    }
}

fn start_listing(
    run: &ListingRun<'_>,
    ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    artifact: &TestArtifact<'_>,
    linked: &LinkedDirectories<'_>,
    build_scripts: &BuildScriptOutputs<'_>,
    slot: usize,
    environments: &mut [u8],
) -> Result<Option<PendingListing>, RunError> {
    if run.interrupted() {
        return Ok(None);
    }
    if run.tripped(ledger) {
        run.policy.borrow_mut().target_skipped();
        return Ok(None);
    }
    let build_script = build_scripts.find(artifact.package_id);
    spawn_listing(artifact, run, linked.as_str(), build_script, slot, environments).map(Some)
}

pub(crate) type Ledger<'ledger> =
    OutcomeLedger<'ledger, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>;
pub(crate) type Watches = [Option<TestWatch>];

impl CargoDriver<'_> {
    fn drained(&self, unaccounted: usize) -> bool {
        self.discovery.drained(unaccounted, self.parked.is_empty())
            && self.active.is_none()
            && self.pending_listing.is_none()
    }

    pub(crate) fn finished(&self, run: &ListingRun<'_>, ledger: &Ledger<'_>) -> bool {
        self.drained(ledger.unaccounted()) && run.doc_tests_finished()
    }

    fn has_room(&self) -> bool {
        self.active.is_none() || self.pending_listing.is_none()
    }

    pub(crate) fn cargo_open(&self) -> bool {
        self.has_room() && self.cargo_backlog.is_none()
    }

    pub(crate) fn listing_open(&self, run: &ListingRun<'_>, ledger: &Ledger<'_>) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.ignoring || (active.backlog.is_none() && listing_wanted(run, ledger))
        })
    }

    fn queue_listing(
        &mut self,
        started: Option<PendingListing>,
        lanes: &Lanes,
    ) -> Result<(), RunError> {
        let Some(next) = started else {
            return Ok(());
        };
        if self.active.is_some() {
            self.pending_listing = Some(next);
            return Ok(());
        }
        self.begin_listing(next, lanes)
    }

    fn begin_listing(&mut self, mut listing: PendingListing, lanes: &Lanes) -> Result<(), RunError> {
        self.ignored.clear();
        self.listing_framer.reset();
        self.ignored_framer.reset();
        listing.ignoring = listing.ignored_child.is_some();
        let child = listing
            .ignored_child
            .take()
            .or_else(|| listing.child.take())
            .ok_or(RunError::Spawn)?;
        listing.lane_pid = attach(&lanes[LISTING_LANE], child)?;
        self.active = Some(listing);
        Ok(())
    }

    fn release_parked(
        &mut self,
        run: &ListingRun<'_>,
        ledger: &Ledger<'_>,
        lanes: &Lanes,
    ) -> Result<(), RunError> {
        while self.has_room() {
            let Some(owned) = self
                .parked
                .take_ready(|package_id| !self.discovery.waits(package_id))
            else {
                self.drop_unreleasable(run);
                return Ok(());
            };
            let artifact = owned.as_artifact();
            debug_gate_action(run.settings.debug, b" release", self.reason, &artifact);
            let started =
                start_listing(
                run,
                ledger,
                &artifact,
                &self.linked,
                &self.build_scripts,
                free_slot(self.active.as_ref()),
                &mut *self.environments,
            )?;
            self.queue_listing(started, lanes)?;
        }
        Ok(())
    }

    fn drop_unreleasable(&mut self, run: &ListingRun<'_>) {
        while self.finishing
            && let Some(owned) = self.parked.take_ready(|_| true)
        {
            self.dropped += 1;
            debug_gate_action(run.settings.debug, b" drop", self.reason, &owned.as_artifact());
        }
    }

    /// Parses one lent chunk of Cargo stdout, or the end-of-stream drain when `eof`, acting on every record it completes while a listing slot is free.
    /// Returns how many chunk bytes were consumed.
    fn advance(
        &mut self,
        chunk: &[u8],
        eof: bool,
        run: &ListingRun<'_>,
        ledger: &Ledger<'_>,
        lanes: &Lanes,
    ) -> Result<usize, RunError> {
        let mut context = Context::from_waker(Waker::noop());
        let mut offset = 0;
        loop {
            if !self.has_room() {
                return Ok(offset);
            }
            let ready_before = self.framer.frame_ready().map_err(|_| RunError::CargoStream)?;
            let (step, consumed) = if eof {
                match CargoSource::poll_lent_eof(&mut self.framer, &mut *self.scratch, 16, &mut context) {
                    Poll::Ready(result) => result.map(|step| (step, 0)),
                    Poll::Pending => return Err(RunError::CargoStream),
                }
            } else {
                match CargoSource::poll_lent(
                    &mut self.framer,
                    &chunk[offset..],
                    &mut *self.scratch,
                    16,
                    &mut context,
                ) {
                    Poll::Ready(result) => result,
                    Poll::Pending => return Err(RunError::CargoStream),
                }
            }
            .map_err(|_| RunError::CargoStream)?;
            offset += consumed;
            let progressed = consumed > 0 || ready_before;
            match step {
                CargoSourceStep::Artifact(artifact) => {
                    let gated = matches!(artifact.target_kind, "test" | "bench")
                        && self.discovery.waits(artifact.package_id);
                    debug_artifact(run.settings.debug, gated, self.discovery.gate(), &artifact);
                    if gated {
                        self.parked.park(&artifact).map_err(RunError::Parked)?;
                    } else {
                        let started = start_listing(
                            run,
                            ledger,
                            &artifact,
                            &self.linked,
                            &self.build_scripts,
                            free_slot(self.active.as_ref()),
                            &mut *self.environments,
                        )?;
                        self.queue_listing(started, lanes)?;
                    }
                }
                CargoSourceStep::BinBuilt {
                    package_id,
                    executable,
                } => {
                    debug_bin_built(run.settings.debug, executable, package_id);
                    self.discovery.bin_built(package_id);
                    self.reason = b"bin-built";
                    self.release_parked(run, ledger, lanes)?;
                }
                CargoSourceStep::BuildFinished { success } => {
                    let (event, reason): (&[u8], &'static [u8]) = if success {
                        (b" build-finished success=true", b"build-succeeded")
                    } else {
                        (b" build-finished success=false", b"build-failed")
                    };
                    debug_build_end(run.settings.debug, event, self.parked.len());
                    self.discovery.build_finished(success);
                    self.finishing = true;
                    self.reason = reason;
                    self.release_parked(run, ledger, lanes)?;
                }
                CargoSourceStep::BuildScript(script) => {
                    for reported in script.linked_paths.iter() {
                        self.linked
                            .add(reported)
                            .map_err(|_| RunError::TestEnvironment)?;
                    }
                    self.build_scripts
                        .add(&script)
                        .map_err(|_| RunError::TestEnvironment)?;
                }
                CargoSourceStep::Read { .. }
                | CargoSourceStep::NeedMore
                | CargoSourceStep::Ignored => {}
                CargoSourceStep::Eof => {
                    self.discovery.stdout_eof();
                    debug_build_end(run.settings.debug, b" stdout-eof", self.parked.len());
                    self.finishing = true;
                    self.reason = b"stdout-eof";
                    return self.release_parked(run, ledger, lanes).map(|()| offset);
                }
            }
            let ready = self.framer.frame_ready().map_err(|_| RunError::CargoStream)?;
            if !eof && offset == chunk.len() && !ready {
                return Ok(offset);
            }
            if !progressed {
                return Err(RunError::CargoStream);
            }
        }
    }

    pub(crate) fn cargo_chunk(
        &mut self,
        lanes: &Lanes,
        range: Range<u32>,
        offset: usize,
        run: &ListingRun<'_>,
        ledger: &Ledger<'_>,
    ) -> Result<(), RunError> {
        let slot = lanes[CARGO_LANE].0.borrow();
        let stream = slot.as_deref().ok_or(RunError::CargoStream)?;
        let bytes = stream.window(range.clone()).ok_or(RunError::CargoStream)?;
        let consumed = offset + self.advance(&bytes[offset..], false, run, ledger, lanes)?;
        let ready = self.framer.frame_ready().map_err(|_| RunError::CargoStream)?;
        self.cargo_backlog = (consumed < bytes.len() || ready).then_some((range, consumed));
        Ok(())
    }

    pub(crate) fn cargo_out_ended(&mut self) {
        self.cargo_eof = true;
    }

    pub(crate) fn forward_stderr(&mut self, bytes: &[u8]) -> Result<(), RunError> {
        write_fd(2, bytes)?;
        announce_lock_wait(&mut self.lock_watch, bytes)
    }

    pub(crate) fn cargo_exited(&mut self, code: i32) {
        release_child(self.process_id);
        self.discovery.exited(code);
    }

    pub(crate) fn stderr_ended(&mut self) {
        self.discovery.stderr_eof();
    }

    pub(crate) fn listing_chunk(
        &mut self,
        lanes: &Lanes,
        range: Range<u32>,
        offset: usize,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<(), RunError> {
        let slot = lanes[LISTING_LANE].0.borrow();
        let stream = slot.as_deref().ok_or(RunError::ListingStream)?;
        let bytes = stream.window(range.clone()).ok_or(RunError::ListingStream)?;
        let consumed = offset + self.listing_bytes(&bytes[offset..], lanes, run, ledger, watches)?;
        let ready = self.listing_framer.frame_ready().map_err(|_| RunError::ListingStream)?;
        if let Some(active) = self.active.as_mut() {
            let stalled = !active.ignoring && (consumed < bytes.len() || ready);
            active.backlog = stalled.then_some((range, consumed));
        }
        Ok(())
    }

    fn listing_bytes(
        &mut self,
        chunk: &[u8],
        lanes: &Lanes,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<usize, RunError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(chunk.len());
        };
        if active.ignoring {
            feed_ignored_lines(chunk, &mut self.ignored_framer, self.ignored)?;
            return Ok(chunk.len());
        }
        let executable = core::str::from_utf8(&active.path[..active.path_length])
            .map_err(|_| RunError::Arguments)?;
        let selection = Selection {
            filter: run.filter,
            exact: run.exact,
            filterset: run.prepared,
            package: active.identity.package(),
            binary: active.identity.binary(),
            mode: run.settings.ignore,
            ignored: &*self.ignored,
        };
        let filter = NameFilter::Selected(&selection);
        let scan = ListingScan {
            lanes,
            run,
            executable,
            target: active.target,
            environment: &self.environments[slot_range(active.slot)],
            layout: &active.layout,
        };
        shard::load_records(run, executable, active.target)?;
        let mut context = Context::from_waker(Waker::noop());
        let mut offset = 0;
        loop {
            let stopped = run.tripped(ledger);
            if !status_queue::room_for(EVENT_RESERVE) {
                status_queue::defer();
                return Ok(offset);
            }
            let Poll::Ready(polled) = ListingSource::poll_lent(
                &mut self.listing_framer,
                ledger.sessions(),
                &chunk[offset..],
                ExecutablePath::Borrowed(executable),
                filter,
                |name| shard::holds(run, watches, ledger.sessions().len(), name),
                &mut context,
            ) else {
                return Ok(offset);
            };
            let (step, consumed) = polled.map_err(|_| RunError::ListingStream)?;
            offset += consumed;
            if matches!(scan.apply(step, stopped, ledger, watches)?, Flow::Pause) {
                return Ok(offset);
            }
            let ready = self.listing_framer.frame_ready().map_err(|_| RunError::ListingStream)?;
            if offset == chunk.len() && !ready {
                return Ok(offset);
            }
        }
    }

    fn listing_drain(
        &mut self,
        lanes: &Lanes,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<bool, RunError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(true);
        };
        let executable = core::str::from_utf8(&active.path[..active.path_length])
            .map_err(|_| RunError::Arguments)?;
        let selection = Selection {
            filter: run.filter,
            exact: run.exact,
            filterset: run.prepared,
            package: active.identity.package(),
            binary: active.identity.binary(),
            mode: run.settings.ignore,
            ignored: &*self.ignored,
        };
        let filter = NameFilter::Selected(&selection);
        let scan = ListingScan {
            lanes,
            run,
            executable,
            target: active.target,
            environment: &self.environments[slot_range(active.slot)],
            layout: &active.layout,
        };
        shard::load_records(run, executable, active.target)?;
        let mut context = Context::from_waker(Waker::noop());
        loop {
            let stopped = run.tripped(ledger);
            if !status_queue::room_for(EVENT_RESERVE) {
                status_queue::defer();
                return Ok(false);
            }
            let Poll::Ready(polled) = ListingSource::poll_lent_eof(
                &mut self.listing_framer,
                ledger.sessions(),
                ExecutablePath::Borrowed(executable),
                filter,
                |name| shard::holds(run, watches, ledger.sessions().len(), name),
                &mut context,
            ) else {
                return Ok(false);
            };
            let step = polled.map_err(|_| RunError::ListingStream)?;
            match scan.apply(step, stopped, ledger, watches)? {
                Flow::Eof => {
                    shard::seal(run);
                    return Ok(true);
                }
                Flow::Pause => return Ok(false),
                Flow::Next => {}
            }
        }
    }

    pub(crate) fn listing_stdout_ended(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.stdout = if active.ignoring {
                StdoutState::Drained
            } else {
                StdoutState::Ended
            };
        }
    }

    pub(crate) fn listing_stderr_ended(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.stderr_done = true;
        }
    }

    pub(crate) fn listing_exited(&mut self, code: i32) {
        if let Some(active) = self.active.as_mut() {
            release_child(active.lane_pid);
            active.exit = Some(code);
        }
    }

    /// Advances everything that was waiting on room, a free job slot, or a finished listing, until nothing moves.
    pub(crate) fn resume(
        &mut self,
        lanes: &Lanes,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<(), RunError> {
        while self.resume_once(lanes, run, ledger, watches)? {}
        Ok(())
    }

    fn resume_once(
        &mut self,
        lanes: &Lanes,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<bool, RunError> {
        if self.dispatch_shards(lanes, run, ledger, watches)? {
            return Ok(true);
        }
        if self.settle_listing(lanes, run, ledger, watches)? {
            return Ok(true);
        }
        if let Some((range, offset)) = self.listing_backlog()
            && listing_wanted(run, ledger)
            && status_queue::room_for(EVENT_RESERVE)
        {
            self.listing_chunk(lanes, range, offset, run, ledger, watches)?;
            return Ok(self.listing_backlog().is_none_or(|(_, moved)| moved > offset));
        }
        let parked = self.parked.len();
        self.release_parked(run, ledger, lanes)?;
        if self.parked.len() != parked {
            return Ok(true);
        }
        if let Some((range, offset)) = self.cargo_backlog.clone()
            && self.has_room()
        {
            self.cargo_backlog = None;
            self.cargo_chunk(lanes, range, offset, run, ledger)?;
            return Ok(self.cargo_backlog.as_ref().is_none_or(|(_, moved)| *moved > offset));
        }
        if self.cargo_eof && !self.discovery.stdout_closed() && self.cargo_backlog.is_none() && self.has_room() {
            self.advance(&[], true, run, ledger, lanes)?;
            return Ok(self.discovery.stdout_closed());
        }
        Ok(false)
    }

    fn listing_backlog(&self) -> Option<(Range<u32>, usize)> {
        self.active.as_ref().and_then(|active| active.backlog.clone())
    }

    fn settle_listing(
        &mut self,
        lanes: &Lanes,
        run: &ListingRun<'_>,
        ledger: &mut Ledger<'_>,
        watches: &mut Watches,
    ) -> Result<bool, RunError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        if active.stdout == StdoutState::Ended
            && active.backlog.is_none()
            && listing_wanted(run, ledger)
        {
            let done = self.listing_drain(lanes, run, ledger, watches)?;
            if let Some(active) = self.active.as_mut()
                && done
            {
                active.stdout = StdoutState::Drained;
            }
            return Ok(done);
        }
        if !(active.stdout == StdoutState::Drained && active.stderr_done && active.exit.is_some()) {
            return Ok(false);
        }
        if active.exit.is_some_and(|code| code != 0) && !run.interrupted() {
            return Err(RunError::ListingFailed);
        }
        if active.ignoring {
            return self.start_main_listing(lanes).map(|()| true);
        }
        if shard::busy(run) {
            return Ok(false);
        }
        self.active = None;
        if let Some(next) = self.pending_listing.take() {
            self.begin_listing(next, lanes)?;
        }
        Ok(true)
    }

    fn start_main_listing(&mut self, lanes: &Lanes) -> Result<(), RunError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        let child = active.child.take().ok_or(RunError::Spawn)?;
        active.ignoring = false;
        active.stdout = StdoutState::Open;
        active.stderr_done = false;
        active.exit = None;
        active.lane_pid = attach(&lanes[LISTING_LANE], child)?;
        self.listing_framer.reset();
        Ok(())
    }
}


/// Runs the whole cargo-val run: setup, the awaited prime loop, and the summary.
///
/// # Errors
///
/// Returns [`RunError`] naming the stage that failed.
pub(crate) async fn run_body(parser: &CliParser<'_>) -> Result<RunSummary, RunError> {
    let started_millis = monotonic_millis();
    let harness = validated_harness(parser)?;
    let environment = unsafe { Environment::current() };
    let settings = runtime_settings(&environment, &harness, parser)?;
    let mut storage = run_storage(settings.process);
    let storage = &mut storage;
    let (cargo_storage, (libdir_bytes, inherited), (sessions, watches), (shard_names, sched_bytes, picked)) = storage;
    let inherited_count = environment
        .collect(inherited)
        .map_err(|_| RunError::Environment)?;
    let inherited = &inherited[..inherited_count];
    let configured_target = utf8_variable(&environment, b"CARGO_VAL_TARGET_DIR")?;
    let target_pairs = [("CARGO_VAL_TARGET_DIR", configured_target.unwrap_or(""))];
    let target_source = BorrowedSettings::new(&target_pairs[..usize::from(configured_target.is_some())]);
    let target_mode = resolve_target_dir(&target_source, parser)?;
    if parser.doc_selection() == DocSelection::Only {
        return run_doc_only(parser, &settings, target_mode, started_millis, inherited).await;
    }
    let queries = start_libdir_queries(&environment, parser, inherited)?;
    let inherited_dylib_path = utf8_variable(&environment, DYLIB_PATH_KEY.as_bytes())?;
    let home = utf8_variable(&environment, b"HOME")?;
    let filter = filter_text(parser)?;
    let signals = SignalPipe::install().map_err(|_| RunError::Reactor)?;
    let StartedChildren {
        cargo: cargo_child,
        doc_tests,
        bins,
        gate,
        lock_watch,
    } = start_children(parser, &settings, target_mode, inherited).await?;
    let cargo_process_id = cargo_child.process_id();
    let mut driver = new_driver(cargo_storage, cargo_process_id, bins, gate, lock_watch)?;
    let mut ledger = OutcomeLedger::new(sessions).map_err(|_| RunError::Outcome)?;
    let (library_directories, directory_count) = resolve_libdirs(queries, libdir_bytes)
        .await
        .inspect_err(|_| cargo_child.terminate())?;
    let prepared = held(settings.filterset.as_ref().map(Filterset::prepare));
    let run = held(ListingRun {
        signals: &signals,
        filter,
        exact: parser.exact(),
        targets: RefCell::new(TargetTable::new()),
        started_millis,
        harness_arguments: match settings.process {
            ProcessMode::Shard => shard_arguments(settings.ignore),
            ProcessMode::Test => ignore_arguments(settings.ignore),
        },
        environment: inherited,
        library_directories: &library_directories[..directory_count],
        inherited_dylib_path,
        home,
        settings: &settings,
        prepared: (*prepared).as_ref(),
        docs: DocObservation::default(),
        policy: RefCell::new(StopPolicy::new(settings.max_fail)),
        doc: RefCell::new(doc_tests),
        launch_storage: RefCell::new(held_launch_storage()),
        ready: Cell::new(None),
        withdrawn_ready: Cell::new(None),
        shards: RefCell::new(ShardArena::with_declared(shard_names, DECLARED_BYTES)),
        sched: RefCell::new(SchedTable::new(sched_bytes)),
        specials: RefCell::new(SpecialPlan::new(picked)),
        sched_target: Cell::new(None),
            stop_sent: Cell::new(false),
    });
    observe_cargo(cargo_child, &mut driver, &run, &mut ledger, watches).await?;
    if driver.discovery.lost_artifacts_on_clean_exit(driver.dropped) {
        return Err(RunError::CargoStream);
    }
    finish_run(parser, &run, driver.discovery.exit_code())
}

type CargoStorage = (
    Box<[u8]>,
    Box<[u8]>,
    Box<[u8]>,
    Box<[u8]>,
    Box<[u8]>,
    Box<[u8]>,
    Box<[u8]>,
    IgnoredSet,
);

type RunStorage<'environment> = (
    CargoStorage,
    (
        Box<[u8]>,
        Box<[&'environment [u8]]>,
    ),
    (
        Box<SessionSlots<RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>>,
        Box<Watches>,
    ),
    (Box<[u8]>, Box<[u8]>, Box<[u32]>),
);

// run-sized fixed storage too large for any stack frame or future, allocated straight on the heap once at setup
fn run_storage<'environment>(process: ProcessMode) -> RunStorage<'environment> {
    (
        (
            zeroed(FRAME_CAPACITY),
            zeroed(SCRATCH_CAPACITY),
            zeroed(LINKED_CAPACITY),
            zeroed(BUILD_SCRIPT_CAPACITY),
            zeroed(SOURCE_CAPACITY + 1),
            zeroed(RESULT_LINE_CAPACITY + 1),
            zeroed(ENVIRONMENT_CAPACITY * LISTING_SLOTS),
            IgnoredSet::new(),
        ),
        (zeroed(LIBDIR_CAPACITY * 2), std::vec![b"".as_slice(); ENV_CAPACITY].into_boxed_slice()),
        (Box::new(SessionSlots::new()), (0..LIVE_CAPACITY).map(|_| None).collect()),
        match process {
            ProcessMode::Shard => (
                zeroed(crate::build_defaults::SHARD_ARENA_BYTES + DECLARED_BYTES),
                zeroed(crate::build_defaults::SCHED_RECORD_BYTES),
                std::vec![0; crate::build_defaults::SCHED_RECORD_BYTES / RECORD_BYTES].into_boxed_slice(),
            ),
            ProcessMode::Test => (zeroed(0), zeroed(crate::build_defaults::SCHED_RECORD_BYTES), std::vec::Vec::new().into_boxed_slice()),
        },
    )
}

fn zeroed(length: usize) -> Box<[u8]> {
    std::vec![0; length].into_boxed_slice()
}

// the driver and the run own tens of KiB each of fixed parking and target tables, too large to sit in the root future
fn held<Value>(value: Value) -> std::boxed::Box<Value> {
    std::boxed::Box::new(value)
}

fn new_driver(
    (frame, scratch, linked, scripts, listing_frame, ignored_frame, environments, ignored): &mut CargoStorage,
    process_id: u32,
    bins: BinCounts,
    gate: BinGate,
    lock_watch: LockWatch,
) -> Result<Box<CargoDriver<'_>>, RunError> {
    Ok(Box::new(CargoDriver {
        process_id,
        framer: ArtifactFramer::new(frame, FRAME_CAPACITY - 1).map_err(|_| RunError::CargoStream)?,
        scratch,
        linked: LinkedDirectories::new(linked),
        build_scripts: BuildScriptOutputs::new(scripts),
        listing_framer: ArtifactFramer::new(listing_frame, SOURCE_CAPACITY)
            .map_err(|_| RunError::ListingStream)?,
        ignored_framer: ArtifactFramer::new(ignored_frame, RESULT_LINE_CAPACITY)
            .map_err(|_| RunError::ListingStream)?,
        ignored,
        environments,
        active: None,
        pending_listing: None,
        cargo_backlog: None,
        cargo_eof: false,
        finishing: false,
        reason: b"",
        discovery: DiscoveryFsm::new(bins, gate),
        parked: ParkedArtifacts::new(),
        dropped: 0,
        lock_watch,
    }))
}

fn filter_text<'argument>(parser: &CliParser<'argument>) -> Result<&'argument str, RunError> {
    Ok(parser
        .filter()
        .map(core::str::from_utf8)
        .transpose()
        .map_err(|_| RunError::Arguments)?
        .unwrap_or(""))
}

// a shard runs its tests one at a time so its result lines arrive in listing order and a crash names the test that died
const fn shard_arguments(mode: IgnoreMode) -> &'static [&'static [u8]] {
    match mode {
        IgnoreMode::Skip => &SHARD_ARGUMENTS,
        IgnoreMode::Only => &SHARD_ONLY_ARGUMENTS,
        IgnoreMode::Include => &SHARD_INCLUDE_ARGUMENTS,
    }
}

const fn ignore_arguments(mode: IgnoreMode) -> &'static [&'static [u8]] {
    match mode {
        IgnoreMode::Skip => &[],
        IgnoreMode::Only => &IGNORED_ARGUMENTS,
        IgnoreMode::Include => &INCLUDE_ARGUMENTS,
    }
}

struct StartedChildren {
    cargo: Child,
    doc_tests: Option<Box<DocRun>>,
    bins: BinCounts,
    gate: BinGate,
    lock_watch: LockWatch,
}

// isolated mode reads the metadata first because the build directories come from it; the other modes overlap the read with the builds
async fn start_children(
    parser: &CliParser<'_>,
    settings: &Settings,
    target_mode: TargetDirMode<'_>,
    environment: &[&[u8]],
) -> Result<StartedChildren, RunError> {
    let isolating = target_mode == TargetDirMode::Isolated;
    let mut metadata_child = (settings.bin_gate || isolating)
        .then(|| spawn_metadata(parser, environment))
        .and_then(Result::ok);
    let spawned = metadata_child.is_some();
    let mut facts = None;
    if isolating && let Some(child) = metadata_child.take() {
        facts = read_metadata(child).await;
    }
    let directories = build_directories(target_mode, facts.as_deref())?;
    let cargo = spawn_cargo(parser, environment, directories.as_ref().map(|chosen| &chosen.test))?;
    track(cargo.process_id())?;
    let mut doc_tests = match parser.doc_selection() {
        DocSelection::Excluded | DocSelection::Only => None,
        DocSelection::Included => Some(spawn_doc_tests(
            parser,
            settings.ignore,
            environment,
            directories.as_ref().map(|chosen| &chosen.doc),
        )?),
    };
    if let Some(child) = metadata_child {
        facts = read_metadata(child).await;
    }
    let mut lock_watch = LockWatch::new();
    match (&directories, facts.as_deref().and_then(MetadataFacts::target_directory)) {
        (Some(chosen), _) => lock_watch.set_directory(chosen.test.directory()),
        (None, Some(shared)) => {
            lock_watch.set_directory(shared);
            if let Some(doc) = doc_tests.as_mut() {
                doc.lock_watch.set_directory(shared);
            }
        }
        (None, None) => {}
    }
    let (bins, gate) = resolve_bin_gate(settings, spawned, facts);
    Ok(StartedChildren {
        cargo,
        doc_tests,
        bins,
        gate,
        lock_watch,
    })
}

fn resolve_target_dir<'value>(
    source: &'value BorrowedSettings<'value>,
    parser: &CliParser<'value>,
) -> Result<TargetDirMode<'value>, RunError> {
    TargetDirSources::from_settings(source, parser.target_dir(), crate::build_defaults::TARGET_DIR)
        .resolve()
        .map(|(mode, _)| mode)
        .map_err(RunError::TargetDir)
}

// isolated mode without usable metadata falls back to cargo's own directory, and says so
fn build_directories(
    mode: TargetDirMode<'_>,
    facts: Option<&MetadataFacts>,
) -> Result<Option<BuildDirectories>, RunError> {
    match mode {
        TargetDirMode::Shared => Ok(None),
        TargetDirMode::Path(root) => BuildDirectories::under(root)
            .map(Some)
            .ok_or(RunError::Arguments),
        TargetDirMode::Isolated => {
            let isolated = facts
                .and_then(|found| BuildDirectories::isolated(found.target_directory()?, found.workspace_root()?));
            if isolated.is_none() {
                write_fd(
                    2,
                    b"cargo-val: cargo metadata gave no usable target directory; building in cargo's own target directory\n",
                )?;
            }
            Ok(isolated)
        }
    }
}

type LibdirQueries = (Child, Option<Child>);

fn start_libdir_queries(
    environment: &Environment<'_>,
    parser: &CliParser<'_>,
    inherited: &[&[u8]],
) -> Result<LibdirQueries, RunError> {
    let host_query = spawn_libdir_query(environment.get(b"RUSTC"), None, inherited)
        .map_err(|_| RunError::RustcLibdir)?;
    let target_query = target_triple(parser.cargo_arguments())
        .map(|triple| spawn_libdir_query(environment.get(b"RUSTC"), Some(triple), inherited))
        .transpose()
        .map_err(|_| RunError::RustcLibdir)?;
    Ok((host_query, target_query))
}

fn utf8_variable<'environment>(
    environment: &Environment<'environment>,
    key: &[u8],
) -> Result<Option<&'environment str>, RunError> {
    environment
        .get(key)
        .map(core::str::from_utf8)
        .transpose()
        .map_err(|_| RunError::Environment)
}

/// Spawns `rustc --print target-libdir [--target <triple>]` without waiting for it.
///
/// `rustc` is the `RUSTC` environment value when present, otherwise the name resolved through `PATH`.
/// The printed directory holds the standard library the test binaries load.
///
/// # Errors
///
/// Returns the spawn error.
pub(crate) fn spawn_libdir_query(
    rustc: Option<&[u8]>,
    triple: Option<&[u8]>,
    environment: &[&[u8]],
) -> Result<Child, SpawnError> {
    let mut arguments: Vec<&[u8]> = Vec::from([b"--print".as_slice(), b"target-libdir".as_slice()]);
    if let Some(triple) = triple {
        arguments.push(b"--target");
        arguments.push(triple);
    }
    spawn(rustc.unwrap_or(b"rustc"), &arguments, environment, &[], None)
}

async fn resolve_libdirs(
    queries: LibdirQueries,
    buffer: &mut [u8],
) -> Result<([&str; 2], usize), RunError> {
    let (host_buffer, target_buffer) = buffer.split_at_mut(LIBDIR_CAPACITY);
    let host_length = collect_libdir(queries.0, host_buffer).await?;
    let target_length = match queries.1 {
        Some(query) => collect_libdir(query, target_buffer).await?,
        None => 0,
    };
    let directories = [
        core::str::from_utf8(&host_buffer[..host_length]).map_err(|_| RunError::RustcLibdir)?,
        core::str::from_utf8(&target_buffer[..target_length]).map_err(|_| RunError::RustcLibdir)?,
    ];
    Ok((directories, 1 + usize::from(target_length > 0)))
}

fn finish_run(
    parser: &CliParser<'_>,
    run: &ListingRun<'_>,
    cargo_exit_code: Option<i32>,
) -> Result<RunSummary, RunError> {
    interrupted(run.signals)?;
    if cargo_exit_code.is_some_and(|code| code != 0) {
        run.interrupt_doc_tests();
        return Err(RunError::CargoFailed);
    }
    let doc_tests = run.finish_doc_tests(parser.doc_selection() == DocSelection::Included)?;
    let stop = run.policy.borrow().report();
    conclude(
        parser,
        run.settings,
        &run.targets.borrow(),
        run.started_millis,
        stop,
        doc_tests,
    )
}

fn monotonic_millis() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    u64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

type LineBuffer = Line<512>;

impl<const CAPACITY: usize> Line<CAPACITY> {
    fn write(&self, descriptor: i32) -> Result<(), RunError> {
        write_fd(descriptor, self.bytes())
    }
}

fn write_stop_trailer(report: &StopReport, shards: bool) -> Result<(), RunError> {
    let mut line = LineBuffer::new();
    line.text(b"cargo-val: stopped at --max-fail ");
    line.number(report.max_fail);
    line.text(b"; in flight at trip ");
    line.number(report.in_flight_at_trip);
    line.text(if shards { b" (shard children terminated, their unreported names withdrawn); launched after trip " } else { b" (finished normally); launched after trip " });
    line.number(report.launched_after_trip);
    line.text(b"; listed tests not run ");
    line.number(report.listed_not_run);
    line.text(b"; test binaries not listed ");
    line.number(report.targets_not_listed);
    line.text(b"; doc tests at trip: ");
    line.text(report.doc_at_trip.text());
    line.text(b"\n");
    line.write(1)
}

fn write_no_tests(parser: &CliParser<'_>, ignore: IgnoreMode) -> Result<(), RunError> {
    let mut line = LineBuffer::new();
    line.text(b"cargo-val: no tests to run; selection matched nothing: filter ");
    match parser.filter() {
        Some(filter) => {
            line.text(b"'");
            line.text(filter);
            line.text(b"'");
        }
        None => line.text(b"none"),
    }
    line.text(if parser.exact() {
        b", exact"
    } else {
        b", substring"
    });
    match ignore {
        IgnoreMode::Skip => {}
        IgnoreMode::Only => line.text(b", ignored only"),
        IgnoreMode::Include => line.text(b", including ignored"),
    }
    line.text(b", cargo arguments:");
    for argument in parser.cargo_arguments() {
        line.text(b" ");
        line.text(argument);
    }
    line.text(b"\n");
    line.write(2)
}

const DOC_STDERR_CAPACITY: usize = 16384;
const NO_LIBRARY_MESSAGE: &[u8] = b"no library targets found";

/// What became of the doc-test child once the run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFinish {
    /// The child reported its results to completion.
    Ran,
    /// Fail-fast interrupted the child before it started executing doctests.
    Stopped,
    /// The default selection found no library target, so Cargo had no doc tests to run.
    NoLibrary,
}

/// Doc-test results folded into the run totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocTotals {
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    /// `running N tests` headers seen, one per library crate.
    pub blocks: usize,
    /// Headers that announced zero doctests.
    pub empty_blocks: usize,
    pub finish: DocFinish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocPhase {
    Starting,
    Executing,
    Interrupted,
}

pub(crate) struct DocRun {
    child: Option<Child>,
    process_id: u32,
    parser: DocParser,
    stdout_eof: bool,
    stderr_eof: bool,
    exit_code: Option<i32>,
    phase: DocPhase,
    stderr_bytes: [u8; DOC_STDERR_CAPACITY],
    stderr_length: usize,
    lock_watch: LockWatch,
    backlog: Option<(Range<u32>, usize)>,
}

fn spawn_doc_tests(
    parser: &CliParser<'_>,
    ignore: IgnoreMode,
    environment: &[&[u8]],
    directory: Option<&ChildDirectory>,
) -> Result<Box<DocRun>, RunError> {
    let mut arguments: Vec<&[u8]> = Vec::from([b"test".as_slice(), b"--doc".as_slice()]);
    if let Some(directory) = directory {
        arguments.push(directory.argument());
    }
    for argument in parser
        .cargo_arguments()
        .iter()
        .copied()
        .chain(parser.filter())
    {
        if argument.contains(&0) {
            return Err(RunError::Arguments);
        }
        arguments.push(argument);
    }
    if parser.exact() || ignore != IgnoreMode::Skip {
        arguments.push(b"--");
    }
    if parser.exact() {
        arguments.push(b"--exact");
    }
    match ignore {
        IgnoreMode::Skip => {}
        IgnoreMode::Only => arguments.push(b"--ignored"),
        IgnoreMode::Include => arguments.push(b"--include-ignored"),
    }
    let child = spawn_in_group(b"cargo", &arguments, environment, &[], None).map_err(|_| RunError::Spawn)?;
    let process_id = child.process_id();
    track_group(process_id).map_err(|_| RunError::Reactor)?;
    if child.lower_priority().is_err() {
        write_fd(2, b"cargo-val: could not lower the doc test priority\n")?;
    }
    let mut lock_watch = LockWatch::new();
    if let Some(directory) = directory {
        lock_watch.set_directory(directory.directory());
    }
    Ok(Box::new(DocRun {
        child: Some(child),
        process_id,
        parser: DocParser::new(),
        stdout_eof: false,
        stderr_eof: false,
        exit_code: None,
        phase: DocPhase::Starting,
        backlog: None,
        stderr_bytes: [0; DOC_STDERR_CAPACITY],
        stderr_length: 0,
        lock_watch,
    }))
}

impl DocRun {
    pub(crate) fn finished(&self) -> bool {
        self.stdout_eof && self.stderr_eof && self.exit_code.is_some() && self.backlog.is_none()
    }

    pub(crate) fn take_child(&mut self) -> Option<Child> {
        self.child.take()
    }

    /// Feeds `bytes` from `offset` one line at a time, stopping while the status queue lacks room for one more event; returns how far it got.
    pub(crate) fn stdout_lines(
        &mut self,
        bytes: &[u8],
        mut offset: usize,
        color: bool,
        observation: &DocObservation,
    ) -> Result<usize, RunError> {
        while offset < bytes.len() {
            if !status_queue::has_room() {
                status_queue::defer();
                break;
            }
            let rest = &bytes[offset..];
            let line = memchr::memchr(b'\n', rest).map_or(rest.len(), |newline| newline + 1);
            let phase = &mut self.phase;
            self.parser.feed(&rest[..line], |event| {
                record_doc_event(event, phase, color, observation)
            })?;
            offset += line;
        }
        Ok(offset)
    }

    pub(crate) fn set_backlog(&mut self, backlog: Option<(Range<u32>, usize)>) {
        self.backlog = backlog;
    }

    pub(crate) fn backlog(&self) -> Option<(Range<u32>, usize)> {
        self.backlog.clone()
    }

    pub(crate) fn stdout_ended(&mut self) {
        self.stdout_eof = true;
    }

    pub(crate) fn stderr_chunk(&mut self, bytes: &[u8]) -> Result<(), RunError> {
        self.keep_stderr(bytes);
        announce_lock_wait(&mut self.lock_watch, bytes)
    }

    pub(crate) fn stderr_ended(&mut self) {
        self.stderr_eof = true;
    }

    pub(crate) fn exited(&mut self, code: i32) {
        release_group(self.process_id);
        self.exit_code = Some(code);
    }

    fn keep_stderr(&mut self, bytes: &[u8]) {
        let room = DOC_STDERR_CAPACITY - self.stderr_length;
        let taken = bytes.len().min(room);
        self.stderr_bytes[self.stderr_length..self.stderr_length + taken]
            .copy_from_slice(&bytes[..taken]);
        self.stderr_length += taken;
    }

    fn state_for_trip(&mut self) -> DocAtTrip {
        if self.exit_code.is_some() {
            DocAtTrip::AlreadyFinished
        } else if self.phase == DocPhase::Executing {
            DocAtTrip::ExecutingLeftToFinish
        } else {
            interrupt_group(self.process_id);
            self.phase = DocPhase::Interrupted;
            DocAtTrip::InterruptedBeforeExecuting
        }
    }

    fn conclude(&self, implicit: bool) -> Result<DocFinish, RunError> {
        let stderr = &self.stderr_bytes[..self.stderr_length];
        if self.phase == DocPhase::Interrupted {
            return Ok(DocFinish::Stopped);
        }
        if implicit && contains(stderr, NO_LIBRARY_MESSAGE) {
            return Ok(DocFinish::NoLibrary);
        }
        write_fd(2, stderr)?;
        Ok(DocFinish::Ran)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn record_doc_event(
    event: DocEvent<'_>,
    phase: &mut DocPhase,
    color: bool,
    observation: &DocObservation,
) -> Result<(), RunError> {
    match event {
        DocEvent::Running { count } => {
            if *phase == DocPhase::Starting {
                *phase = DocPhase::Executing;
            }
            observation.blocks.set(observation.blocks.get() + 1);
            if count == 0 {
                observation
                    .empty_blocks
                    .set(observation.empty_blocks.get() + 1);
            }
            Ok(())
        }
        DocEvent::Result { name, outcome, .. } => report_doc_result(name, outcome, color, observation),
        DocEvent::FailureLine(line) => {
            write_fd(2, line)?;
            write_fd(2, b"\n")
        }
        DocEvent::Finished { .. } => Ok(()),
    }
}

fn report_doc_result(
    name: &[u8],
    outcome: DocOutcome,
    color: bool,
    observation: &DocObservation,
) -> Result<(), RunError> {
    let (terminal, counter) = match outcome {
        DocOutcome::Passed => (TerminalOutcome::Passed, &observation.passed),
        DocOutcome::Failed => (TerminalOutcome::Failed, &observation.failed),
        DocOutcome::Ignored => (TerminalOutcome::Ignored, &observation.ignored),
    };
    counter.set(counter.get() + 1);
    let mut report = [0; NAME_CAPACITY + 34];
    let name_length = name.len().min(NAME_CAPACITY);
    report[..name_length].copy_from_slice(&name[..name_length]);
    report[name_length] = b' ';
    let label_length = write_report(&[terminal], color, &mut report[name_length + 1..])
        .map_err(|_| RunError::Outcome)?;
    write_fd(1, &report[..name_length + 1 + label_length])
}

fn write_doc_notice(totals: &DocTotals) -> Result<(), RunError> {
    let mut line = LineBuffer::new();
    match totals.finish {
        DocFinish::NoLibrary => line.text(b"cargo-val: doc tests: no library target to test\n"),
        DocFinish::Stopped => line.text(b"cargo-val: doc tests: stopped before executing\n"),
        DocFinish::Ran if totals.passed + totals.failed + totals.ignored == 0 => {
            line.text(b"cargo-val: doc tests: running 0 tests (");
        }
        DocFinish::Ran => return Ok(()),
    }
    if totals.finish == DocFinish::Ran {
        line.number(totals.blocks);
        line.text(b" doc test run(s) reported, ");
        line.number(totals.empty_blocks);
        line.text(b" with zero tests)\n");
    }
    line.write(1)
}

impl ListingRun<'_> {
    fn finish_doc_tests(&self, implicit: bool) -> Result<Option<DocTotals>, RunError> {
        let Some(doc) = self.doc.borrow_mut().take() else {
            return Ok(None);
        };
        Ok(Some(close_doc_tests(&doc, implicit, &self.docs)?))
    }
}

fn close_doc_tests(
    doc: &DocRun,
    implicit: bool,
    observation: &DocObservation,
) -> Result<DocTotals, RunError> {
    let finish = doc.conclude(implicit)?;
    let failed = observation.failed.get();
    if finish == DocFinish::Ran && doc.exit_code != Some(0) && failed == 0 {
        return Err(RunError::DocTests);
    }
    Ok(DocTotals {
        passed: observation.passed.get(),
        failed,
        ignored: observation.ignored.get(),
        blocks: observation.blocks.get(),
        empty_blocks: observation.empty_blocks.get(),
        finish,
    })
}

fn doc_counts(doc: &DocTotals) -> Counts {
    Counts {
        discovered: doc.passed + doc.failed + doc.ignored,
        passed: doc.passed,
        failed: doc.failed,
        ignored: doc.ignored,
        filtered: 0,
    }
}

fn write_summary(
    targets: &TargetTable,
    doc_tests: Option<&DocTotals>,
    started_millis: u64,
) -> Result<Counts, RunError> {
    let mut total = Counts::default();
    for target in 0..targets.len() {
        let counts = targets.counts(target);
        write_fd(1, render_counts(targets.id(target), &counts, None).bytes())?;
        if !counts.balanced() {
            let mut line = Line::<CONTEXT_CAPACITY>::new();
            line.text(b"cargo-val: ");
            line.text(targets.id(target));
            line.text(b": discovered does not equal passed + failed + ignored + filtered\n");
            write_fd(2, line.bytes())?;
            return Err(RunError::SummaryMismatch);
        }
        total = total.plus(&counts);
    }
    if let Some(doc) = doc_tests {
        let counts = doc_counts(doc);
        write_fd(1, render_counts(b"doc-tests", &counts, None).bytes())?;
        total = total.plus(&counts);
    }
    let wall = monotonic_millis().saturating_sub(started_millis);
    write_fd(1, render_counts(b"total", &total, Some(wall)).bytes())?;
    Ok(total)
}

fn conclude(
    parser: &CliParser<'_>,
    settings: &Settings,
    targets: &TargetTable,
    started_millis: u64,
    stop: Option<StopReport>,
    doc_tests: Option<DocTotals>,
) -> Result<RunSummary, RunError> {
    if let Some(report) = stop {
        write_stop_trailer(&report, settings.process == ProcessMode::Shard)?;
    }
    if let Some(doc) = &doc_tests {
        write_doc_notice(doc)?;
    }
    let total = write_summary(targets, doc_tests.as_ref(), started_millis)?;
    let empty = total.passed + total.failed == 0;
    if empty && settings.no_tests != NoTestsPolicy::Pass {
        write_no_tests(parser, settings.ignore)?;
    }
    Ok(RunSummary {
        passed: total.passed,
        failed: total.failed,
        stop,
        no_tests: empty.then_some(settings.no_tests),
        doc_tests,
    })
}

async fn run_doc_only(
    parser: &CliParser<'_>,
    settings: &Settings,
    target_mode: TargetDirMode<'_>,
    started_millis: u64,
    environment: &[&[u8]],
) -> Result<RunSummary, RunError> {
    let signals = SignalPipe::install().map_err(|_| RunError::Reactor)?;
    let metadata_child = (!matches!(target_mode, TargetDirMode::Path(_)))
        .then(|| spawn_metadata(parser, environment))
        .and_then(Result::ok);
    let facts = match metadata_child {
        Some(child) => read_metadata(child).await,
        None => None,
    };
    let directories = build_directories(target_mode, facts.as_deref())?;
    let mut doc = spawn_doc_tests(
        parser,
        settings.ignore,
        environment,
        directories.as_ref().map(|chosen| &chosen.doc),
    )?;
    if directories.is_none()
        && let Some(shared) = facts.as_deref().and_then(MetadataFacts::target_directory)
    {
        doc.lock_watch.set_directory(shared);
    }
    let observation = DocObservation::default();
    observe_doc(&mut doc, &signals, settings.color, &observation).await?;
    interrupted(&signals)?;
    let totals = close_doc_tests(&doc, false, &observation)?;
    conclude(
        parser,
        settings,
        &TargetTable::new(),
        started_millis,
        None,
        Some(totals),
    )
}

fn interrupted(signals: &SignalPipe) -> Result<(), RunError> {
    signals.drain().map_err(|_| RunError::Reactor)?;
    let Some(signal) = signals.received() else {
        return Ok(());
    };
    Err(RunError::Interrupted(signal))
}

#[cfg(test)]
mod tests {
    use super::{ERROR_LINE_CAPACITY, ErrorLine, LaunchCause, OutcomeError, RunError};

    fn rendered(cause: LaunchCause) -> std::string::String {
        let line: ErrorLine = RunError::Launch(cause).render();
        std::string::String::from_utf8(line.bytes().to_vec()).expect("launch line is UTF-8")
    }

    #[test]
    fn launch_failure_line_names_each_cause() {
        let cases = [
            (
                LaunchCause::Full,
                "cargo-val: exact test launch failed: no live-test slot is free\n",
            ),
            (
                LaunchCause::Duplicate,
                "cargo-val: exact test launch failed: the same test name is already live in this test binary\n",
            ),
            (
                LaunchCause::NameTooLong {
                    required: 1500,
                    available: 1024,
                },
                "cargo-val: exact test launch failed: test name needs 1500 bytes and 1024 are available\n",
            ),
            (
                LaunchCause::Spawn { os_code: 2 },
                "cargo-val: exact test launch failed: spawn failed with os error 2\n",
            ),
            (
                LaunchCause::CapacityChanged,
                "cargo-val: exact test launch failed: the live-test table rejected the child after its capacity check\n",
            ),
            (
                LaunchCause::Accounting(OutcomeError::CapacityExhausted),
                "cargo-val: exact test launch failed: outcome accounting refused the child: the live-session table is full\n",
            ),
        ];
        let mut checked = 0;
        for (cause, expected) in cases {
            assert_eq!(rendered(cause), expected);
            checked += 1;
        }
        assert_eq!(checked, 6);
    }

    #[test]
    fn longest_launch_failure_line_fits_the_error_line() {
        let longest = LaunchCause::NameTooLong {
            required: usize::MAX,
            available: usize::MAX,
        };
        let line = rendered(longest);
        assert!(line.ends_with("are available\n"), "{line}");
        assert!(line.len() < ERROR_LINE_CAPACITY);
    }

    #[test]
    fn libtest_result_lines_classify_the_exact_child() {
        use crate::runner::LibtestVerdict;
        let cases: [(&[u8], LibtestVerdict); 6] = [
            (b"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 716 filtered out; finished in 0.00s", LibtestVerdict::Ran),
            (b"test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.00s", LibtestVerdict::Ran),
            (b"test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 716 filtered out; finished in 0.00s", LibtestVerdict::Skipped),
            (b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 717 filtered out; finished in 0.00s", LibtestVerdict::NothingRan),
            (b"running 1 test", LibtestVerdict::Unreported),
            (b"test result: ok. garbled", LibtestVerdict::Unreported),
        ];
        let mut checked = 0;
        for (line, expected) in cases {
            let mut output = super::ChildOutput::new();
            output.observe_stdout(line);
            output.observe_stdout(b"\n");
            assert_eq!(output.verdict(), expected, "{}", std::string::String::from_utf8_lossy(line));
            checked += 1;
        }
        assert_eq!(checked, cases.len());
    }

    fn collected(chunk_size: usize, listing: &[u8]) -> Result<crate::ignored_set::IgnoredSet, RunError> {
        let mut frame_bytes = [0; super::RESULT_LINE_CAPACITY + 1];
        let mut framer = crate::artifact_stream::ArtifactFramer::new(&mut frame_bytes, super::RESULT_LINE_CAPACITY)
            .expect("framer fits its record limit");
        let mut set = crate::ignored_set::IgnoredSet::new();
        for chunk in listing.chunks(chunk_size) {
            super::feed_ignored_lines(chunk, &mut framer, &mut set)?;
        }
        Ok(set)
    }

    #[test]
    fn real_ignored_listing_fills_the_set_at_every_chunk_size() {
        const IGNORED: &[u8] = include_bytes!("../tests/fixtures/listing/ignore_modes_list_ignored.txt");
        for chunk_size in [1, 2, 7, 16, IGNORED.len()] {
            let set = collected(chunk_size, IGNORED).expect("real listing frames");

            assert_eq!(set.len(), 1, "chunk size {chunk_size}");
            assert!(set.contains(b"fixture::ignored_with_reason"), "chunk size {chunk_size}");
        }
    }

    #[test]
    fn full_listing_names_every_test_and_skips_the_summary() {
        const LISTING: &[u8] = include_bytes!("../tests/fixtures/listing/ignore_modes_list.txt");
        let set = collected(5, LISTING).expect("real listing frames");

        assert_eq!(set.len(), 3);
        assert!(set.contains(b"fixture::passes"));
        assert!(set.contains(b"fixture::fails"));
    }

    #[test]
    fn invalid_utf8_listing_line_is_refused() {
        let outcome = collected(4, b"fixture::\xff: test\n");

        assert!(matches!(outcome, Err(RunError::ListingStream)));
    }

    #[test]
    fn listing_line_past_the_record_limit_is_refused() {
        let mut listing = std::vec![b'a'; super::RESULT_LINE_CAPACITY + 1];
        listing.extend_from_slice(b"\n");

        assert!(matches!(collected(64, &listing), Err(RunError::ListingStream)));
    }

    #[test]
    fn refusal_without_detail_renders_its_static_message() {
        let line = RunError::Reactor.render();
        assert_eq!(line.bytes(), RunError::Reactor.message());
    }
}

#[cfg(all(test, unix))]
mod listing_allocation_tests {
    use core::cell::{Cell, RefCell};

    use super::{DocObservation, ENVIRONMENT_CAPACITY, LISTING_SLOTS, ListingRun, PendingListing, Settings, TargetTable, spawn_listing};
    use crate::edge_spawn::held_launch_storage;
    use crate::cargo_message::TestArtifact;
    use crate::config::{IgnoreMode, NoTestsPolicy};
    use crate::edge_signal::SignalPipe;
    use crate::shard_sched::{SchedTable, SpecialPlan};
    use crate::stop::StopPolicy;

    fn reap(listing: &mut PendingListing) {
        for child in [listing.child.take(), listing.ignored_child.take()].into_iter().flatten() {
            let mut status = 0;
            let process = child.process_id().cast_signed();
            assert_eq!(unsafe { libc::waitpid(process, &raw mut status, 0) }, process);
        }
    }

    #[test]
    fn spawning_a_listing_child_allocates_zero_times() {
        crate::edge_exit::fixture::in_single_threaded_process(spawning_a_listing_child_allocates_zero_times_in_isolation);
    }

    fn spawning_a_listing_child_allocates_zero_times_in_isolation() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let inherited_storage = std::env::vars()
            .map(|(key, value)| std::format!("{key}={value}"))
            .collect::<std::vec::Vec<_>>();
        let inherited = inherited_storage.iter().map(std::string::String::as_bytes).collect::<std::vec::Vec<_>>();
        let settings = Settings {
            bin_gate: false,
            debug: false,
            ignore: IgnoreMode::Include,
            filterset: None,
            jobs: 2,
            max_fail: None,
            no_tests: NoTestsPolicy::Pass,
            process: crate::config::ProcessMode::Test,
            color: false,
        };
        let signals = SignalPipe::install().expect("signal pipe installs once in the unit test process");
        let run = ListingRun {
            signals: &signals,
            filter: "",
            exact: false,
            targets: RefCell::new(TargetTable::new()),
            started_millis: 0,
            harness_arguments: &[],
            environment: &inherited,
            library_directories: &[],
            inherited_dylib_path: None,
            home: None,
            settings: &settings,
            prepared: None,
            docs: DocObservation::default(),
            policy: RefCell::new(StopPolicy::new(None)),
            doc: RefCell::new(None),
            launch_storage: RefCell::new(held_launch_storage()),
            ready: core::cell::Cell::new(None),
            withdrawn_ready: Cell::new(None),
            shards: RefCell::new(crate::shard_arena::ShardArena::new(&mut [])),
            sched: RefCell::new(SchedTable::new(&mut [])),
            specials: RefCell::new(SpecialPlan::new(&mut [])),
            sched_target: Cell::new(None),
            stop_sent: Cell::new(false),
        };
        let artifact = TestArtifact {
            executable,
            package_id: "path+file:///repo/parser#0.3.1",
            manifest_path: manifest,
            target_name: "cargo_val",
            target_kind: "lib",
        };

        let mut environments = super::zeroed(ENVIRONMENT_CAPACITY * LISTING_SLOTS);

        let (listing, allocations) =
            crate::test_alloc::measure_allocations(|| spawn_listing(&artifact, &run, "", None, 1, &mut environments));

        let mut listing = listing.expect("the listing child starts");
        reap(&mut listing);
        std::println!("{{\"listing_spawn_allocations\":{allocations}}}");
        assert_eq!(allocations, 0);
    }
}
