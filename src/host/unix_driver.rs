//! One poll-driven owner of Cargo discovery, libtest listings, and child outcomes.

use core::cell::{Cell, RefCell};
use core::ffi::{CStr, c_char, c_int};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use conflaguration::{BorrowedSettings, SettingsSource, resolve_color};
use proxima_core::io::AsyncRead;

use crate::ExecutablePath;
use crate::artifact_stream::ArtifactFramer;
use crate::cargo_message::TestArtifact;
use crate::cargo_source::{CargoSource, CargoSourceStep};
use crate::cli::{CliParser, DocSelection};
use crate::config::{
    FailFastError, FailFastSources, FiltersetSources, HarnessFault, HarnessRequest, IgnoreError,
    IgnoreMode, IgnoreSources, NoTestsError, NoTestsPolicy, NoTestsSources, ThreadSources,
    TargetDirError, TargetDirMode, TargetDirSources, ThreadsError, ThreadsFault, ThreadsSource,
};
use crate::discover_listing::{
    DocEvent, DocOutcome, DocParser, ListingLine, ListingSource, ListingSourceError,
    ListingSourceStep, parse_listing_line,
};
use crate::filterset::{FilterError, Filterset, Prepared};
use crate::ignored_set::{IgnoredSet, IgnoredSetFull};
use crate::isolated_target::{BuildDirectories, ChildDirectory, LockKind, LockWatch};
use crate::metadata_scan::{BinCounts, MetadataFacts, MetadataScan};
use crate::parked_artifacts::{ParkFault, ParkedArtifacts};
use crate::summary::{Counts, Tally, TargetTable, render as render_counts};
use crate::edge_signal_unix::{SignalPipe, conventional_status, release_child, track_child};
use crate::host::unix_env::UnixEnvironment;
use crate::host::unix_rustc::RustcLibdirQuery;
use crate::host::unix_spawn::{UnixChild, spawn_raw_unix, spawn_raw_unix_in_new_group};
use crate::host_event::HostEvent;
use crate::outcomes::{OutcomeError, OutcomeLedger, TerminalOutcome};
use crate::reporter::write_report;
use crate::runner::{
    LaunchCause, LibtestVerdict, RunningUnixChild, UnixLaunchInput, UnixLaunchOwner,
    UnixOutcomeError, UnixOutcomeInput, UnixOutcomeStep,
};
use crate::schedule::{SessionSlotId, SessionSlots};
use crate::selection::{NameFilter, Selection};
use crate::stop::{DocAtTrip, Observation, StopPolicy, StopReport};
use crate::test_environment::{
    BuildScriptOutput, BuildScriptOutputs, DYLIB_PATH_KEY_C, LinkedDirectories, MAX_ENTRIES,
    TestEnvironmentInput, TestEnvironmentLayout,
};
use crate::test_intent::ExactTestIntent;
use crate::unix_arguments::{UnixArgumentsError, merge_environment};

#[cfg(target_os = "linux")]
use crate::host::linux_exit::{
    LinuxExitEvent as NativeEvent, LinuxExitPort as NativePort, LinuxExitStatus as NativeStatus,
    LinuxPidWatch as NativeWatch,
};
#[cfg(target_os = "linux")]
use crate::host::linux_pipe::LinuxPipeReader as NativeReader;
#[cfg(target_os = "macos")]
use crate::host::macos_exit::{
    MacosExitEvent as NativeEvent, MacosExitPort as NativePort, MacosExitStatus as NativeStatus,
    MacosProcessWatch as NativeWatch,
};
#[cfg(target_os = "macos")]
use crate::host::macos_pipe::MacosPipeReader as NativeReader;

const LIVE_CAPACITY: usize = 64;
const DECIMAL_DIGITS: [u8; 10] = *b"0123456789";
const IGNORED_ARGUMENTS: [&[u8]; 1] = [b"--ignored"];
const INCLUDE_ARGUMENTS: [&[u8]; 1] = [b"--include-ignored"];
const IDENTITY_CAPACITY: usize = 128;
const NAME_CAPACITY: usize = 1024;
const ENV_CAPACITY: usize = 1024;
const PATH_CAPACITY: usize = 4096;
const SCRATCH_CAPACITY: usize = 16384;
const LINKED_CAPACITY: usize = 32768;
const BUILD_SCRIPT_CAPACITY: usize = 65536;
const ENVIRONMENT_CAPACITY: usize = LINKED_CAPACITY + 16384;
const LIBDIR_CAPACITY: usize = 1024;
const SOURCE_CAPACITY: usize = 8192;
const FRAME_CAPACITY: usize = 131_072;
const CAPTURE_CAPACITY: usize = 4096;
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

struct PendingListing {
    target: usize,
    child: UnixChild,
    ignored_child: Option<UnixChild>,
    identity: Identity,
    path: [u8; PATH_CAPACITY],
    path_length: usize,
    environment: [u8; ENVIRONMENT_CAPACITY],
    layout: TestEnvironmentLayout,
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
    DocTests,
    Spawn,
    Reactor,
    CargoStream,
    ListingStream,
    Launch(LaunchCause),
    Outcome,
    CargoFailed,
    Interrupted(c_int),
    ListingFailed,
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
            Self::DocTests => b"cargo-val: cargo test --doc failed before reporting results\n",
            Self::Spawn => b"cargo-val: child spawn failed\n",
            Self::Reactor => b"cargo-val: reactor operation failed\n",
            Self::CargoStream => b"cargo-val: cargo output could not be parsed\n",
            Self::ListingStream => b"cargo-val: libtest listing could not be parsed\n",
            Self::Launch(_) => b"cargo-val: exact test launch failed\n",
            Self::Outcome => b"cargo-val: child completion failed\n",
            Self::CargoFailed => b"cargo-val: cargo test build failed\n",
            Self::ListingFailed => b"cargo-val: libtest listing failed\n",
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
            Self::Spawn { errno } => {
                line.text(b"posix_spawn failed with errno ");
                line.number(usize::try_from(errno.unsigned_abs()).unwrap_or(usize::MAX));
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

fn write_arguments_error<const CAPACITY: usize>(
    line: &mut Line<CAPACITY>,
    error: UnixArgumentsError,
) {
    line.text(b"argument lowering failed: ");
    let (reason, sizes): (&[u8], Option<(usize, usize)>) = match error {
        UnixArgumentsError::InteriorNul => (b"an argument contains a NUL byte", None),
        UnixArgumentsError::InvalidEnvironmentEntry => {
            (b"an environment entry has no key before '='", None)
        }
        UnixArgumentsError::ArgumentStorageTooSmall {
            required,
            available,
        } => (b"argument bytes need", Some((required, available))),
        UnixArgumentsError::ArgumentPointersTooSmall {
            required,
            available,
        } => (b"argument pointers need", Some((required, available))),
        UnixArgumentsError::EnvironmentPointersTooSmall {
            required,
            available,
        } => (b"environment pointers need", Some((required, available))),
    };
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

struct TestWatch {
    slot: SessionSlotId,
    process_id: libc::pid_t,
    stdout_token: u64,
    stderr_token: u64,
    process: NativeWatch,
    output: ChildOutput,
}

impl TestWatch {
    fn stream_for(&self, token: u64) -> Option<bool> {
        if token == self.stdout_token {
            Some(true)
        } else if token == self.stderr_token {
            Some(false)
        } else {
            None
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
    color: bool,
}

fn runtime_settings(
    environment: &UnixEnvironment<'_>,
    harness: &HarnessRequest<'_>,
    parser: &CliParser<'_>,
) -> Result<Settings, RunError> {
    let mut pairs = [("", ""); 11];
    let mut count = 0;
    for (key, c_key) in [
        ("RUST_TEST_THREADS", c"RUST_TEST_THREADS"),
        ("CARGO_VAL_JOBS", c"CARGO_VAL_JOBS"),
        ("CARGO_VAL_MAX_FAIL", c"CARGO_VAL_MAX_FAIL"),
        ("CARGO_VAL_FAIL_FAST", c"CARGO_VAL_FAIL_FAST"),
        ("CARGO_VAL_NO_TESTS", c"CARGO_VAL_NO_TESTS"),
        ("CARGO_VAL_COLOR", c"CARGO_VAL_COLOR"),
        ("NO_COLOR", c"NO_COLOR"),
        ("CARGO_VAL_IGNORED", c"CARGO_VAL_IGNORED"),
        ("CARGO_VAL_FILTERSET", c"CARGO_VAL_FILTERSET"),
        ("CARGO_VAL_BIN_GATE", c"CARGO_VAL_BIN_GATE"),
        ("CARGO_VAL_LOG", c"CARGO_VAL_LOG"),
    ] {
        if let Some(value) = environment.get(c_key) {
            pairs[count] = (
                key,
                core::str::from_utf8(value).map_err(|_| RunError::Settings)?,
            );
            count += 1;
        }
    }
    let source = BorrowedSettings::new(&pairs[..count]);
    let jobs =
        ThreadSources::from_settings(&source, harness.test_threads, crate::build_defaults::JOBS)
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

fn write_fd(descriptor: c_int, mut bytes: &[u8]) -> Result<(), RunError> {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        let written = usize::try_from(written).map_err(|_| RunError::Reactor)?;
        if written == 0 {
            return Err(RunError::Reactor);
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

fn track(target: libc::pid_t) -> Result<(), RunError> {
    track_child(target).map_err(|_| RunError::Reactor)
}

// the pid is untracked the moment it is reaped, before the kernel can hand it to an unrelated process
fn released<Status>(status: Option<Status>, target: libc::pid_t) -> Option<Status> {
    if status.is_some() {
        release_child(target);
    }
    status
}

fn reap_into(
    process: &mut NativeWatch,
    target: libc::pid_t,
    exit_status: &mut Option<NativeStatus>,
) -> Result<(), RunError> {
    if let Some(status) = released(process.reap().map_err(|_| RunError::Reactor)?, target) {
        *exit_status = Some(status);
    }
    Ok(())
}

fn deliver_exit(
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watch_slot: &mut Option<TestWatch>,
    exit_code: i32,
    reporting: Reporting<'_>,
) -> Result<(), RunError> {
    let Some(watch) = watch_slot.as_ref() else {
        return Ok(());
    };
    let outcome = deliver(
        ledger,
        HostEvent::Exit {
            slot: watch.slot,
            status: exit_code,
        },
        reporting,
        watch.output.verdict(),
    )?;
    if let Some(outcome) = outcome {
        if outcome == TerminalOutcome::Failed {
            watch.output.write_failure()?;
        }
        *watch_slot = None;
    }
    Ok(())
}

fn reap_unreported_children(
    port: &NativePort,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    reporting: Reporting<'_>,
) -> Result<(), RunError> {
    for watch_slot in watches.iter_mut() {
        let Some(watch) = watch_slot.as_mut() else {
            continue;
        };
        let Some(status) = released(
            watch.process.reap().map_err(|_| RunError::Reactor)?,
            watch.process_id,
        ) else {
            continue;
        };
        #[cfg(target_os = "macos")]
        if watch.process.registered() {
            port.unwatch_process(
                ledger
                    .sessions()
                    .get(watch.slot)
                    .ok_or(RunError::Outcome)?
                    .child()
                    .process_id(),
            )
            .map_err(|_| RunError::Reactor)?;
        }
        #[cfg(target_os = "linux")]
        let _ = port;
        deliver_exit(ledger, watch_slot, status.code.unwrap_or(1), reporting)?;
    }
    Ok(())
}

fn environment_pointers(
    entries: &[&CStr],
    pointers: &mut [*const c_char],
) -> Result<usize, RunError> {
    if pointers.len() < entries.len() + 1 {
        return Err(RunError::Environment);
    }
    for (index, entry) in entries.iter().enumerate() {
        pointers[index] = entry.as_ptr();
    }
    pointers[entries.len()] = core::ptr::null();
    Ok(entries.len() + 1)
}

fn reader_pair(
    port: &NativePort,
    child: UnixChild,
) -> Result<(NativeReader<'_>, NativeReader<'_>, NativeWatch), RunError> {
    let (process_id, stdout, stderr) = child.into_parts();
    let watch = port.watch(process_id).map_err(|_| RunError::Reactor)?;
    let stdout_token = port
        .watch_descriptor(stdout.raw())
        .map_err(|_| RunError::Reactor)?;
    let stderr_token = port
        .watch_descriptor(stderr.raw())
        .map_err(|_| RunError::Reactor)?;
    Ok((
        NativeReader::new(stdout, port, stdout_token),
        NativeReader::new(stderr, port, stderr_token),
        watch,
    ))
}

fn drain_reader(
    reader: &mut NativeReader<'_>,
    descriptor: c_int,
    mut watch: Option<&mut LockWatch>,
) -> Result<bool, RunError> {
    let mut bytes = [0; SOURCE_CAPACITY];
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match Pin::new(&mut *reader).poll_read(&mut context, &mut bytes) {
            Poll::Ready(Ok(0)) => return Ok(true),
            Poll::Ready(Ok(length)) => {
                write_fd(descriptor, &bytes[..length])?;
                if let Some(watch) = watch.as_deref_mut() {
                    announce_lock_wait(watch, &bytes[..length])?;
                }
            }
            Poll::Ready(Err(_)) => return Err(RunError::Reactor),
            Poll::Pending => return Ok(false),
        }
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
fn report_nothing_ran(reporting: Reporting<'_>, target: Option<usize>, name: &[u8]) -> RunError {
    let mut line = Line::<CONTEXT_CAPACITY>::new();
    line.text(b"cargo-val: test ");
    line.text(name);
    line.text(b" in ");
    if let Some(target) = target {
        line.text(reporting.targets.borrow().id(target));
    }
    line.text(b" exited 0 but libtest reported 0 passed, 0 failed, 0 ignored\n");
    let _ = write_fd(2, line.bytes());
    RunError::NothingRan
}

fn deliver(
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    event: HostEvent<'_>,
    reporting: Reporting<'_>,
    verdict: LibtestVerdict,
) -> Result<Option<TerminalOutcome>, RunError> {
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
    let target = ledger.sessions().get(slot).map(RunningUnixChild::target);
    let step = UnixOutcomeInput {
        ledger,
        event,
        report_output: &mut report[name_length..],
        color: reporting.color,
        verdict,
    }
    .apply()
    .map_err(|error| match error {
        UnixOutcomeError::NothingRan => {
            report_nothing_ran(reporting, target, &report[..name_length.saturating_sub(1)])
        }
        _ => RunError::Outcome,
    })?;
    match step {
        UnixOutcomeStep::Reported {
            outcome,
            report_bytes,
        } => {
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
            Ok(Some(outcome))
        }
        UnixOutcomeStep::Pending | UnixOutcomeStep::Stdout(_) | UnixOutcomeStep::Stderr(_) => {
            Ok(None)
        }
    }
}

fn register_test(
    port: &NativePort,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    slot: SessionSlotId,
    reporting: Reporting<'_>,
) -> Result<(), RunError> {
    let child = ledger
        .sessions()
        .get(slot)
        .ok_or(RunError::Outcome)?
        .child();
    let process_id = child.process_id();
    track(process_id)?;
    let mut process = port.watch(process_id).map_err(|_| RunError::Reactor)?;
    let stdout_token = port
        .watch_descriptor(child.stdout().raw())
        .map_err(|_| RunError::Reactor)?;
    let stderr_token = port
        .watch_descriptor(child.stderr().raw())
        .map_err(|_| RunError::Reactor)?;
    let immediate_status = released(process.reap().map_err(|_| RunError::Reactor)?, process_id);
    watches[slot.index()] = Some(TestWatch {
        slot,
        process_id,
        stdout_token,
        stderr_token,
        process,
        output: ChildOutput::new(),
    });
    if let Some(status) = immediate_status {
        let outcome = deliver(
            ledger,
            HostEvent::Exit {
                slot,
                status: status.code.unwrap_or(1),
            },
            reporting,
            LibtestVerdict::Unreported,
        )?;
        if let Some(outcome) = outcome {
            if outcome == TerminalOutcome::Failed {
                watches[slot.index()]
                    .as_ref()
                    .ok_or(RunError::Outcome)?
                    .output
                    .write_failure()?;
            }
            watches[slot.index()] = None;
        }
    }
    Ok(())
}

fn test_event(
    port: &NativePort,
    event: NativeEvent,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    reporting: Reporting<'_>,
) -> Result<bool, RunError> {
    let token = event.token;
    for watch_slot in watches.iter_mut() {
        let Some(watch) = watch_slot.as_mut() else {
            continue;
        };
        let slot = watch.slot;
        if let Some(stdout) = watch.stream_for(token) {
            let descriptor = {
                let child = ledger
                    .sessions()
                    .get(slot)
                    .ok_or(RunError::Outcome)?
                    .child();
                if stdout {
                    child.stdout().raw()
                } else {
                    child.stderr().raw()
                }
            };
            let mut bytes = [0; SOURCE_CAPACITY];
            loop {
                let read =
                    unsafe { libc::read(descriptor, bytes.as_mut_ptr().cast(), bytes.len()) };
                if let Ok(length) = usize::try_from(read)
                    && length > 0
                {
                    let output = &mut watch.output;
                    if stdout {
                        output.observe_stdout(&bytes[..length]);
                    } else {
                        output.observe_stderr(&bytes[..length]);
                    }
                    let event = if stdout {
                        HostEvent::Stdout {
                            slot,
                            bytes: &bytes[..length],
                        }
                    } else {
                        HostEvent::Stderr {
                            slot,
                            bytes: &bytes[..length],
                        }
                    };
                    let outcome = deliver(ledger, event, reporting, output.verdict())?;
                    if let Some(outcome) = outcome {
                        if outcome == TerminalOutcome::Failed {
                            output.write_failure()?;
                        }
                        *watch_slot = None;
                        return Ok(true);
                    }
                    continue;
                }
                if read == 0 {
                    port.unwatch_descriptor(descriptor)
                        .map_err(|_| RunError::Reactor)?;
                    let event = if stdout {
                        HostEvent::StdoutEof { slot }
                    } else {
                        HostEvent::StderrEof { slot }
                    };
                    let outcome = deliver(ledger, event, reporting, watch.output.verdict())?;
                    if let Some(outcome) = outcome {
                        if outcome == TerminalOutcome::Failed {
                            watch.output.write_failure()?;
                        }
                        *watch_slot = None;
                    }
                    return Ok(true);
                }
                let code = last_error();
                if code == libc::EINTR {
                    continue;
                }
                if code == libc::EAGAIN || code == libc::EWOULDBLOCK {
                    return Ok(true);
                }
                return Err(RunError::Reactor);
            }
        }
        if token == watch.process.token() {
            observe_exit(&mut watch.process, event);
            let reaped = released(
                watch.process.reap().map_err(|_| RunError::Reactor)?,
                watch.process_id,
            );
            if let Some(status) = reaped {
                deliver_exit(ledger, watch_slot, status.code.unwrap_or(1), reporting)?;
            }
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn last_error() -> c_int {
    unsafe { *libc::__error() }
}
#[cfg(target_os = "linux")]
fn last_error() -> c_int {
    unsafe { *libc::__errno_location() }
}

fn override_entries<'storage>(
    layout: &TestEnvironmentLayout,
    environment: &'storage [u8],
) -> Result<([&'storage CStr; MAX_ENTRIES], usize), RunError> {
    let mut entries = [c""; MAX_ENTRIES];
    for (index, entry) in entries.iter_mut().enumerate().take(layout.len()) {
        *entry = layout
            .c_entry(environment, index)
            .ok_or(RunError::TestEnvironment)?;
    }
    Ok((entries, layout.len()))
}

fn spawn_list(
    executable: &CStr,
    ignored: bool,
    envp: &[*const c_char],
    directory: &CStr,
) -> Result<UnixChild, RunError> {
    let mut argv = [
        executable.as_ptr(),
        c"--list".as_ptr(),
        core::ptr::null(),
        core::ptr::null(),
    ];
    let argument_count = if ignored {
        argv[2] = c"--ignored".as_ptr();
        4
    } else {
        3
    };
    let child = spawn_raw_unix(executable, &argv[..argument_count], envp, Some(directory), false)
        .map_err(|_| RunError::Spawn)?;
    track(child.process_id())?;
    Ok(child)
}

fn spawn_listing(
    artifact: &TestArtifact<'_>,
    run: &ListingRun<'_>,
    linked_directories: &str,
    build_script: Option<BuildScriptOutput<'_>>,
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
    let mut environment = [0; ENVIRONMENT_CAPACITY];
    let layout = TestEnvironmentLayout::build(
        &TestEnvironmentInput {
            artifact: *artifact,
            linked_directories,
            library_directories: run.library_directories,
            inherited_dylib_path: run.inherited_dylib_path,
            home: run.home,
            build_script,
        },
        &mut environment,
    )
    .map_err(|_| RunError::TestEnvironment)?;
    let mut path = [0; PATH_CAPACITY];
    path[..executable.len()].copy_from_slice(executable.as_bytes());
    let executable_cstr =
        CStr::from_bytes_with_nul(&path[..=executable.len()]).map_err(|_| RunError::Arguments)?;
    let (overrides, override_count) = override_entries(&layout, &environment)?;
    let mut envp = [core::ptr::null(); ENV_CAPACITY + MAX_ENTRIES + 1];
    merge_environment(run.environment, &overrides[..override_count], &mut envp)
        .map_err(|_| RunError::Environment)?;
    let directory = layout.c_current_directory(&environment);
    let child = spawn_list(executable_cstr, false, &envp, directory)?;
    let ignored_child = if run.settings.ignore == IgnoreMode::Include {
        None
    } else {
        Some(spawn_list(executable_cstr, true, &envp, directory)?)
    };
    Ok(PendingListing {
        target,
        child,
        ignored_child,
        identity,
        path,
        path_length: executable.len(),
        environment,
        layout,
    })
}

fn launch_listed(
    run: &ListingRun<'_>,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    intent: ExactTestIntent<'_, '_, '_>,
    target: usize,
    environment: &[u8],
    layout: &TestEnvironmentLayout,
) -> Result<(), RunError> {
    let mut argument_bytes = [0; PATH_CAPACITY + NAME_CAPACITY + 64];
    let mut argument_pointers = [core::ptr::null(); 5];
    let mut environment_pointers = [core::ptr::null(); ENV_CAPACITY + MAX_ENTRIES + 1];
    let (overrides, override_count) = override_entries(layout, environment)?;
    if run.tripped(ledger) {
        run.policy.borrow_mut().launched();
    }
    let slot = UnixLaunchInput {
        target,
        owner: UnixLaunchOwner::Ledger(ledger),
        intent,
        inherited_environment: run.environment,
        child_overrides: &overrides[..override_count],
        harness_arguments: run.harness_arguments,
        current_directory: layout.c_current_directory(environment),
        argument_bytes: &mut argument_bytes,
        argument_pointers: &mut argument_pointers,
        environment_pointers: &mut environment_pointers,
    }
    .launch()
    .map_err(|error| {
        report_launch_failure(
            LaunchCause::from(&error),
            intent.test_name(),
            executable_text(intent.executable()),
        )
    })?;
    run.targets.borrow_mut().tally(target, Tally::Launched);
    register_test(run.port, ledger, watches, slot, run.reporting())
}

struct ListingRun<'run> {
    port: &'run NativePort,
    signals: &'run SignalPipe,
    filter: &'run str,
    exact: bool,
    targets: RefCell<TargetTable>,
    started_millis: u64,
    harness_arguments: &'run [&'run [u8]],
    environment: &'run [&'run CStr],
    library_directories: &'run [&'run str],
    inherited_dylib_path: Option<&'run str>,
    home: Option<&'run str>,
    settings: &'run Settings,
    prepared: Option<&'run Prepared<'run>>,
    docs: DocObservation,
    policy: RefCell<StopPolicy>,
    doc: RefCell<Option<DocRun<'run>>>,
}

#[derive(Default)]
struct DocObservation {
    passed: Cell<usize>,
    failed: Cell<usize>,
    ignored: Cell<usize>,
    blocks: Cell<usize>,
    empty_blocks: Cell<usize>,
}

impl ListingRun<'_> {
    fn interrupted(&self) -> bool {
        self.signals.received().is_some()
    }

    fn reporting(&self) -> Reporting<'_> {
        Reporting {
            color: self.settings.color,
            targets: &self.targets,
        }
    }

    fn tripped(
        &self,
        ledger: &OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> bool {
        let failures = ledger.snapshot().failed + self.docs.failed.get();
        let observation = self.policy.borrow().observe_failures(failures);
        if observation == Observation::FirstTrip {
            let doc = self.interrupt_doc_tests();
            self.policy
                .borrow_mut()
                .record_trip(ledger.sessions().len(), doc);
        }
        observation != Observation::Clear
    }

    fn interrupt_doc_tests(&self) -> DocAtTrip {
        self.doc
            .try_borrow_mut()
            .ok()
            .and_then(|mut doc| doc.as_mut().map(DocRun::state_for_trip))
            .unwrap_or(DocAtTrip::NotSpawned)
    }

    fn pump_doc_tests(&self) -> Result<(), RunError> {
        match self.doc.borrow_mut().as_mut() {
            Some(doc) => doc.pump(self.settings.color, &self.docs),
            None => Ok(()),
        }
    }

    fn observe_doc_event(&self, event: NativeEvent) {
        if let Some(doc) = self.doc.borrow_mut().as_mut() {
            doc.observe_exit_event(event);
        }
    }

    fn doc_tests_finished(&self) -> bool {
        self.doc.borrow().as_ref().is_none_or(DocRun::finished)
    }
}

enum Flow {
    Next,
    Pause,
    Eof,
}

// a full job set stops reading the listing, and a level-triggered readable fd left
// registered would return from every kevent and starve the zero-ready backstop.
#[derive(Clone, Copy)]
enum StdoutRegistration {
    Registered,
    Paused,
}

impl StdoutRegistration {
    fn sync(&mut self, stdout: &mut NativeReader<'_>, wanted: bool) -> Result<(), RunError> {
        match (*self, wanted) {
            (Self::Registered, false) => {
                stdout.pause().map_err(|_| RunError::Reactor)?;
                *self = Self::Paused;
            }
            (Self::Paused, true) => {
                stdout.resume().map_err(|_| RunError::Reactor)?;
                *self = Self::Registered;
            }
            _ => {}
        }
        Ok(())
    }
}

fn listing_wanted(
    run: &ListingRun<'_>,
    ledger: &OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
) -> bool {
    run.tripped(ledger) || ledger.sessions().len() < run.settings.jobs
}

struct ListingScan<'scan, 'run> {
    run: &'scan ListingRun<'run>,
    target: usize,
    environment: &'scan [u8],
    layout: &'scan TestEnvironmentLayout,
}

impl ListingScan<'_, '_> {
    fn apply<ReadError>(
        &self,
        step: Poll<Result<ListingSourceStep<'_, '_, '_>, ListingSourceError<ReadError>>>,
        stopped: bool,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    ) -> Result<Flow, RunError> {
        let run = self.run;
        let Poll::Ready(step) = step else {
            return Ok(Flow::Pause);
        };
        match step.map_err(|_| RunError::ListingStream)? {
            ListingSourceStep::Launch(_) if run.interrupted() => {}
            ListingSourceStep::Launch(_) if stopped => {
                run.policy.borrow_mut().listed_test_skipped();
            }
            ListingSourceStep::Launch(intent) => {
                launch_listed(run, ledger, watches, intent, self.target, self.environment, self.layout)?;
                if ledger.sessions().len() >= run.settings.jobs {
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
    line: &mut [u8; RESULT_LINE_CAPACITY],
    length: &mut usize,
    set: &mut IgnoredSet,
) -> Result<(), RunError> {
    for byte in bytes {
        if *byte != b'\n' {
            *line.get_mut(*length).ok_or(RunError::ListingStream)? = *byte;
            *length += 1;
            continue;
        }
        let parsed = parse_listing_line(&line[..*length]).map_err(|_| RunError::ListingStream)?;
        *length = 0;
        if let ListingLine::Test(name) = parsed {
            set.insert(name.as_bytes()).map_err(RunError::IgnoredSet)?;
        }
    }
    Ok(())
}

// the `--list --ignored` child is read to completion before the full listing launches anything
fn collect_ignored(
    run: &ListingRun<'_>,
    child: UnixChild,
    set: &mut IgnoredSet,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
) -> Result<(), RunError> {
    let process_id = child.process_id();
    let (mut stdout, mut stderr, mut process) = reader_pair(run.port, child)?;
    let mut line = [0; RESULT_LINE_CAPACITY];
    let mut line_length = 0;
    let mut bytes = [0; SOURCE_CAPACITY];
    let (mut stdout_eof, mut stderr_eof, mut exit_status) = (false, false, None);
    let mut events = [NativeEvent::default(); 64];
    let mut context = Context::from_waker(Waker::noop());
    reap_into(&mut process, process_id, &mut exit_status)?;
    loop {
        run.pump_doc_tests()?;
        while !stdout_eof {
            match Pin::new(&mut stdout).poll_read(&mut context, &mut bytes) {
                Poll::Ready(Ok(0)) => stdout_eof = true,
                Poll::Ready(Ok(count)) => {
                    feed_ignored_lines(&bytes[..count], &mut line, &mut line_length, set)?;
                }
                Poll::Ready(Err(_)) => return Err(RunError::Reactor),
                Poll::Pending => break,
            }
        }
        if !stderr_eof {
            stderr_eof = drain_reader(&mut stderr, 2, None)?;
        }
        reap_into(&mut process, process_id, &mut exit_status)?;
        if stdout_eof && stderr_eof && exit_status.is_some() {
            break;
        }
        let ready = wait_for_events(run.port, run.signals, &mut events)?;
        for event in events.iter().copied().take(ready) {
            run.observe_doc_event(event);
            if event.token == stdout.token() {
                stdout.wake_read_ready();
            } else if event.token == stderr.token() {
                stderr.wake_read_ready();
            } else if event.token == process.token() {
                observe_exit(&mut process, event);
                reap_into(&mut process, process_id, &mut exit_status)?;
            } else {
                test_event(run.port, event, ledger, watches, run.reporting())?;
            }
        }
        reap_unreported_children(run.port, ledger, watches, run.reporting())?;
    }
    if exit_status.is_some_and(|status| status.code != Some(0)) && !run.interrupted() {
        return Err(RunError::ListingFailed);
    }
    Ok(())
}

fn run_listing(
    run: &ListingRun<'_>,
    pending: PendingListing,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
) -> Result<(), RunError> {
    let executable = core::str::from_utf8(&pending.path[..pending.path_length])
        .map_err(|_| RunError::Arguments)?;
    let mut ignored_set = IgnoredSet::new();
    if let Some(ignored_child) = pending.ignored_child {
        collect_ignored(run, ignored_child, &mut ignored_set, ledger, watches)?;
    }
    let listing_id = pending.child.process_id();
    let listing_target = pending.target;
    let (mut stdout, mut stderr, mut process) = reader_pair(run.port, pending.child)?;
    let scan = ListingScan {
        run,
        target: listing_target,
        environment: &pending.environment,
        layout: &pending.layout,
    };
    let mut stdout_bytes = [0; SOURCE_CAPACITY];
    let mut frame_bytes = [0; SOURCE_CAPACITY + 1];
    let mut source = ListingSource::new(&mut stdout_bytes).map_err(|_| RunError::ListingStream)?;
    let mut framer = ArtifactFramer::new(&mut frame_bytes, SOURCE_CAPACITY)
        .map_err(|_| RunError::ListingStream)?;
    let selection = Selection {
        filter: run.filter,
        exact: run.exact,
        filterset: run.prepared,
        package: pending.identity.package(),
        binary: pending.identity.binary(),
        mode: run.settings.ignore,
        ignored: &ignored_set,
    };
    let filter = NameFilter::Selected(&selection);
    let mut stderr_eof = false;
    let mut stdout_eof = false;
    let mut exit_status = None;
    reap_into(&mut process, listing_id, &mut exit_status)?;
    let mut events = [NativeEvent::default(); 64];
    let mut context = Context::from_waker(Waker::noop());
    let mut stdout_registration = StdoutRegistration::Registered;
    while !stdout_eof || !stderr_eof || exit_status.is_none() {
        run.pump_doc_tests()?;
        if !stdout_eof && listing_wanted(run, ledger) {
            stdout_registration.sync(&mut stdout, true)?;
            loop {
                let stopped = run.tripped(ledger);
                let step = source.poll_next(
                    Pin::new(&mut stdout),
                    &mut framer,
                    ledger.sessions(),
                    ExecutablePath::Borrowed(executable),
                    filter,
                    &mut context,
                );
                match scan.apply(step, stopped, ledger, watches)? {
                    Flow::Next => {}
                    Flow::Pause => break,
                    Flow::Eof => {
                        stdout_eof = true;
                        break;
                    }
                }
            }
        }
        if !stderr_eof {
            stderr_eof = drain_reader(&mut stderr, 2, None)?;
        }
        reap_into(&mut process, listing_id, &mut exit_status)?;
        if stdout_eof && stderr_eof && exit_status.is_some() {
            break;
        }
        if !stdout_eof {
            stdout_registration.sync(&mut stdout, listing_wanted(run, ledger))?;
        }
        let ready = wait_for_events(run.port, run.signals, &mut events)?;
        for event in events.iter().copied().take(ready) {
            run.observe_doc_event(event);
            if event.token == stdout.token() {
                stdout.wake_read_ready();
            } else if event.token == stderr.token() {
                stderr.wake_read_ready();
            } else if event.token == process.token() {
                observe_exit(&mut process, event);
                reap_into(&mut process, listing_id, &mut exit_status)?;
            } else {
                test_event(run.port, event, ledger, watches, run.reporting())?;
            }
        }
        reap_unreported_children(run.port, ledger, watches, run.reporting())?;
    }
    if exit_status.is_some_and(|status| status.code != Some(0)) && !run.interrupted() {
        return Err(RunError::ListingFailed);
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
    envp: &[*const c_char],
    directory: Option<&ChildDirectory>,
) -> Result<UnixChild, RunError> {
    let mut argv = [core::ptr::null(); 64];
    let forwarded = parser.cargo_arguments();
    if forwarded.len() + 6 > argv.len() {
        return Err(RunError::Arguments);
    }
    argv[0] = c"cargo".as_ptr();
    argv[1] = c"test".as_ptr();
    argv[2] = c"--no-run".as_ptr();
    argv[3] = c"--message-format=json".as_ptr();
    let mut count = 4;
    if let Some(directory) = directory {
        argv[count] = directory.argument().as_ptr();
        count += 1;
    }
    for argument in forwarded {
        if argument.contains(&0) {
            return Err(RunError::Arguments);
        }
        argv[count] = argument.as_ptr().cast();
        count += 1;
    }
    spawn_raw_unix(c"cargo", &argv[..=count], envp, None, true).map_err(|_| RunError::Spawn)
}

const METADATA_VALUE_FLAGS: [&[u8]; 4] = [b"--manifest-path", b"-m", b"--config", b"-Z"];
const METADATA_BOOLEAN_FLAGS: [&[u8]; 3] = [b"--locked", b"--frozen", b"--offline"];

// the bin set comes from one `cargo metadata --no-deps`; a spawn failure just leaves build-finished as the signal
fn spawn_metadata(parser: &CliParser<'_>, envp: &[*const c_char]) -> Result<UnixChild, RunError> {
    let mut argv = [core::ptr::null(); 64];
    let mut count = 0;
    for fixed in [c"cargo", c"metadata", c"--no-deps", c"--format-version=1"] {
        argv[count] = fixed.as_ptr();
        count += 1;
    }
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
                if part.contains(&0) || count + 2 >= argv.len() {
                    return Err(RunError::Arguments);
                }
                argv[count] = part.as_ptr().cast();
                count += 1;
            }
            index = end;
        } else {
            index += 1;
        }
    }
    if !locked {
        argv[count] = c"--locked".as_ptr();
        count += 1;
    }
    spawn_raw_unix(c"cargo", &argv[..=count], envp, None, true).map_err(|_| RunError::Spawn)
}

// blocks until the short-lived metadata child finishes; none of the run's own children can be affected
fn read_metadata(child: UnixChild) -> Option<MetadataFacts> {
    let (process_id, stdout, stderr) = child.into_parts();
    let mut descriptors = [
        libc::pollfd {
            fd: stdout.raw(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr.raw(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let mut scan = MetadataScan::new();
    let mut bytes = [0; SOURCE_CAPACITY];
    let mut usable = true;
    while descriptors.iter().any(|descriptor| descriptor.fd >= 0) {
        // safety: `descriptors` is a live array of two pollfd for the call's duration.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
        if ready < 0 && last_error() == libc::EINTR {
            continue;
        }
        if ready < 0 {
            usable = false;
            break;
        }
        for (index, descriptor) in descriptors.iter_mut().enumerate() {
            if descriptor.fd < 0 || descriptor.revents == 0 {
                continue;
            }
            // safety: the descriptor is open and `bytes` is writable for its length.
            let read = unsafe { libc::read(descriptor.fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            match usize::try_from(read) {
                Ok(0) => descriptor.fd = -1,
                Ok(length) if index == 0 && usable => {
                    usable = scan.feed(&bytes[..length]).is_ok();
                }
                Ok(_) => {}
                Err(_) if matches!(last_error(), libc::EINTR | libc::EAGAIN) => {}
                Err(_) => {
                    descriptor.fd = -1;
                    usable = false;
                }
            }
        }
    }
    let mut status = 0;
    // safety: `status` is a valid out pointer and `process_id` is this function's own child.
    while unsafe { libc::waitpid(process_id, &raw mut status, 0) } < 0 && last_error() == libc::EINTR {}
    drop((stdout, stderr));
    (usable && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0).then(|| scan.finish())
}

fn pause_cargo(
    stdout: &mut NativeReader<'_>,
    stderr: &mut NativeReader<'_>,
    stdout_open: bool,
    stderr_open: bool,
) -> Result<(), RunError> {
    if stdout_open {
        stdout.pause().map_err(|_| RunError::Reactor)?;
    }
    if stderr_open {
        stderr.pause().map_err(|_| RunError::Reactor)?;
    }
    Ok(())
}

fn resume_cargo(
    stdout: &mut NativeReader<'_>,
    stderr: &mut NativeReader<'_>,
    stdout_open: bool,
    stderr_open: bool,
) -> Result<(), RunError> {
    if stdout_open {
        stdout.resume().map_err(|_| RunError::Reactor)?;
    }
    if stderr_open {
        stderr.resume().map_err(|_| RunError::Reactor)?;
    }
    Ok(())
}

struct CargoDriver<'run, 'buffer> {
    stdout: NativeReader<'run>,
    stderr: NativeReader<'run>,
    process: NativeWatch,
    process_id: libc::pid_t,
    source: CargoSource<'buffer>,
    framer: ArtifactFramer<'buffer>,
    scratch: &'buffer mut [u8],
    linked: LinkedDirectories<'buffer>,
    build_scripts: BuildScriptOutputs<'buffer>,
    stdout_eof: bool,
    stderr_eof: bool,
    exit_code: Option<i32>,
    pending_listing: Option<PendingListing>,
    bins: BinCounts,
    gate: BinGate,
    build: BuildState,
    dropped: usize,
    parked: ParkedArtifacts,
    lock_watch: LockWatch,
}

// a test or bench executable may use CARGO_BIN_EXE_<name>, so it waits for its package's bins
/// How a test or bench executable learns that its package's bins exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinGate {
    Off,
    Metadata,
    BuildFinished,
}

impl BinGate {
    const fn name(self) -> &'static [u8] {
        match self {
            Self::Off => b"off",
            Self::Metadata => b"metadata",
            Self::BuildFinished => b"build-finished",
        }
    }
}

/// What cargo said about the build so far; only a build that succeeded proves every bin exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildState {
    Running,
    Succeeded,
    Failed,
}

fn waits_for_bins(bins: &BinCounts, gate: BinGate, build: BuildState, package_id: &str) -> bool {
    match (build, gate) {
        (BuildState::Succeeded, _) | (_, BinGate::Off) => false,
        (_, BinGate::Metadata) => bins.awaiting(package_id),
        (_, BinGate::BuildFinished) => true,
    }
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
    let mut bytes = [0_u8; 1100];
    if path.len() >= bytes.len() {
        return false;
    }
    bytes[..path.len()].copy_from_slice(path.as_bytes());
    // safety: the zero fill after `path` terminates the string and `bytes` outlives the call.
    unsafe { libc::access(bytes.as_ptr().cast(), libc::F_OK) == 0 }
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

fn resolve_bin_gate(settings: &Settings, spawned: bool, facts: Option<MetadataFacts>) -> (BinCounts, BinGate) {
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
    ledger: &OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    artifact: &TestArtifact<'_>,
    linked: &LinkedDirectories<'_>,
    build_scripts: &BuildScriptOutputs<'_>,
) -> Result<Option<PendingListing>, RunError> {
    if run.interrupted() {
        return Ok(None);
    }
    if run.tripped(ledger) {
        run.policy.borrow_mut().target_skipped();
        return Ok(None);
    }
    let build_script = build_scripts.find(artifact.package_id);
    spawn_listing(artifact, run, linked.as_str(), build_script).map(Some)
}

impl CargoDriver<'_, '_> {
    fn reap(&mut self) -> Result<(), RunError> {
        let reaped = released(
            self.process.reap().map_err(|_| RunError::Reactor)?,
            self.process_id,
        );
        if let Some(status) = reaped {
            self.exit_code = Some(status.code.unwrap_or(1));
        }
        Ok(())
    }

    fn drained(&self, unaccounted: usize) -> bool {
        self.stdout_eof
            && self.stderr_eof
            && self.exit_code.is_some()
            && unaccounted == 0
            && self.parked.is_empty()
    }

    fn run_listing_paused(
        &mut self,
        run: &ListingRun<'_>,
        listing: PendingListing,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    ) -> Result<(), RunError> {
        let stdout_open = !self.stdout_eof;
        let stderr_open = !self.stderr_eof;
        pause_cargo(&mut self.stdout, &mut self.stderr, stdout_open, stderr_open)?;
        run_listing(run, listing, ledger, watches)?;
        resume_cargo(&mut self.stdout, &mut self.stderr, stdout_open, stderr_open)
    }

    fn release_parked(
        &mut self,
        run: &ListingRun<'_>,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
        reason: &[u8],
    ) -> Result<(), RunError> {
        loop {
            let (bins, gate, build) = (&self.bins, self.gate, self.build);
            let Some(owned) = self
                .parked
                .take_ready(|package_id| !waits_for_bins(bins, gate, build, package_id))
            else {
                return Ok(());
            };
            let artifact = owned.as_artifact();
            gate_debug(run.settings.debug, |line| {
                line.text(b" release reason=");
                line.text(reason);
                line.text(b" target=");
                line.text(artifact.target_name.as_bytes());
                line.text(b" package=");
                line.text(artifact.package_id.as_bytes());
            });
            let started =
                start_listing(run, ledger, &artifact, &self.linked, &self.build_scripts)?;
            if let Some(next_listing) = started
                && let Some(previous_listing) = self.pending_listing.replace(next_listing)
            {
                self.run_listing_paused(run, previous_listing, ledger, watches)?;
            }
        }
    }

    fn finish_build(
        &mut self,
        build: BuildState,
        reason: &[u8],
        run: &ListingRun<'_>,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    ) -> Result<(), RunError> {
        self.build = build;
        self.release_parked(run, ledger, watches, reason)?;
        while let Some(owned) = self.parked.take_ready(|_| true) {
            let artifact = owned.as_artifact();
            self.dropped += 1;
            gate_debug(run.settings.debug, |line| {
                line.text(b" drop reason=");
                line.text(reason);
                line.text(b" target=");
                line.text(artifact.target_name.as_bytes());
                line.text(b" package=");
                line.text(artifact.package_id.as_bytes());
            });
        }
        Ok(())
    }

    fn poll_artifacts(
        &mut self,
        context: &mut Context<'_>,
        run: &ListingRun<'_>,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    ) -> Result<(), RunError> {
        loop {
            let step = self.source.poll_next(
                Pin::new(&mut self.stdout),
                &mut self.framer,
                &mut *self.scratch,
                16,
                context,
            );
            match step {
                Poll::Ready(Ok(CargoSourceStep::Artifact(artifact))) => {
                    let gated = matches!(artifact.target_kind, "test" | "bench")
                        && waits_for_bins(
                            &self.bins,
                            self.gate,
                            self.build,
                            artifact.package_id,
                        );
                    debug_artifact(run.settings.debug, gated, self.gate, &artifact);
                    if gated {
                        self.parked.park(&artifact).map_err(RunError::Parked)?;
                        continue;
                    }
                    let started = start_listing(
                        run,
                        ledger,
                        &artifact,
                        &self.linked,
                        &self.build_scripts,
                    )?;
                    if let Some(next_listing) = started
                        && let Some(previous_listing) = self.pending_listing.replace(next_listing)
                    {
                        self.run_listing_paused(run, previous_listing, ledger, watches)?;
                    }
                }
                Poll::Ready(Ok(CargoSourceStep::BinBuilt {
                    package_id,
                    executable,
                })) => {
                    debug_bin_built(run.settings.debug, executable, package_id);
                    self.bins.note_built(package_id);
                    self.release_parked(run, ledger, watches, b"bin-built")?;
                }
                Poll::Ready(Ok(CargoSourceStep::BuildFinished { success })) => {
                    let (build, event, reason): (_, &[u8], &[u8]) = if success {
                        (BuildState::Succeeded, b" build-finished success=true", b"build-succeeded")
                    } else {
                        (BuildState::Failed, b" build-finished success=false", b"build-failed")
                    };
                    debug_build_end(run.settings.debug, event, self.parked.len());
                    self.finish_build(build, reason, run, ledger, watches)?;
                    self.finish_build(build, reason, run, ledger, watches)?;
                }
                Poll::Ready(Ok(CargoSourceStep::BuildScript(script))) => {
                    for reported in script.linked_paths.iter() {
                        self.linked
                            .add(reported)
                            .map_err(|_| RunError::TestEnvironment)?;
                    }
                    self.build_scripts
                        .add(&script)
                        .map_err(|_| RunError::TestEnvironment)?;
                }
                Poll::Ready(Ok(
                    CargoSourceStep::Read { .. }
                    | CargoSourceStep::NeedMore
                    | CargoSourceStep::Ignored,
                )) => {}
                Poll::Ready(Ok(CargoSourceStep::Eof)) => {
                    self.stdout_eof = true;
                    let build = match self.build {
                        BuildState::Running => BuildState::Failed,
                        settled => settled,
                    };
                    debug_build_end(run.settings.debug, b" stdout-eof", self.parked.len());
                    return self.finish_build(build, b"stdout-eof", run, ledger, watches);
                }
                Poll::Ready(Err(_)) => return Err(RunError::CargoStream),
                Poll::Pending => return Ok(()),
            }
        }
    }

    fn drive(
        &mut self,
        run: &ListingRun<'_>,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    ) -> Result<(), RunError> {
        let mut events = [NativeEvent::default(); 64];
        let mut context = Context::from_waker(Waker::noop());
        self.reap()?;
        while !self.drained(ledger.unaccounted()) || !run.doc_tests_finished() {
            run.pump_doc_tests()?;
            if !self.stdout_eof {
                self.poll_artifacts(&mut context, run, ledger, watches)?;
            }
            if let Some(listing) = self.pending_listing.take() {
                self.run_listing_paused(run, listing, ledger, watches)?;
            }
            if !self.stderr_eof {
                self.stderr_eof = drain_reader(&mut self.stderr, 2, Some(&mut self.lock_watch))?;
            }
            self.reap()?;
            run.pump_doc_tests()?;
            if self.drained(ledger.unaccounted()) && run.doc_tests_finished() {
                break;
            }
            let ready = wait_for_events(run.port, run.signals, &mut events)?;
            for event in events.iter().copied().take(ready) {
                run.observe_doc_event(event);
                self.dispatch_event(event, run, ledger, watches)?;
            }
            reap_unreported_children(run.port, ledger, watches, run.reporting())?;
        }
        Ok(())
    }

    fn dispatch_event(
        &mut self,
        event: NativeEvent,
        run: &ListingRun<'_>,
        ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
    ) -> Result<(), RunError> {
        let token = event.token;
        if token == self.stdout.token() {
            self.stdout.wake_read_ready();
        } else if token == self.stderr.token() {
            self.stderr.wake_read_ready();
        } else if token == self.process.token() {
            observe_exit(&mut self.process, event);
            self.reap()?;
        } else {
            test_event(run.port, event, ledger, watches, run.reporting())?;
        }
        Ok(())
    }
}

fn collect_libdirs(
    host_query: RustcLibdirQuery,
    target_query: Option<RustcLibdirQuery>,
    host_libdir: &mut [u8],
    target_libdir: &mut [u8],
) -> Result<(usize, usize), RunError> {
    let host_length = host_query
        .finish(host_libdir)
        .map_err(|_| RunError::RustcLibdir)?;
    let target_length = target_query
        .map(|query| query.finish(target_libdir))
        .transpose()
        .map_err(|_| RunError::RustcLibdir)?
        .unwrap_or(0);
    Ok((host_length, target_length))
}

/// Drives Cargo's artifact source and child process events from one native reactor.
///
/// # Errors
///
/// Returns [`RunError`] naming the stage that failed: arguments, environment, spawn, reactor, stream parsing, launch, or outcome accounting.
pub fn run_unix(parser: &CliParser<'_>) -> Result<RunSummary, RunError> {
    let started_millis = monotonic_millis();
    let harness = validated_harness(parser)?;
    let environment = unsafe { UnixEnvironment::current() };
    let settings = runtime_settings(&environment, &harness, parser)?;
    let mut inherited = [c""; ENV_CAPACITY];
    let inherited_count = environment
        .collect(&mut inherited)
        .map_err(|_| RunError::Environment)?;
    let inherited = &inherited[..inherited_count];
    let mut envp = [core::ptr::null(); ENV_CAPACITY + 1];
    let envp_count = environment_pointers(inherited, &mut envp)?;
    let envp = &envp[..envp_count];
    let configured_target = utf8_variable(&environment, c"CARGO_VAL_TARGET_DIR")?;
    let target_pairs = [("CARGO_VAL_TARGET_DIR", configured_target.unwrap_or(""))];
    let target_source = BorrowedSettings::new(&target_pairs[..usize::from(configured_target.is_some())]);
    let target_mode = resolve_target_dir(&target_source, parser)?;
    if parser.doc_selection() == DocSelection::Only {
        return run_doc_only(parser, &settings, target_mode, started_millis, envp);
    }
    let queries = start_libdir_queries(&environment, parser, envp)?;
    let inherited_dylib_path = utf8_variable(&environment, DYLIB_PATH_KEY_C)?;
    let home = utf8_variable(&environment, c"HOME")?;
    let filter = parser
        .filter()
        .map(core::str::from_utf8)
        .transpose()
        .map_err(|_| RunError::Arguments)?
        .unwrap_or("");
    let port = NativePort::new().map_err(|_| RunError::Reactor)?;
    let signals = install_signals(&port)?;
    let started = start_children(parser, &settings, target_mode, &port, envp)?;
    let cargo_child = started.cargo;
    let cargo_process_id = cargo_child.process_id();
    let doc_tests = started.doc_tests;
    let (bins, gate, lock_watch) = (started.bins, started.gate, started.lock_watch);
    let (stdout, stderr, process) = reader_pair(&port, cargo_child)?;
    let mut source_bytes = [0; SOURCE_CAPACITY];
    let mut frame_bytes = [0; FRAME_CAPACITY];
    let mut scratch = [0; SCRATCH_CAPACITY];
    let mut linked_bytes = [0; LINKED_CAPACITY];
    let mut build_script_bytes = [0; BUILD_SCRIPT_CAPACITY];
    let mut driver = CargoDriver {
        stdout,
        stderr,
        process,
        process_id: cargo_process_id,
        source: CargoSource::new(&mut source_bytes).map_err(|_| RunError::CargoStream)?,
        framer: ArtifactFramer::new(&mut frame_bytes, FRAME_CAPACITY - 1)
            .map_err(|_| RunError::CargoStream)?,
        scratch: &mut scratch,
        linked: LinkedDirectories::new(&mut linked_bytes),
        build_scripts: BuildScriptOutputs::new(&mut build_script_bytes),
        stdout_eof: false,
        stderr_eof: false,
        exit_code: None,
        pending_listing: None,
        bins,
        gate,
        build: BuildState::Running,
        dropped: 0,
        parked: ParkedArtifacts::new(),
        lock_watch,
    };
    let mut sessions = SessionSlots::<RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>::new();
    let mut ledger = OutcomeLedger::new(&mut sessions).map_err(|_| RunError::Outcome)?;
    let mut watches: [Option<TestWatch>; LIVE_CAPACITY] = [const { None }; LIVE_CAPACITY];
    let mut libdir_bytes = [0; LIBDIR_CAPACITY * 2];
    let (library_directories, directory_count) = resolve_libdirs(queries, &mut libdir_bytes)
        .inspect_err(|_| unsafe {
            libc::kill(cargo_process_id, libc::SIGKILL);
        })?;
    let prepared = settings.filterset.as_ref().map(Filterset::prepare);
    let run = ListingRun {
        port: &port,
        signals: &signals,
        filter,
        exact: parser.exact(),
        targets: RefCell::new(TargetTable::new()),
        started_millis,
        harness_arguments: match settings.ignore {
            IgnoreMode::Skip => &[],
            IgnoreMode::Only => &IGNORED_ARGUMENTS,
            IgnoreMode::Include => &INCLUDE_ARGUMENTS,
        },
        environment: inherited,
        library_directories: &library_directories[..directory_count],
        inherited_dylib_path,
        home,
        settings: &settings,
        prepared: prepared.as_ref(),
        docs: DocObservation::default(),
        policy: RefCell::new(StopPolicy::new(settings.max_fail)),
        doc: RefCell::new(doc_tests),
    };
    driver.drive(&run, &mut ledger, &mut watches)?;
    if driver.dropped > 0 && driver.exit_code == Some(0) {
        return Err(RunError::CargoStream);
    }
    finish_run(parser, &run, driver.exit_code)
}

struct StartedChildren<'port> {
    cargo: UnixChild,
    doc_tests: Option<DocRun<'port>>,
    bins: BinCounts,
    gate: BinGate,
    lock_watch: LockWatch,
}

// isolated mode reads the metadata first because the build directories come from it; the other modes overlap the read with the builds
fn start_children<'port>(
    parser: &CliParser<'_>,
    settings: &Settings,
    target_mode: TargetDirMode<'_>,
    port: &'port NativePort,
    envp: &[*const c_char],
) -> Result<StartedChildren<'port>, RunError> {
    let isolating = target_mode == TargetDirMode::Isolated;
    let mut metadata_child = (settings.bin_gate || isolating)
        .then(|| spawn_metadata(parser, envp))
        .and_then(Result::ok);
    let spawned = metadata_child.is_some();
    let mut facts = None;
    if isolating {
        facts = metadata_child.take().and_then(read_metadata);
    }
    let directories = build_directories(target_mode, facts.as_ref())?;
    let cargo = spawn_cargo(parser, envp, directories.as_ref().map(|chosen| &chosen.test))?;
    track(cargo.process_id())?;
    let mut doc_tests = match parser.doc_selection() {
        DocSelection::Excluded | DocSelection::Only => None,
        DocSelection::Included => Some(spawn_doc_tests(
            port,
            parser,
            settings.ignore,
            envp,
            directories.as_ref().map(|chosen| &chosen.doc),
        )?),
    };
    if let Some(child) = metadata_child {
        facts = read_metadata(child);
    }
    let mut lock_watch = LockWatch::new();
    match (&directories, facts.as_ref().and_then(MetadataFacts::target_directory)) {
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

type LibdirQueries = (RustcLibdirQuery, Option<RustcLibdirQuery>);

fn start_libdir_queries(
    environment: &UnixEnvironment<'_>,
    parser: &CliParser<'_>,
    envp: &[*const c_char],
) -> Result<LibdirQueries, RunError> {
    let host_query = RustcLibdirQuery::start(environment.get(c"RUSTC"), None, envp)
        .map_err(|_| RunError::RustcLibdir)?;
    let target_query = target_triple(parser.cargo_arguments())
        .map(|triple| RustcLibdirQuery::start(environment.get(c"RUSTC"), Some(triple), envp))
        .transpose()
        .map_err(|_| RunError::RustcLibdir)?;
    Ok((host_query, target_query))
}

fn utf8_variable<'environment>(
    environment: &UnixEnvironment<'environment>,
    key: &CStr,
) -> Result<Option<&'environment str>, RunError> {
    environment
        .get(key)
        .map(core::str::from_utf8)
        .transpose()
        .map_err(|_| RunError::Environment)
}

fn resolve_libdirs(
    queries: LibdirQueries,
    buffer: &mut [u8; LIBDIR_CAPACITY * 2],
) -> Result<([&str; 2], usize), RunError> {
    let (host_buffer, target_buffer) = buffer.split_at_mut(LIBDIR_CAPACITY);
    let (host_length, target_length) =
        collect_libdirs(queries.0, queries.1, host_buffer, target_buffer)?;
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
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // safety: `now` is a valid timespec for the duration of the call.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    if status != 0 {
        return 0;
    }
    u64::try_from(now.tv_sec).unwrap_or(0) * 1000 + u64::try_from(now.tv_nsec / 1_000_000).unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn observe_exit(watch: &mut NativeWatch, event: NativeEvent) {
    watch.observe(&event);
}

#[cfg(target_os = "linux")]
fn observe_exit(_watch: &mut NativeWatch, _event: NativeEvent) {}

#[cfg(target_os = "macos")]
fn wait_ready(port: &NativePort, events: &mut [NativeEvent]) -> Result<usize, c_int> {
    port.wait_for(events, 100)
}

#[cfg(target_os = "linux")]
fn wait_ready(port: &NativePort, events: &mut [NativeEvent]) -> Result<usize, c_int> {
    port.wait(events)
}

// the signal event is consumed here so every loop that waits also forwards, and none sees its token
fn wait_for_events(
    port: &NativePort,
    signals: &SignalPipe,
    events: &mut [NativeEvent],
) -> Result<usize, RunError> {
    let ready = match wait_ready(port, events) {
        Ok(ready) => ready,
        Err(libc::EINTR) => 0,
        Err(_) => return Err(RunError::Reactor),
    };
    let mut kept = 0;
    for index in 0..ready {
        if events[index].token == signals.token() {
            signals.drain().map_err(|_| RunError::Reactor)?;
        } else {
            events[kept] = events[index];
            kept += 1;
        }
    }
    Ok(kept)
}

/// Fixed-capacity text assembled without allocation; text past the capacity is cut.
pub struct Line<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    length: usize,
}

type LineBuffer = Line<512>;

impl<const CAPACITY: usize> Line<CAPACITY> {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            length: 0,
        }
    }

    fn text(&mut self, text: &[u8]) {
        let room = self.bytes.len() - self.length;
        let taken = text.len().min(room);
        self.bytes[self.length..self.length + taken].copy_from_slice(&text[..taken]);
        self.length += taken;
    }

    fn number(&mut self, value: usize) {
        let mut digits = [0; 20];
        let mut start = digits.len();
        let mut remaining = value;
        loop {
            start -= 1;
            digits[start] = DECIMAL_DIGITS[remaining % 10];
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        self.text(&digits[start..]);
    }

    /// The assembled bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }

    fn write(&self, descriptor: c_int) -> Result<(), RunError> {
        write_fd(descriptor, &self.bytes[..self.length])
    }
}

fn write_stop_trailer(report: &StopReport) -> Result<(), RunError> {
    let mut line = LineBuffer::new();
    line.text(b"cargo-val: stopped at --max-fail ");
    line.number(report.max_fail);
    line.text(b"; in flight at trip ");
    line.number(report.in_flight_at_trip);
    line.text(b" (finished normally); launched after trip ");
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

const DOC_ARGUMENT_CAPACITY: usize = 72;
const DOC_STDERR_CAPACITY: usize = 16384;
const DOC_NICE: c_int = 10;
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

struct DocRun<'port> {
    stdout: NativeReader<'port>,
    stderr: NativeReader<'port>,
    process: NativeWatch,
    process_id: libc::pid_t,
    parser: DocParser,
    stdout_eof: bool,
    stderr_eof: bool,
    exit_code: Option<i32>,
    phase: DocPhase,
    stderr_bytes: [u8; DOC_STDERR_CAPACITY],
    stderr_length: usize,
    lock_watch: LockWatch,
}

fn spawn_doc_tests<'port>(
    port: &'port NativePort,
    parser: &CliParser<'_>,
    ignore: IgnoreMode,
    envp: &[*const c_char],
    directory: Option<&ChildDirectory>,
) -> Result<DocRun<'port>, RunError> {
    let mut argv = [core::ptr::null(); DOC_ARGUMENT_CAPACITY];
    let mut count = 0;
    let mut push = |argument: *const c_char| -> Result<(), RunError> {
        if count + 1 >= DOC_ARGUMENT_CAPACITY {
            return Err(RunError::Arguments);
        }
        argv[count] = argument;
        count += 1;
        Ok(())
    };
    push(c"cargo".as_ptr())?;
    push(c"test".as_ptr())?;
    push(c"--doc".as_ptr())?;
    if let Some(directory) = directory {
        push(directory.argument().as_ptr())?;
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
        push(argument.as_ptr().cast())?;
    }
    if parser.exact() || ignore != IgnoreMode::Skip {
        push(c"--".as_ptr())?;
    }
    if parser.exact() {
        push(c"--exact".as_ptr())?;
    }
    match ignore {
        IgnoreMode::Skip => {}
        IgnoreMode::Only => push(c"--ignored".as_ptr())?,
        IgnoreMode::Include => push(c"--include-ignored".as_ptr())?,
    }
    let child = spawn_raw_unix_in_new_group(c"cargo", &argv[..=count], envp, None, true)
        .map_err(|_| RunError::Spawn)?;
    let process_id = child.process_id();
    track(-process_id)?;
    lower_priority(process_id)?;
    let (stdout, stderr, process) = reader_pair(port, child)?;
    let mut lock_watch = LockWatch::new();
    if let Some(directory) = directory {
        lock_watch.set_directory(directory.directory());
    }
    Ok(DocRun {
        stdout,
        stderr,
        process,
        process_id,
        parser: DocParser::new(),
        stdout_eof: false,
        stderr_eof: false,
        exit_code: None,
        phase: DocPhase::Starting,
        stderr_bytes: [0; DOC_STDERR_CAPACITY],
        stderr_length: 0,
        lock_watch,
    })
}

fn lower_priority(process_id: libc::pid_t) -> Result<(), RunError> {
    let who = libc::id_t::try_from(process_id).map_err(|_| RunError::Spawn)?;
    let status = unsafe { libc::setpriority(libc::PRIO_PGRP, who, DOC_NICE) };
    if status == 0 {
        return Ok(());
    }
    write_fd(2, b"cargo-val: could not lower the doc test priority\n")
}

impl DocRun<'_> {
    fn finished(&self) -> bool {
        self.stdout_eof && self.stderr_eof && self.exit_code.is_some()
    }

    fn pump(&mut self, color: bool, observation: &DocObservation) -> Result<(), RunError> {
        let mut context = Context::from_waker(Waker::noop());
        let mut bytes = [0; SOURCE_CAPACITY];
        while !self.stdout_eof {
            match Pin::new(&mut self.stdout).poll_read(&mut context, &mut bytes) {
                Poll::Ready(Ok(0)) => self.stdout_eof = true,
                Poll::Ready(Ok(length)) => {
                    let phase = &mut self.phase;
                    self.parser.feed(&bytes[..length], |event| {
                        record_doc_event(event, phase, color, observation)
                    })?;
                }
                Poll::Ready(Err(_)) => return Err(RunError::Reactor),
                Poll::Pending => break,
            }
        }
        while !self.stderr_eof {
            match Pin::new(&mut self.stderr).poll_read(&mut context, &mut bytes) {
                Poll::Ready(Ok(0)) => self.stderr_eof = true,
                Poll::Ready(Ok(length)) => {
                    self.keep_stderr(&bytes[..length]);
                    announce_lock_wait(&mut self.lock_watch, &bytes[..length])?;
                }
                Poll::Ready(Err(_)) => return Err(RunError::Reactor),
                Poll::Pending => break,
            }
        }
        if self.exit_code.is_none()
            && let Some(status) = released(
                self.process.reap().map_err(|_| RunError::Reactor)?,
                -self.process_id,
            )
        {
            self.exit_code = Some(status.code.unwrap_or(1));
        }
        Ok(())
    }

    fn keep_stderr(&mut self, bytes: &[u8]) {
        let room = DOC_STDERR_CAPACITY - self.stderr_length;
        let taken = bytes.len().min(room);
        self.stderr_bytes[self.stderr_length..self.stderr_length + taken]
            .copy_from_slice(&bytes[..taken]);
        self.stderr_length += taken;
    }

    fn observe_exit_event(&mut self, event: NativeEvent) {
        if event.token == self.process.token() {
            observe_exit(&mut self.process, event);
        }
    }

    fn state_for_trip(&mut self) -> DocAtTrip {
        if self.exit_code.is_some() {
            DocAtTrip::AlreadyFinished
        } else if self.phase == DocPhase::Executing {
            DocAtTrip::ExecutingLeftToFinish
        } else {
            unsafe { libc::killpg(self.process_id, libc::SIGINT) };
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
        DocEvent::Result { name, outcome } => report_doc_result(name, outcome, color, observation),
        DocEvent::FailureLine(line) => {
            write_fd(2, line)?;
            write_fd(2, b"\n")
        }
        DocEvent::Finished => Ok(()),
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
    doc: &DocRun<'_>,
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
        write_stop_trailer(&report)?;
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

fn run_doc_only(
    parser: &CliParser<'_>,
    settings: &Settings,
    target_mode: TargetDirMode<'_>,
    started_millis: u64,
    envp: &[*const c_char],
) -> Result<RunSummary, RunError> {
    let port = NativePort::new().map_err(|_| RunError::Reactor)?;
    let signals = install_signals(&port)?;
    let facts = (!matches!(target_mode, TargetDirMode::Path(_)))
        .then(|| spawn_metadata(parser, envp))
        .and_then(Result::ok)
        .and_then(read_metadata);
    let directories = build_directories(target_mode, facts.as_ref())?;
    let mut doc = spawn_doc_tests(
        &port,
        parser,
        settings.ignore,
        envp,
        directories.as_ref().map(|chosen| &chosen.doc),
    )?;
    if directories.is_none()
        && let Some(shared) = facts.as_ref().and_then(MetadataFacts::target_directory)
    {
        doc.lock_watch.set_directory(shared);
    }
    let observation = DocObservation::default();
    let mut events = [NativeEvent::default(); 64];
    loop {
        doc.pump(settings.color, &observation)?;
        if doc.finished() {
            break;
        }
        let ready = wait_for_events(&port, &signals, &mut events)?;
        for event in events.iter().copied().take(ready) {
            doc.observe_exit_event(event);
        }
    }
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

fn install_signals(port: &NativePort) -> Result<SignalPipe, RunError> {
    SignalPipe::install(|descriptor| port.watch_descriptor(descriptor))
        .map_err(|_| RunError::Reactor)
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
    use crate::unix_arguments::UnixArgumentsError;

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
                LaunchCause::Arguments(UnixArgumentsError::InteriorNul),
                "cargo-val: exact test launch failed: argument lowering failed: an argument contains a NUL byte\n",
            ),
            (
                LaunchCause::Arguments(UnixArgumentsError::EnvironmentPointersTooSmall {
                    required: 130,
                    available: 128,
                }),
                "cargo-val: exact test launch failed: argument lowering failed: environment pointers need 130 and 128 are available\n",
            ),
            (
                LaunchCause::Spawn {
                    errno: libc::ENOENT,
                },
                "cargo-val: exact test launch failed: posix_spawn failed with errno 2\n",
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
        assert_eq!(checked, 8);
    }

    #[test]
    fn longest_launch_failure_line_fits_the_error_line() {
        let longest = LaunchCause::Arguments(UnixArgumentsError::EnvironmentPointersTooSmall {
            required: usize::MAX,
            available: usize::MAX,
        });
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

    #[test]
    fn refusal_without_detail_renders_its_static_message() {
        let line = RunError::Reactor.render();
        assert_eq!(line.bytes(), RunError::Reactor.message());
    }
}
