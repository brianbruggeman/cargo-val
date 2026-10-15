//! One poll-driven owner of Cargo discovery, libtest listings, and child outcomes.

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::ops::Range;
use core::task::{Context, Poll, Waker};

use conflaguration::{BorrowedSettings, SettingsSource, resolve_color};

use crate::ExecutablePath;
use crate::build_defaults::SIGNAL_SLOTS;
use crate::artifact_stream::{ArtifactFramer, RecordStep};
use crate::cargo_message::{BuildScript, TestArtifact};
use crate::cargo_source::{CargoSource, CargoSourceStep};
use crate::cli::{CliParser, DocSelection};
use crate::config::{
    FILTERSET, FailFastError, FailFastSources, HarnessFault, HarnessRequest, IgnoreMode, NO_TESTS, NoTestsPolicy,
    EXIT_WITH_PARENT, ExitWithParent, OUTPUT, TEST_TIMEOUT, TestTimeout, exit_with_parent_unsupported, OutputMode, OutputSetting, PROCESS, ProcessMode, ProcessSetting, SettingError, TARGET_DIR, TargetDirMode, ThreadSources, ThreadsError,
    ThreadsFault, ThreadsSource,
};
use crate::discover_listing::{
    DocEvent, DocOutcome, DocParser, ListingLine, ListingSource, ListingSourceStep,
    parse_listing_line,
};
use crate::discover_cargo::{BinGate, DiscoveryFsm};
use crate::filterset::{FilterError, Filterset, Prepared};
use crate::ignored_set::{IgnoredSet, IgnoredSetFull};
use crate::edge_store;
use crate::edge_stream::ChildStream;
use crate::last_run;
use crate::isolated_target::{BuildDirectories, ChildDirectory, LockKind, LockWatch};
use crate::line::Line;
use crate::failure_class::{Exit, Location};
use crate::hyperlink::{editor_template, links_enabled, site_in, write_link};
use crate::source_excerpt::{colour_allowed, for_failure, for_span, splice_excerpt};
use crate::render::review::{Action, Key, Rerun as Standing};
use crate::render::{Event, Mode, Outcome, STDERR_STREAM, STDOUT_STREAM, State, Stream, located, stream_close, stream_open, stream_page};
use crate::view_defaults::{FAILURES, FRAME_BYTES, RERUN_BYTES};
use crate::run::{
    CARGO_LANE, DOC_LANE, EDITOR_LANE, JOB_BASE, KEY_LANE, LISTING_LANE, Lane, Lanes, attach, attach_session, collect_child, collect_libdir,
    observe_cargo, observe_doc,
};
use crate::metadata_scan::{BinCounts, MetadataFacts, MetadataScan};
use prime::os::readiness::Readiness;
use proxima_core::signal::Signal;
use proxima_primitives::pipe::SignalSource;
use std::boxed::Box;
use std::collections::{HashSet, VecDeque};
use std::format;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::string::String;
use std::io;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::time::Instant;
use std::vec::Vec;
use crate::parked_artifacts::{OwnedArtifact, ParkFault, ParkedArtifacts};
use crate::status_queue::{self, EVENT_RESERVE};
use crate::summary::{Counts, Tally, TargetTable};
use crate::edge_signal::{
    SignalPipe, conventional_status, interrupt_group, release_child, release_group, track_child, track_group, track_group_hangup, abandon_slot, reserve_slot, TABLE_FULL,
};
use crate::edge_args::{DYLIB_PATH_KEY, Environment};
use crate::edge_spawn::{
    ArgumentsError, Child, HeldLaunchStorage, LaunchStorage, READY_AHEAD, SpawnError, held_launch_storage, resume_ready, spawn,
    spawn_in_group, spawn_listed,
};
use crate::host_event::HostEvent;
use crate::outcomes::{OutcomeError, OutcomeLedger, TerminalOutcome};
use crate::runner::{
    LaunchCause, LibtestVerdict, RunningChild, LaunchInput, LaunchOwner,
    ChildOutcomeError, OutcomeInput, OutcomeStep,
};
use crate::schedule::{SessionSlotId, SessionSlots};
use crate::sched_binary::{GROUP_CAPACITY, RECORD_BYTES};
use crate::shard_arena::{DECLARED_BYTES, Rerun, ShardArena};
use crate::shard_sched::{SchedTable, SpecialPlan};

mod shard;
pub(crate) mod window;
use shard::ShardWatch;
use window::{PAGE_BYTES, SYNC_WINDOW_BYTES, WINDOW_BYTES};
use crate::selection::{NameFilter, Selection};
use crate::stop::{self, DocAtTrip, StopPolicy, StopReport};
use crate::test_environment::{
    BuildScriptOutput, BuildScriptOutputs, LinkedDirectories, MAX_ENTRIES,
    TestEnvironmentInput, TestEnvironmentLayout,
};
use crate::test_intent::ExactTestIntent;

pub(crate) const LIVE_CAPACITY: usize = 64;
const _: () = assert!(LIVE_CAPACITY < SIGNAL_SLOTS, "jobs are capped at LIVE_CAPACITY, so the signal table needs one more slot for the doc test group");
const IGNORED_ARGUMENTS: [&[u8]; 1] = [b"--ignored"];
const INCLUDE_ARGUMENTS: [&[u8]; 1] = [b"--include-ignored"];
const SHARD_ARGUMENTS: [&[u8]; 1] = [b"--test-threads=1"];
const SHARD_ONLY_ARGUMENTS: [&[u8]; 2] = [b"--ignored", b"--test-threads=1"];
const SHARD_INCLUDE_ARGUMENTS: [&[u8]; 2] = [b"--include-ignored", b"--test-threads=1"];
const IDENTITY_CAPACITY: usize = 128;
pub(crate) const NAME_CAPACITY: usize = 1024;
const ENV_CAPACITY: usize = 1024;
const PATH_CAPACITY: usize = 4096;
const SCRATCH_CAPACITY: usize = 131_072;
const DIAGNOSTIC_CAPACITY: usize = 1_048_576;
const BUILD_FAILURE_PREFIX: &[u8] = b"error: could not compile";
const BUILD_WAITING_PREFIX: &[u8] = b"warning: build failed, waiting for other jobs to finish";
const LINKED_CAPACITY: usize = 32768;
const BUILD_SCRIPT_CAPACITY: usize = 65536;
const ENVIRONMENT_CAPACITY: usize = LINKED_CAPACITY + 16384;
const LISTING_SLOTS: usize = 8;
const SIGNAL_HOLDERS: usize = 3;
const LIBDIR_CAPACITY: usize = 1024;
const SOURCE_CAPACITY: usize = 8192;
const FRAME_CAPACITY: usize = 131_072;
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
    Timeline,
    Setting(SettingError),
    Filterset(FilterError),
    IgnoredSet(IgnoredSetFull),
    NothingRan,
    SummaryMismatch,
    TargetTable,
    Parked(ParkFault),
    TestThreads(ThreadsError),
    FailFast(FailFastError),
    DocTests,
    Spawn,
    Reactor,
    StatusQueue,
    SignalSlots,
    CargoStream,
    ListingStream,
    Launch(LaunchCause),
    Outcome,
    CargoFailed,
    CargoRefused,
    Interrupted(i32),
    Cancelled,
    Tripped,
    ListingFailed,
    DeclaredCapacity,
}

impl RunError {
    /// 2 for every refusal including an invocation cargo rejected, `BUILD_FAILED_EXIT_CODE` when the build failed, `128 + signal` when a forwarded SIGINT or SIGTERM ended the run.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Interrupted(signal) => conventional_status(signal),
            Self::CargoFailed => BUILD_FAILED_EXIT_CODE,
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
            Self::Timeline => b"cargo-val: --timeline is a terminal view; use --output plain or rich\n",
            Self::Setting(error) => error.message(),
            Self::IgnoredSet(_) => b"cargo-val: a test binary lists more ignored tests than the fixed set holds\n",
            Self::NothingRan => b"cargo-val: an exact test launch ran no test body\n",
            Self::SummaryMismatch => b"cargo-val: test counts do not balance\n",
            Self::Parked(_) => b"cargo-val: too many test executables are waiting for their package bin targets, or a path is too long to hold\n",
            Self::TargetTable => b"cargo-val: more test binaries than the fixed table or a binary id is too long\n",
            Self::TestThreads(error) => error.message(),
            Self::FailFast(error) => error.message(),
            Self::DocTests => b"cargo-val: cargo test --doc failed before reporting results\n",
            Self::Spawn => b"cargo-val: child spawn failed\n",
            Self::Reactor => b"cargo-val: reactor operation failed\n",
            Self::StatusQueue => b"cargo-val: status queue refused a record: a writer appended without room\n",
            Self::SignalSlots => b"cargo-val: the signal table has no free slot; raise CARGO_VAL_BUILD_SIGNAL_SLOTS and rebuild\n",
            Self::CargoStream => b"cargo-val: cargo output could not be parsed\n",
            Self::ListingStream => b"cargo-val: libtest listing could not be parsed\n",
            Self::Launch(_) => b"cargo-val: exact test launch failed\n",
            Self::Outcome => b"cargo-val: child completion failed\n",
            Self::CargoRefused => b"cargo-val: cargo refused the invocation before building; its error is above\n",
            Self::ListingFailed => b"cargo-val: libtest listing failed\n",
            Self::Cancelled => b"cargo-val: a session was cancelled by its scope\n",
            Self::Tripped => b"cargo-val: a launch was refused after the --max-fail trip\n",
            Self::DeclaredCapacity => {
                b"cargo-val: declared test scheduling records exceed the build's capacity; raise CARGO_VAL_BUILD_SCHED_RECORD_BYTES\n"
            }
            Self::CargoFailed | Self::Filterset(_) | Self::Interrupted(_) => b"",
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
            Self::SignalSlots => {
                line.text(b"cargo-val: the signal table holds ");
                line.number(SIGNAL_SLOTS);
                line.text(b" children and groups and has no free slot; raise CARGO_VAL_BUILD_SIGNAL_SLOTS and rebuild\n");
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
    /// Tests discovered across every binary, the count the last-run record keeps.
    pub discovered: usize,
}

/// Exit status when the selection ran no tests and the policy is `fail`.
pub const NO_TESTS_EXIT_CODE: i32 = 4;
pub const BUILD_FAILED_EXIT_CODE: i32 = 3;
const CARGO_FAILED_LINE: &[u8] = b"cargo-val: cargo test build failed\n";

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
    scope: Signal,
    output: ChildOutput,
    streams: Option<(Rc<ChildStream>, Rc<ChildStream>)>,
    shard: Option<ShardWatch>,
    group: [u8; GROUP_CAPACITY],
    group_length: usize,
    started_ms: u64,
    status: Option<i32>,
    rerun: Option<u64>,
}

impl TestWatch {
    // a session its scope ended has no verdict of its own, so its exit status is replaced by the cancellation
    fn exit(&self, code: i32) -> Result<i32, RunError> {
        if self.scope.is_fired() { Err(RunError::Cancelled) } else { Ok(code) }
    }

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

struct ChildOutput {
    result_line: [u8; RESULT_LINE_CAPACITY],
    result_line_length: usize,
    result_line_overflow: bool,
    result: Option<(usize, usize, usize)>,
}

impl ChildOutput {
    const fn new() -> Self {
        Self {
            result_line: [0; RESULT_LINE_CAPACITY],
            result_line_length: 0,
            result_line_overflow: false,
            result: None,
        }
    }

    fn observe_stdout(&mut self, bytes: &[u8]) {
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
const STDOUT: i32 = 1;

// panic paths are relative to the directory cargo ran rustc in, which is the manifest's workspace; no manifest means the current directory
fn manifest_directory(manifest: Option<&[u8]>) -> PathBuf {
    manifest
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .and_then(|path| Path::new(path).parent())
        .map_or_else(PathBuf::new, Path::to_path_buf)
}


struct Settings {
    bin_gate: bool,
    debug: bool,
    ignore: IgnoreMode,
    filterset: Option<Filterset>,
    link_template: Option<String>,
    editor: Vec<u8>,
    probe_ms: u64,
    jobs: usize,
    max_fail: Option<usize>,
    no_tests: NoTestsPolicy,
    process: ProcessMode,
    output: OutputMode,
    output_why: &'static str,
    color: bool,
    mouse: Option<bool>,
    exit_with_parent: ExitWithParent,
    test_limit_ms: Option<u32>,
    heartbeat_ms: u64,
    source_root: PathBuf,
}

const SETTING_KEYS: [&str; 28] = [
    "CARGO_VAL_EDITOR_PROBE_MS",
    "RUST_TEST_THREADS",
    "CARGO_VAL_JOBS",
    "CARGO_VAL_MAX_FAIL",
    "CARGO_VAL_FAIL_FAST",
    "CARGO_VAL_NO_TESTS",
    "CARGO_VAL_PROCESS",
    "CARGO_VAL_OUTPUT",
    "CARGO_VAL_EXIT_WITH_PARENT",
    "CARGO_VAL_TEST_TIMEOUT",
    "CARGO_VAL_HEARTBEAT_SECS",
    "CI",
    "TERM",
    "CARGO_VAL_COLOR",
    "CARGO_VAL_MOUSE",
    "NO_COLOR",
    "CARGO_VAL_IGNORED",
    "CARGO_VAL_FILTERSET",
    "CARGO_VAL_BIN_GATE",
    "CARGO_VAL_LOG",
    "CLAUDECODE",
    "CODEX_SANDBOX",
    "CURSOR_TRACE_ID",
    "GEMINI_CLI",
    "CARGO_VAL_EDITOR_URL",
    "VISUAL",
    "EDITOR",
    "TERM_PROGRAM",
];

fn runtime_settings(
    environment: &Environment<'_>,
    harness: &HarnessRequest<'_>,
    parser: &CliParser<'_>,
) -> Result<Settings, RunError> {
    let mut pairs = [("", ""); SETTING_KEYS.len()];
    let mut count = 0;
    for key in SETTING_KEYS {
        if let Some(value) = environment.get(key.as_bytes()) {
            pairs[count] = (
                key,
                core::str::from_utf8(value).map_err(|_| RunError::Settings)?,
            );
            count += 1;
        }
    }
    let source = BorrowedSettings::new(&pairs[..count]);
    settings_from(&source, harness, parser)
}

fn settings_from(
    source: &BorrowedSettings<'_>,
    harness: &HarnessRequest<'_>,
    parser: &CliParser<'_>,
) -> Result<Settings, RunError> {
    let process = PROCESS
        .build_default(Some(crate::build_defaults::PROCESS))
        .from_settings(source, parser.process())
        .resolve::<ProcessSetting>()
        .map_err(RunError::Setting)?
        .0
        .mode(cfg!(windows));
    let (output, output_why) = resolve_output(source, parser)?;
    if parser.timeline() && output.is_agent_like() {
        return Err(RunError::Timeline);
    }    // a shard child holds many tests, so shard mode keeps about twice as many children as cores in flight
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
    let jobs = ThreadSources::from_settings(source, harness.test_threads, default_jobs)
        .resolve(LIVE_CAPACITY)
        .map_err(RunError::TestThreads)?
        .value;
    let max_fail = FailFastSources::from_settings(source, parser.fail_fast())
        .resolve()
        .map_err(RunError::FailFast)?
        .max_fail;
    let no_tests = NO_TESTS
        .from_settings(source, parser.no_tests())
        .resolve::<NoTestsPolicy>()
        .map_err(RunError::Setting)?
        .0;
    let color = output == OutputMode::Rich
        && resolve_color(source, crate::build_defaults::COLOR).map_err(|_| RunError::Settings)?;
    let mouse = (output == OutputMode::Rich).then(|| resolve_mouse(source)).transpose()?;
    let ignore = harness.ignore_mode(source).map_err(RunError::Setting)?;
    let filterset = FILTERSET
        .from_settings(source, parser.filterset())
        .supplied()
        .map(|(_, text)| {
            Filterset::parse(text).map_err(|error| {
                report_filterset_error(text, error);
                RunError::Filterset(error)
            })
        })
        .transpose()?;
    let editor_url = crate::hyperlink::EDITOR_URL
        .from_settings(source, None)
        .supplied()
        .map(|(_, text)| core::str::from_utf8(text).map(String::from).map_err(|_| RunError::Settings))
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
    let exit_with_parent = resolve_exit_with_parent(source, parser)?;
    QUIT_LOG.store(debug, Ordering::Relaxed);
    Ok(Settings {
        bin_gate,
        debug,
        ignore,
        filterset,
        link_template: links_enabled(output, color, true).then(|| String::from_utf8_lossy(editor_template(editor_url.as_deref(), source)).into_owned()),
        editor: crate::editor_launch::editor_command(source.get("VISUAL"), source.get("EDITOR")),
        probe_ms: source.get("CARGO_VAL_EDITOR_PROBE_MS").map(|text| text.parse::<u64>().map_err(|_| RunError::Settings)).transpose()?.unwrap_or(crate::editor_launch::PROBE_DEADLINE_MS),
        jobs,
        max_fail,
        no_tests,
        process,
        output,
        output_why,
        color,
        mouse,
        exit_with_parent,
        test_limit_ms: TEST_TIMEOUT.from_settings(source, parser.test_timeout()).resolve::<TestTimeout>().map_err(RunError::Setting)?.0.limit_ms(),
        heartbeat_ms: crate::config::heartbeat_millis(source, parser.heartbeat()).map_err(RunError::Setting)?,
        source_root: manifest_directory(parser.manifest_path()),
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
    track_child(target).map_err(|code| if code == TABLE_FULL { RunError::SignalSlots } else { RunError::Reactor })
}

// the editor leads its own group too, but a walk hangs it up: an interrupt is only a key to it
fn track_hangup(target: u32) -> Result<(), RunError> {
    track_group_hangup(target).map_err(|code| if code == TABLE_FULL { RunError::SignalSlots } else { RunError::Reactor })
}

// a session child leads its own process group, so the signal table holds its group and a forwarded signal reaches what the test spawned
fn track_leader(target: u32) -> Result<(), RunError> {
    track_group(target).map_err(|code| if code == TABLE_FULL { RunError::SignalSlots } else { RunError::Reactor })
}

/// Runs `spawn` only after claiming a signal-table slot, so a full table refuses while no child exists, and hands the slot back if the spawn fails.
///
/// # Errors
///
/// Returns [`RunError::SignalSlots`] when the table is full, otherwise the error `spawn` returned.
pub fn spawn_reserved<Output>(spawn: impl FnOnce() -> Result<Output, RunError>) -> Result<Output, RunError> {
    reserve_slot().map_err(|_| RunError::SignalSlots)?;
    spawn().inspect_err(|_| abandon_slot())
}

fn session_streams(lane: &Lane) -> Option<(Rc<ChildStream>, Rc<ChildStream>)> {
    Some((lane.0.borrow().clone()?, lane.1.borrow().clone()?))
}

// a session's child runs on its own prime lane; its streams and exit arrive as items shaped like the events the outcome ledger already takes
fn start_session(
    ledger: &Ledger<'_>,
    watches: &mut Watches,
    (lanes, store): (&Lanes, &Rc<edge_store::Store>),
    slot: SessionSlotId,
    (shard, scope): (Option<ShardWatch>, Signal),
) -> Result<(), RunError> {
    let child = ledger
        .sessions()
        .get(slot)
        .ok_or(RunError::Outcome)?
        .child();
    let process_id = child.process_id();
    track_leader(process_id)?;
    attach_session(&lanes[JOB_BASE + slot.index()], child, (store, u32::try_from(slot.index() * 2).map_err(|_| RunError::Outcome)?))?;
    *lanes[JOB_BASE + slot.index()].4.borrow_mut() = Some(SignalSource::new(&scope));
    watches[slot.index()] = Some(TestWatch {
        slot,
        process_id,
        scope,
        output: ChildOutput::new(),
        streams: session_streams(&lanes[JOB_BASE + slot.index()]),
        shard,
        group: [0; GROUP_CAPACITY],
        group_length: 0,
        started_ms: monotonic_millis(),
        status: None,
        rerun: None,
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
            HostEvent::Stderr { slot, bytes: piece }
        };
        let step = deliver(ledger, event, run.reporting(), watch)?;
        if shard_tripped {
            run.trip.fire();
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
    if !shard::resume_announcing(run, ledger, watch)? {
        return Ok(false);
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
        run.trip.fire();
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
    let step = deliver(ledger, event, run.reporting(), watch)?;
    settled(run, ledger, watches, index, step).map(|_| ())
}

pub(crate) fn session_exited(
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
    index: usize,
    code: i32,
) -> Result<(), RunError> {
    let Some(watch) = watches[index].as_mut() else {
        return Ok(());
    };
    watch.status = Some(code);
    quit_step(b"child-reaped", watch.process_id as usize);
    release_group(watch.process_id);
    let (slot, exit) = (watch.slot, watch.exit(code));
    if watch.rerun.is_some() && exit == Err(RunError::Cancelled) {
        watches[index] = None;
        ledger.retire(slot).ok_or(RunError::Outcome)?;
        return Ok(());
    }
    if run.settings.process == ProcessMode::Test && exit == Err(RunError::Cancelled) {
        let target = ledger.sessions().get(slot).map(RunningChild::target);
        watches[index] = None;
        ledger.retire(slot).ok_or(RunError::Outcome)?;
        return shard::withdraw_solo(run, ledger, target);
    }
    let event = HostEvent::Exit {
        slot,
        status: code,
    };
    let step = deliver(ledger, event, run.reporting(), watch)?;
    settled(run, ledger, watches, index, step).map(|_| ())
}

pub(crate) fn session_cancelled(ledger: &Ledger<'_>, watches: &mut Watches, index: usize) {
    let Some(watch) = watches[index].as_mut() else {
        return;
    };
    if let Some(session) = ledger.sessions().get(watch.slot) {
        quit_step(b"child-signalled", watch.process_id as usize);
        session.child().terminate();
    }
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
        OutcomeStep::Reported { .. } => {
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

// one line per child, so a wait on another cargo process is never silent; the live frame is erased first, or the line lands one row under a frame the next redraw cannot reach
fn announce_lock_wait(watch: &mut LockWatch, bytes: &[u8], render: &Render<'_>) -> Result<(), RunError> {
    let Some(kind) = watch.observe(bytes) else {
        return Ok(());
    };
    if render.0 == OutputMode::Rich {
        let lock = match kind {
            LockKind::PackageCache => &b"package cache"[..],
            LockKind::BuildDirectory | LockKind::Other => watch.which(),
        };
        return emit(render, &Event::Waiting { lock });
    }
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

pub(crate) type RenderState<'storage> = State<'storage, RERUN_BYTES>;

/// The run's output mode with the one render state and the frame its projection fills; plain output is `emit` into this.
pub(crate) type Render<'storage> = (OutputMode, RefCell<RenderState<'storage>>, RefCell<&'storage mut Line<FRAME_BYTES>>, RefCell<Option<Rc<edge_store::Store>>>);

const TIMELINE_COLUMNS: usize = 80;
const PROJECTED_LINE: usize = NAME_CAPACITY + 2 * 128 + 64;

const fn rendered(outcome: TerminalOutcome) -> Outcome {
    match outcome {
        TerminalOutcome::Passed => Outcome::Passed,
        TerminalOutcome::Failed => Outcome::Failed,
        TerminalOutcome::Ignored => Outcome::Ignored,
    }
}

/// Projects one block-edge event without touching the state, for lines a shard child's buffer keeps.
pub(crate) fn projected(render: &Render<'_>, event: &Event<'_>) -> Line<PROJECTED_LINE> {
    let mut line = Line::new();
    _ = render.1.borrow().project(event, &mut line);
    line
}

/// Moves the cursor back over the live frame, if one is drawn, so what is written next lands where the frame was.
pub(crate) fn erase_live(render: &Render<'_>) -> Result<(), RunError> {
    let (output, state, frame, _) = render;
    let mut frame = frame.borrow_mut();
    let frame: &mut Line<FRAME_BYTES> = &mut frame;
    frame.clear();
    if *output == OutputMode::Rich {
        state.borrow_mut().erase_rich(frame);
    }
    if frame.bytes().is_empty() {
        return Ok(());
    }
    write_fd(1, frame.bytes())
}

/// Writes bytes a shard child kept for the terminal; a rich run writes nothing above its live frame, review holds the detail.
pub(crate) fn emit_kept(render: &Render<'_>, bytes: &[u8]) -> Result<(), RunError> {
    if render.0 == OutputMode::Rich {
        return Ok(());
    }
    write_fd(1, bytes)
}

const STATUS_WORDS: [&[u8]; 14] = [
    b"Compiling", b"Fresh", b"Finished", b"Blocking", b"Checking", b"Building", b"Downloading", b"Downloaded", b"Updating", b"Locking", b"Adding", b"Documenting", b"Running", b"Dirty",
];

const LOCK_WAIT: &[u8] = b"waiting for file lock on ";

fn is_status_line(line: &[u8]) -> bool {
    STATUS_WORDS.iter().any(|word| {
        let mut seen = visible(line).skip_while(|byte| *byte == b' ');
        seen.by_ref().take(word.len() + 1).eq(word.iter().copied().chain(core::iter::once(b' ')))
    })
}

// the stream key of the warning chain; pages carry no key, so it only names the chain in its view
const WARNINGS_STREAM: u32 = u32::MAX - 1;
const WARNING_PAGE_BYTES: usize = 4096;

// a rich run writes nothing above its frame, so cargo's own warnings are sealed into the run store and the state keeps the chain
fn hold_warnings(render: &Render<'_>, bytes: &[u8]) {
    let Some(store) = render.3.borrow().clone() else {
        return;
    };
    let mut state = render.1.borrow_mut();
    let chain = bytes.chunks(WARNING_PAGE_BYTES).try_fold(state.held(), |chain, page| {
        edge_store::append(&store, (WARNINGS_STREAM, chain.0, chain.1, 0), page).map(|(_, first, last, _)| (first, last)).inspect_err(|error| edge_store::lose(&store, page.len(), error))
    });
    if let Ok(chain) = chain {
        state.hold(chain);
    }
}

// streams what a rich run held back from the store in order, once, after the frame is gone and before the closing block
fn release_warnings(render: &Render<'_>) -> Result<(), RunError> {
    let (first, last) = render.1.borrow().held();
    let Some(store) = render.3.borrow().clone() else {
        return Ok(());
    };
    if first == 0 {
        return Ok(());
    }
    erase_live(render)?;
    let mut page = [0_u8; WARNING_PAGE_BYTES];
    let mut position = (0, first);
    while let Ok((at, length)) = edge_store::next_page(&store, position, &mut page) {
        write_fd(2, &page[..length])?;
        position = (at, at + (edge_store::HEADER_BYTES + length) as u64);
        if at == last {
            break;
        }
    }
    render.1.borrow_mut().hold((0, 0));
    Ok(())
}

// a rich run shows cargo's progress in the frame, so status lines are consumed; anything else is held for the closing block
fn summarize_status_lines(bytes: &[u8], render: &Render<'_>) -> Result<(), RunError> {
    let mut rest = bytes;
    while !rest.is_empty() {
        let length = memchr::memchr(b'\n', rest).map_or(rest.len(), |newline| newline + 1);
        let (line, after) = rest.split_at(length);
        rest = after;
        if !is_status_line(line) {
            hold_warnings(render, line);
            continue;
        }
        if let Some(at) = memchr::memmem::find(line, LOCK_WAIT) {
            let lock = line[at + LOCK_WAIT.len()..].trim_ascii_end();
            emit(render, &Event::Waiting { lock })?;
        }
    }
    Ok(())
}

/// The events whose rich projection is a frame redraw, recomputed from the state by the next one; while the editor owns the terminal they are not projected, and every other event is scrollback that waits in the status queue.
const fn redraws_frame(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Tick { .. } | Event::Key(_) | Event::Notice { .. } | Event::Resize { .. } | Event::Begin { .. } | Event::Previous { .. } | Event::Waiting { .. } | Event::Reran { .. } | Event::Retried { .. }
    )
}

/// Applies `event` to the render state and writes its projection to standard output; a frame that was too small says so on stderr.
pub(crate) fn emit(render: &Render<'_>, event: &Event<'_>) -> Result<(), RunError> {
    render.1.borrow_mut().apply(event);
    show(render, event)
}

// the projection of an event the state already applied
fn show(render: &Render<'_>, event: &Event<'_>) -> Result<(), RunError> {
    let (output, state, frame, store) = render;
    let (mut state, mut frame) = (state.borrow_mut(), frame.borrow_mut());
    let frame: &mut Line<FRAME_BYTES> = &mut frame;
    frame.clear();
    let projected = match output {
        OutputMode::Plain | OutputMode::Agent | OutputMode::Json => state.project(event, frame),
        OutputMode::Rich => {
            if matches!(event, Event::Begin { .. }) {
                CURSOR_HIDDEN.store(true, Ordering::SeqCst);
            }
            if editor_reserved() && redraws_frame(event) {
                return Ok(());
            }
            fill_review(&mut state, store);
            let projected = state.project_live(event, frame);
            MOUSE_REPORTING.store(state.mouse_on(), Ordering::SeqCst);
            projected
        }
    };
    flush(frame, projected)
}

// the review shows the selected failure's output window, read from the run store once per selection into the state's one buffer
fn fill_review(state: &mut RenderState<'_>, store: &RefCell<Option<Rc<edge_store::Store>>>) {
    let Some(store) = store.borrow().clone() else {
        return;
    };
    state.fill_window(|view, buffer| match <&mut [u8; WINDOW_BYTES]>::try_from(buffer) {
        Ok(window) => window::locate(&store, (view, &[]), window),
        Err(_) => 0,
    });
}

fn flush(frame: &Line<FRAME_BYTES>, projected: Result<(), usize>) -> Result<(), RunError> {
    if !frame.bytes().is_empty() {
        write_fd(1, frame.bytes())?;
    }
    let Err(lost) = projected else {
        return Ok(());
    };
    let mut notice = Line::<96>::new();
    notice.text(b"cargo-val: output frame cut ");
    notice.number(lost);
    notice.text(b" bytes\n");
    write_fd(2, notice.bytes())
}

/// Writes the timeline of the run so far, 80 columns wide, after the closing block or the rich receipt.
pub(crate) fn emit_timeline(render: &Render<'_>) -> Result<(), RunError> {
    let (_, state, frame, _) = render;
    let mut frame = frame.borrow_mut();
    let frame: &mut Line<FRAME_BYTES> = &mut frame;
    frame.clear();
    let projected = state.borrow().project_timeline(TIMELINE_COLUMNS, frame);
    flush(frame, projected)
}

/// Emits an event that only the live frame shows; plain output already prints its totals once, at the close.
pub(crate) fn emit_live(render: &Render<'_>, event: &Event<'_>) -> Result<(), RunError> {
    if render.0 == OutputMode::Rich {
        return emit(render, event);
    }
    Ok(())
}

/// Set from `CARGO_VAL_LOG=debug`; [`quit_step`] writes nothing while it is clear.
static QUIT_LOG: AtomicBool = AtomicBool::new(false);

/// One stamped line per step of leaving the run, written straight to descriptor 2 so the stamp is the moment the step ran, not the moment a queue drained.
pub fn quit_step(step: &[u8], detail: usize) {
    if !QUIT_LOG.load(Ordering::Relaxed) {
        return;
    }
    let mut line = Line::<192>::new();
    line.text(b"cargo-val: debug: quit ms=");
    line.number(usize::try_from(monotonic_millis()).unwrap_or(usize::MAX));
    line.text(b" step=");
    line.text(step);
    line.text(b" detail=");
    line.number(detail);
    line.text(b"\n");
    let _ = crate::edge_output::write(2, line.bytes());
}

/// Like [`quit_step`] for the build-and-test pipeline: one stamped line naming the target, binary or test a step concerns, so the order work started in is readable from the log alone.
pub fn pipeline_step(step: &[u8], name: &[u8]) {
    if !QUIT_LOG.load(Ordering::Relaxed) {
        return;
    }
    let mut line = Line::<384>::new();
    line.text(b"cargo-val: debug: pipeline ms=");
    line.number(usize::try_from(monotonic_millis()).unwrap_or(usize::MAX));
    line.text(b" step=");
    line.text(step);
    line.text(b" name=");
    line.text(name);
    line.text(b"\n");
    let _ = crate::edge_output::write(2, line.bytes());
}

/// Set once the live frame hides the cursor; [`restore_cursor`] shows it again exactly once on any exit path.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Set while a review frame has asked the terminal to report the mouse; [`restore_cursor`] turns reporting off on any exit path the queued disable never reached.
static MOUSE_REPORTING: AtomicBool = AtomicBool::new(false);

/// Set once the terminal is in key-at-a-time mode and a key lane reads it; [`restore_cursor`] puts the mode back.
static KEYS_RAW: AtomicBool = AtomicBool::new(false);

/// Set from the moment the editor is asked for until it has exited: the editor owns the terminal, so frame redraws are not projected meanwhile and scrollback waits in the status queue.
static EDITOR_RESERVED: AtomicBool = AtomicBool::new(false);

/// The running editor's process group, or zero; [`restore_cursor`] ends it and takes the terminal back on any exit path.
static EDITOR_GROUP: AtomicU32 = AtomicU32::new(0);

const EXIT_FAULT_CODE: i32 = 5;

fn editor_reserved() -> bool {
    EDITOR_RESERVED.load(Ordering::SeqCst)
}

pub(crate) fn editor_running() -> bool {
    EDITOR_GROUP.load(Ordering::SeqCst) != 0
}

// each tick: an exit source cannot see a stop, so a stopped editor is continued here
pub(crate) fn keep_editor_running() {
    let group = EDITOR_GROUP.load(Ordering::SeqCst);
    if group != 0 {
        crate::editor_launch::keep_running(group);
    }
}

// the terminal goes back first: leaving raw mode from a background group would stop this process on SIGTTOU
fn abandon_editor() {
    EDITOR_RESERVED.store(false, Ordering::SeqCst);
    let group = EDITOR_GROUP.swap(0, Ordering::SeqCst);
    quit_step(b"editor-abandon", group as usize);
    if group != 0 {
        crate::editor_launch::abandon(group);
        release_group(group);
        quit_step(b"editor-reclaimed", usize::try_from(crate::editor_launch::foreground_group()).unwrap_or(usize::MAX));
    }
}

/// Whether the run put the terminal in key-at-a-time mode, so the key lane should read it.
pub(crate) fn keys_raw() -> bool {
    KEYS_RAW.load(Ordering::SeqCst)
}

const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

/// Shows the cursor if the live frame hid it, writing directly: the fallback for the panic hook and for exits that never reached the queued close.
/// The normal close queues its show-cursor through `close_cursor` so it lands behind the erase and the closing block, never ahead of them.
pub fn restore_cursor() {
    if MOUSE_REPORTING.swap(false, Ordering::SeqCst) {
        let _ = crate::edge_output::write(1, crate::render::review::MOUSE_OFF);
    }
    abandon_editor();
    if KEYS_RAW.swap(false, Ordering::SeqCst) {
        crate::edge_output::leave_raw_mode(0);
    }
    if CURSOR_HIDDEN.swap(false, Ordering::SeqCst) {
        let _ = crate::edge_output::write(1, SHOW_CURSOR);
    }
}

/// Erases the live frame from the screen; plain mode has no frame, so it writes nothing.
fn erase_frame(render: &Render<'_>) -> Result<(), RunError> {
    let (output, state, frame, _) = render;
    if *output != OutputMode::Rich {
        return Ok(());
    }
    let mut frame = frame.borrow_mut();
    frame.clear();
    state.borrow_mut().erase_rich(&mut **frame);
    if !frame.bytes().is_empty() {
        write_fd(1, frame.bytes())?;
    }
    Ok(())
}

/// Queues the show-cursor behind everything already queued, so the cursor returns only after the erase and the closing block.
fn close_cursor() -> Result<(), RunError> {
    if CURSOR_HIDDEN.load(Ordering::SeqCst) {
        write_fd(1, SHOW_CURSOR)?;
        CURSOR_HIDDEN.store(false, Ordering::SeqCst);
    }
    Ok(())
}

/// Where a finished test's counters and color choice live.
#[derive(Clone, Copy)]
struct Reporting<'run, 'storage> {
    color: bool,
    targets: &'run RefCell<TargetTable>,
    ready: &'run Cell<Option<SessionSlotId>>,
    run: &'run ListingRun<'storage>,
    jobs: usize,
    interrupted: bool,
    render: &'run Render<'storage>,
}

impl Reporting<'_, '_> {
    // a frozen child counts as a live session but not as running, so it starts the moment a job slot frees
    // once the run is stopped its scope has fired and the cancel arm ends it: resuming it would launch a test after the stop
    // it returns the slot it resumed so its start is announced by `announce` after the finished test settles
    fn settle_ready(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> Option<SessionSlotId> {
        let slot = self.ready.get()?;
        let Some(session) = ledger.sessions().get(slot) else {
            self.ready.set(None);
            return None;
        };
        if self.run.admit(ledger).is_err() {
            self.ready.set(None);
            return None;
        }
        if ledger.sessions().len().saturating_sub(1) >= self.jobs {
            return None;
        }
        resume_ready(session.child());
        self.ready.set(None);
        quit_step(&[b"ready-resumed ".as_slice(), session.name_bytes()].concat(), slot.index());
        Some(slot)
    }

    // a frozen child is created while another test still runs, so announcing its start at creation counted both as busy;
    // the start is announced once the slot it took has settled
    fn announce(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        resumed: Option<SessionSlotId>,
    ) -> Result<(), RunError> {
        let Some(session) = resumed.and_then(|slot| ledger.sessions().get(slot)) else {
            return Ok(());
        };
        let targets = self.targets.borrow();
        let id = (targets.id(session.target()), session.name_bytes());
        emit(self.render, &Event::Started { id, solo: false, at_ms: monotonic_millis(), limit_ms: self.run.settings.test_limit_ms })?;
        Ok(())
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
    reporting: Reporting<'_, '_>,
    watch: &TestWatch,
) -> Result<OutcomeStep<'bytes>, RunError> {
    let verdict = watch.verdict();
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
        OutcomeStep::Reported { outcome, .. } => {
            let resumed = reporting.settle_ready(ledger);
            let name = &report[..name_length.saturating_sub(1)];
            let targets = reporting.targets.borrow();
            let id = (target.map_or(&b""[..], |index| targets.id(index)), name);
            if let Some(started) = watch.rerun {
                let millis = monotonic_millis().saturating_sub(started);
                quit_step(&[b"rerun-settled ".as_slice(), &report[..name_length.saturating_sub(1)]].concat(), usize::from(outcome == TerminalOutcome::Passed));
                let rerun = if outcome == TerminalOutcome::Passed { Standing::Passed { millis } } else { Standing::Failed { millis } };
                if outcome == TerminalOutcome::Failed {
                    reporting.run.emit_failure(id, watch.streams.as_ref(), watch.status.map_or(Exit::Code(1), Exit::from_status))?;
                }
                emit(reporting.render, &Event::Reran { id, rerun })?;
                reporting.announce(ledger, resumed)?;
                return Ok(step);
            }
            let withdrawn = reporting.interrupted && reporting.render.0.is_agent_like();
            if !withdrawn {
                emit(reporting.render, &Event::Settled { id, outcome: rendered(outcome), at_ms: Some(monotonic_millis()), solo: false })?;
            }
            if outcome == TerminalOutcome::Failed && !withdrawn {
                let exit = watch.status.map_or(Exit::Code(1), Exit::from_status);
                reporting.run.emit_failure(id, watch.streams.as_ref(), exit)?;
            }
            drop(targets);
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
            reporting.announce(ledger, resumed)?;
            Ok(step)
        }
        OutcomeStep::Complete { .. }
        | OutcomeStep::Pending
        | OutcomeStep::Stdout(_)
        | OutcomeStep::Stderr(_) => Ok(step),
    }
}

// a rerun is a fix-loop step: a launch that fails puts the row back and says why on the review footer, and review stays open
fn refuse_rerun(run: &ListingRun<'_>, id: (&[u8], &[u8]), error: RunError) -> Result<(), RunError> {
    let mut notice = Line::<120>::new();
    notice.text(b"rerun not started: ");
    match error {
        RunError::Launch(LaunchCause::Spawn { os_code }) => {
            notice.text(b"the OS refused the launch, error ");
            notice.number(usize::try_from(os_code).unwrap_or(0));
        }
        RunError::Launch(LaunchCause::Full) => notice.text(b"no free job slot"),
        RunError::Launch(LaunchCause::Duplicate) => notice.text(b"the test is already running"),
        RunError::Launch(_) => notice.text(b"the launch was refused"),
        RunError::TestEnvironment => notice.text(b"the test environment could not be built"),
        other => return Err(other),
    }
    notice.text(b": ");
    notice.text(id.1);
    emit(run.render(), &Event::Reran { id, rerun: Standing::Idle })?;
    emit(run.render(), &Event::Warned { id, what: notice.bytes() })
}

fn rerun_live(run: &ListingRun<'_>, ledger: &Ledger<'_>, watches: &Watches, id: (&[u8], &[u8])) -> bool {
    let targets = run.targets.borrow();
    watches
        .iter()
        .flatten()
        .filter(|watch| watch.rerun.is_some())
        .filter_map(|watch| ledger.sessions().get(watch.slot))
        .any(|session| targets.id(session.target()) == id.0 && session.name_bytes() == id.1)
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
    let child = spawn_reserved(|| {
        spawn_listed(
            storage,
            executable.as_bytes(),
            ignored,
            run.environment,
            overrides,
            directory.as_bytes(),
        )
        .map_err(|_| RunError::Spawn)
    })?;
    track(child.process_id())?;
    Ok(child)
}

fn slot_range(slot: usize) -> Range<usize> {
    slot * ENVIRONMENT_CAPACITY..(slot + 1) * ENVIRONMENT_CAPACITY
}

// each listing holds up to two signal slots (the list and the ignored-list child) beside every running test, the build cargo and the doc test group, so the table is never asked for more than it has
fn listing_limit(jobs: usize) -> usize {
    let headroom = SIGNAL_SLOTS.saturating_sub(jobs + SIGNAL_HOLDERS) / 2;
    jobs.clamp(2, LISTING_SLOTS).min(headroom).max(1)
}

fn listings_in_flight(active: Option<&PendingListing>, pending: &VecDeque<PendingListing>) -> usize {
    usize::from(active.is_some()) + pending.len()
}

fn free_slot(active: Option<&PendingListing>, pending: &VecDeque<PendingListing>) -> usize {
    (0..LISTING_SLOTS)
        .find(|slot| active.is_none_or(|listing| listing.slot != *slot) && pending.iter().all(|listing| listing.slot != *slot))
        .unwrap_or(0)
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
    pipeline_step(b"listing-spawned", artifact.target_name.as_bytes());
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
    let (run, target) = (scan.run, scan.target);
    let slot = spawn_exact(scan, ledger, watches, intent, None).map_err(|error| match error {
        RunError::Launch(cause) => report_launch_failure(cause, intent.test_name(), executable_text(intent.executable())),
        other => other,
    })?;
    run.targets.borrow_mut().tally(target, Tally::Launched);
    pipeline_step(b"test-launched", intent.test_name().as_bytes());
    let targets = run.targets.borrow();
    let id = (targets.id(target), intent.test_name().as_bytes());
    if run.ready.get() != Some(slot) {
        emit(run.render(), &Event::Started { id, solo: false, at_ms: monotonic_millis(), limit_ms: run.settings.test_limit_ms })?;
    }
    drop(targets);
    shard::note_group(run, watches, slot.index(), intent.test_name());
    Ok(())
}

// the one spawn, capture and settle path: a rerun differs only in the scope its session lives under and the start it carries to its watch
fn spawn_exact(
    scan: &ListingScan<'_, '_>,
    ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut Watches,
    intent: ExactTestIntent<'_, '_, '_>,
    rerun_started: Option<u64>,
) -> Result<SessionSlotId, RunError> {
    let (run, target, environment, layout) = (scan.run, scan.target, scan.environment, scan.layout);
    let (overrides, override_count) = override_entries(layout, environment)?;
    if rerun_started.is_none() && run.tripped(ledger) {
        run.policy.borrow_mut().launched();
    }
    let mut storage = run.launch_storage.borrow_mut();
    let park = run.parks_next(ledger);
    let slot = spawn_reserved(|| {
        LaunchInput {
            target,
            owner: LaunchOwner::Ledger(&mut *ledger),
            intent,
            inherited_environment: run.environment,
            child_overrides: &overrides[..override_count],
            storage: &mut storage,
            harness_arguments: run.harness_arguments,
            current_directory: layout.current_directory(environment).as_bytes(),
        }
        .launch_as(park)
        .map_err(|error| RunError::Launch(LaunchCause::from(&error)))
    })?;
    if park {
        run.ready.set(Some(slot));
    }
    let scope = run.session_scope(rerun_started.map(|_| Rerun::OutputOnly));
    start_session(ledger, watches, (scan.lanes, &scan.run.store), slot, (None, scope))?;
    if let Some(watch) = watches[slot.index()].as_mut() {
        watch.rerun = rerun_started;
    }
    Ok(slot)
}

// an output-only rerun is the only place a tripping test's block is printed, so only an interrupt scopes it
pub(crate) fn scope_for(rerun: Option<Rerun>, (root, trip): (&Signal, &Signal)) -> Signal {
    match rerun {
        Some(Rerun::OutputOnly) => root.clone(),
        _ => trip.clone(),
    }
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
    docs: DocObservation<'run>,
    policy: RefCell<StopPolicy>,
    doc: RefCell<Option<Box<DocRun>>>,
    doc_pending: Cell<bool>,
    launch_storage: RefCell<HeldLaunchStorage>,
    ready: Cell<Option<SessionSlotId>>,
    shards: RefCell<ShardArena<'run>>,
    sched: RefCell<SchedTable<'run>>,
    specials: RefCell<SpecialPlan<'run>>,
    sched_target: Cell<Option<usize>>,
    interrupt_reported: Cell<usize>,
    interrupt_not_run: Cell<usize>,
    quit: Cell<Option<i32>>,
    store: Rc<edge_store::Store>,
    editing: RefCell<Option<Vec<Vec<u8>>>>,
    editor_program: RefCell<Vec<u8>>,
    request: RefCell<Option<(Vec<u8>, u32, u32)>>,
    outside: RefCell<Vec<Vec<Vec<u8>>>>,
    detached_running: Cell<bool>,
    probe: RefCell<Option<(u32, u64)>>,
    probe_site: RefCell<(Vec<u8>, u32, u32)>,
    probe_output: RefCell<(Vec<u8>, Option<bool>, bool)>,
    reads: RefCell<&'run mut [u8]>,
    cursor: Cell<[u64; 5]>,
    head: Cell<usize>,
    root: Signal,
    trip: Signal,
}

pub(crate) struct DocObservation<'run> {
    render: &'run Render<'run>,
    passed: Cell<usize>,
    failed: Cell<usize>,
    ignored: Cell<usize>,
    blocks: Cell<usize>,
    empty_blocks: Cell<usize>,
    failure_lines: RefCell<Vec<u8>>,
    first_failed: RefCell<Vec<u8>>,
}

impl<'run> DocObservation<'run> {
    pub(crate) fn new(render: &'run Render<'run>) -> Self {
        Self {
            render,
            passed: Cell::new(0),
            failed: Cell::new(0),
            ignored: Cell::new(0),
            blocks: Cell::new(0),
            empty_blocks: Cell::new(0),
            failure_lines: RefCell::new(Vec::new()),
            first_failed: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn render(&self) -> &'run Render<'run> {
        self.render
    }
}

impl<'run> ListingRun<'run> {
    pub(crate) fn render(&self) -> &'run Render<'run> {
        self.docs.render
    }

    fn reporting(&self) -> Reporting<'_, 'run> {
        Reporting {
            color: self.settings.color,
            targets: &self.targets,
            ready: &self.ready,
            run: self,
            jobs: self.settings.jobs,
            interrupted: self.interrupted(),
            render: self.docs.render,
        }
    }
}

impl ListingRun<'_> {
    /// Emits a failure with its source excerpt, read once here on the cold path and shaped for this run's output mode.
    pub(crate) fn emit_captured(&self, id: (&[u8], &[u8]), (stdout, stderr): (Stream<'_>, Stream<'_>), exit: Exit) -> Result<(), RunError> {
        let colour = self.settings.output == OutputMode::Rich && self.settings.color && colour_allowed();
        let record = located(stdout, stderr, exit);
        let excerpt = for_failure(&self.settings.source_root, colour, &record);
        let rows = excerpt.as_ref().map(Vec::as_slice).map_err(Vec::as_slice);
        emit(self.render(), &Event::Captured { id, stdout, stderr, exit, excerpt: rows })
    }

    /// Emits a settled failure from its streams: sealed into the run store first, so the store alone holds every byte.
    ///
    /// Each projection takes the window of the output it needs from the store through the run's one read buffer; plain output
    /// queues the whole block and prints it page by page as the output queue has room (see [`ListingRun::resume_failures`]).
    ///
    /// # Errors
    ///
    /// The error of the output write.
    pub(crate) fn emit_failure(&self, id: (&[u8], &[u8]), streams: Option<&(Rc<ChildStream>, Rc<ChildStream>)>, exit: Exit) -> Result<(), RunError> {
        self.emit_shaped(id, streams, exit, |stdout, stderr| (stdout, stderr))
    }

    /// [`ListingRun::emit_failure`] for a caller that narrows the windows to the part of the output it shows, such as libtest's failure block.
    ///
    /// # Errors
    ///
    /// The error of the output write.
    pub(crate) fn emit_shaped(&self, id: (&[u8], &[u8]), streams: Option<&(Rc<ChildStream>, Rc<ChildStream>)>, exit: Exit, shape: impl for<'window> FnOnce(Stream<'window>, Stream<'window>) -> (Stream<'window>, Stream<'window>)) -> Result<(), RunError> {
        let Some((stdout, stderr)) = streams else {
            return self.emit_captured(id, ((b"", 0), (b"", 0)), exit);
        };
        let views = [stdout.settle(), stderr.settle()];
        let (tails, seen) = ([stdout.resident(), stderr.resident()], [stdout.seen(), stderr.seen()]);
        let mut reads = self.reads.borrow_mut();
        let Some((first, rest)) = reads.split_first_chunk_mut::<WINDOW_BYTES>() else {
            return self.emit_captured(id, ((&tails[0], seen[0] - tails[0].len() as u64), (&tails[1], seen[1] - tails[1].len() as u64)), exit);
        };
        let Some((second, _)) = rest.split_first_chunk_mut::<WINDOW_BYTES>() else {
            return self.emit_captured(id, ((&tails[0], seen[0] - tails[0].len() as u64), (&tails[1], seen[1] - tails[1].len() as u64)), exit);
        };
        let lengths = [
            window::locate(&self.store, (views[0], &tails[0]), first).min(SYNC_WINDOW_BYTES),
            window::locate(&self.store, (views[1], &tails[1]), second).min(SYNC_WINDOW_BYTES),
        ];
        let windows = ((&first[..lengths[0]], seen[0] - lengths[0] as u64), (&second[..lengths[1]], seen[1] - lengths[1] as u64));
        let (stdout_window, stderr_window) = shape(windows.0, windows.1);
        let queued = self.settings.output == OutputMode::Plain && windows.0 == stdout_window && windows.1 == stderr_window && tails.iter().all(|tail| tail.is_empty()) && views.iter().any(|view| view.3 > SYNC_WINDOW_BYTES as u64);
        let resume = self.emit_windows(id, (stdout_window, stderr_window), exit, (views, queued))?;
        drop(reads);
        if resume { self.resume_failures() } else { Ok(()) }
    }

    fn emit_windows(&self, id: (&[u8], &[u8]), (stdout, stderr): (Stream<'_>, Stream<'_>), exit: Exit, (views, queued): ([edge_store::View; 2], bool)) -> Result<bool, RunError> {
        let colour = self.settings.output == OutputMode::Rich && self.settings.color && colour_allowed();
        let record = located(stdout, stderr, exit);
        let excerpt = for_failure(&self.settings.source_root, colour, &record);
        let rows = excerpt.as_ref().map(Vec::as_slice).map_err(Vec::as_slice);
        let event = Event::Captured { id, stdout, stderr, exit, excerpt: rows };
        if !queued {
            emit(self.render(), &event)?;
            self.render().1.borrow_mut().keep_views(id, views);
            return Ok(false);
        }
        self.render().1.borrow_mut().apply(&event);
        let position = status_queue::mark();
        let queued_at = {
            let mut state = self.render().1.borrow_mut();
            state.keep_views(id, views);
            state.queue_block(id, position)
        };
        if queued_at.is_some() {
            self.hold_head();
            return Ok(true);
        }
        show(self.render(), &event)?;
        Ok(false)
    }

    // the oldest pending block holds the scrollback at its position; with none pending the scrollback flows
    fn hold_head(&self) {
        let pending = self.render().1.borrow().pending_block(self.head.get());
        status_queue::hold(pending.map(|(_, position)| position));
    }

    // the ledger entry is the block: its name, chains, excerpt and links are the attempt's own, so a later attempt never changes what prints
    fn pop_block(&self, index: usize) {
        self.render().1.borrow_mut().finish_block(index);
        self.head.set(index + 1);
        self.cursor.set([0; 5]);
        self.hold_head();
    }

    /// Whether a failure block still waits to be printed.
    pub(crate) fn failures_pending(&self) -> bool {
        self.render().1.borrow().pending_block(self.head.get()).is_some()
    }

    /// Prints the pending failure blocks in settle order, one frame at a time, while the scrollback ahead of the block is delivered
    /// and the inline lane has room; the pump calls it after every item and wakes it as the screen catches up.
    ///
    /// A block is the GitHub group line, each non-empty stream as a fenced run of its store pages, then the source excerpt.
    /// Pages go through the run's one read page and the render frame, so a block of any size costs no allocation. The pending
    /// blocks are the failure ledger entries from the head index on; only the head prints, and the one cursor is its place.
    /// A full lane only pauses the blocks: the pump keeps reading every producer lane, and the store keeps their output.
    ///
    /// # Errors
    ///
    /// The error of the output write.
    pub(crate) fn resume_failures(&self) -> Result<(), RunError> {
        while status_queue::block_ready() {
            let Some((index, _)) = self.render().1.borrow().pending_block(self.head.get()) else {
                return Ok(());
            };
            if status_queue::insert(|| self.step_block(index))? {
                self.pop_block(index);
            }
        }
        Ok(())
    }

    // stage 0 opens the block, each stream is three stages (open, pages, close) from stage 1 and 4, and stage 7 closes the block
    fn step_block(&self, index: usize) -> Result<bool, RunError> {
        let (_, state, frame, _) = self.render();
        let (state, mut frame) = (state.borrow(), frame.borrow_mut());
        let frame: &mut Line<FRAME_BYTES> = &mut frame;
        let (id, views, links, excerpt) = (state.block_at(index), state.block_views(index), state.block_links(index), state.block_excerpt(index));
        frame.clear();
        let section = self.cursor.get()[0];
        match section {
            0 => {
                state.captured_open(frame, id);
                self.cursor.set([1, 0, 0, 0, 1]);
            }
            7 => {
                state.captured_close(frame, excerpt);
                flush(frame, Ok(()))?;
                return Ok(true);
            }
            _ => self.step_stream(frame, (section, views[usize::try_from((section - 1) / 3).unwrap_or(0)]), links)?,
        }
        flush(frame, Ok(()))?;
        Ok(false)
    }

    fn step_stream(&self, frame: &mut Line<FRAME_BYTES>, (stage, (_, first, last, total)): (u64, edge_store::View), links: (&[u8], &[u8], Option<Location<'_>>)) -> Result<(), RunError> {
        let [_, previous, position, printed, newline_ended] = self.cursor.get();
        let name = if stage < 4 { STDOUT_STREAM } else { STDERR_STREAM };
        match (stage - 1) % 3 {
            0 if total == 0 => self.cursor.set([stage + 3, 0, 0, 0, 1]),
            0 => {
                stream_open(frame, name);
                self.cursor.set([stage + 1, 0, first, 0, 1]);
            }
            1 => {
                let mut reads = self.reads.borrow_mut();
                let page = reads.last_chunk_mut::<PAGE_BYTES>().ok_or(RunError::Outcome)?;
                match edge_store::next_page(&self.store, (previous, position), page) {
                    Ok((at, length)) => {
                        stream_page(frame, &page[..length], links);
                        let ended = u64::from(page[..length].last().is_none_or(|byte| *byte == b'\n'));
                        let after = (at, at + (edge_store::HEADER_BYTES + length) as u64, printed + length as u64);
                        self.cursor.set([if at == last { stage + 1 } else { stage }, after.0, after.1, after.2, ended]);
                    }
                    Err(_) => self.cursor.set([stage + 1, previous, position, printed, newline_ended]),
                }
            }
            _ => {
                stream_close(frame, (newline_ended != 0, total.saturating_sub(printed)));
                self.cursor.set([stage + 1, 0, 0, 0, 1]);
            }
        }
        Ok(())
    }

    pub(crate) const fn signals(&self) -> &SignalPipe {
        self.signals
    }

    pub(crate) fn signal_gate(&self) -> Result<Readiness, RunError> {
        self.signals.gate().map_err(|_| RunError::Reactor)
    }

    pub(crate) fn interrupt_source(&self) -> SignalSource {
        SignalSource::new(self.signals.interrupt())
    }

    pub(crate) fn drain_signals(&self) -> Result<(), RunError> {
        self.signals.drain().map_err(|_| RunError::Reactor)?;
        if self.signals.received().is_some() {
            abandon_editor();
        }
        if self.signals.take_resized() {
            self.refresh_width()?;
        }
        Ok(())
    }

    // the width is data the render state is told, never queried inside it
    pub(crate) fn refresh_width(&self) -> Result<(), RunError> {
        match (self.settings.output, crate::edge_output::columns(1)) {
            (OutputMode::Rich, Some(cols)) => emit(self.render(), &Event::Resize { cols }),
            _ => Ok(()),
        }
    }

    // every session still alive at the moment of the signal is named once, with the group the forward reached and its age, before they are ended
    pub(crate) fn report_interrupt(&self, ledger: &Ledger<'_>, watches: &Watches) -> Result<(), RunError> {
        let Some(signal) = self.signals.received() else {
            return Ok(());
        };
        let live = || {
            watches
                .iter()
                .flatten()
                .filter(|watch| watch.status.is_none())
                .filter_map(|watch| ledger.sessions().get(watch.slot).map(|session| (watch, session)))
        };
        if self.interrupt_reported.get() == 0 {
            if !status_queue::room_for(EVENT_RESERVE) {
                status_queue::defer();
                return Ok(());
            }
            abandon_editor();
            emit(self.render(), &Event::Interrupted { signal: u8::try_from(signal).unwrap_or(u8::MAX), groups: live().count() })?;
            self.interrupt_reported.set(1);
        }
        let targets = self.targets.borrow();
        let arena = self.shards.borrow();
        let now_ms = monotonic_millis();
        let running = live().flat_map(|(watch, session)| {
            let shard_names = watch.shard.iter().flat_map(|shard| shard.unreported(&arena));
            let solo_name = watch.shard.is_none().then(|| session.name_bytes()).into_iter();
            shard_names.chain(solo_name).map(move |name| (watch, session.target(), name))
        });
        for (watch, target, name) in running.skip(self.interrupt_reported.get() - 1) {
            if !status_queue::room_for(EVENT_RESERVE) {
                status_queue::defer();
                return Ok(());
            }
            let id = (targets.id(target), name);
            emit(self.render(), &Event::Running { id, group: watch.process_id, age_ms: now_ms.saturating_sub(watch.started_ms) })?;
            self.interrupt_reported.set(self.interrupt_reported.get() + 1);
        }
        Ok(())
    }

    pub(crate) fn session_scope(&self, rerun: Option<Rerun>) -> Signal {
        scope_for(rerun, (&self.root, &self.trip))
    }

    // quit and interrupt are one event: it is recorded here, the signal table's one walk reaches every spawned child, and the root fires for every consumer that observes it
    // a quit from the review of a finished run leaves nothing to cut short, so only a quit from any other screen is recorded
    pub(crate) fn cancel(&self, interrupt: bool) {
        if interrupt {
            self.signals.raise_interrupt();
        } else {
            let reviewing = matches!(self.render().1.borrow().mode(), Mode::Review { .. });
            self.quit.set((!reviewing).then_some(QUIT_SIGNAL));
            self.signals.stop_children();
        }
        quit_step(b"children-stopped", 0);
        self.root.fire();
        quit_step(b"root-fired", 0);
    }

    // a key is an event like any other: the state moves, the screen redraws, and the action it asked for runs here
    pub(crate) fn key(&self, key: Key) -> Result<(), RunError> {
        let state = self.render().1.borrow();
        let action = state.action(key, |id| state.detail(id));
        let interrupting = matches!(action, Action::Interrupt);
        let leaving = interrupting || matches!(action, Action::Quit);
        let editing = match action {
            Action::OpenEditor { file, line, col } => Some((self.located(file), line, col)),
            _ => None,
        };
        drop(state);
        if leaving {
            quit_step(if interrupting { b"interrupt-key" } else { b"second-q" }, 0);
            self.cancel(interrupting);
        }
        emit(self.render(), &Event::Key(key))?;
        match editing.filter(|_| !editor_reserved()) {
            Some(_) if self.editor_in_flight() => emit(self.render(), &Event::Notice { what: "opening the editor…".as_bytes() }),
            Some(request) => {
                *self.request.borrow_mut() = Some(request);
                Ok(())
            }
            None => Ok(()),
        }
    }

    // the exit source of EDITOR_LANE holds one child: a further press must not replace it before it is reaped
    fn editor_in_flight(&self) -> bool {
        self.probe.borrow().is_some() || self.detached_running.get() || self.request.borrow().is_some() || self.editing.borrow().is_some()
    }

    // the editor is resolved here, at the driver edge, from the run's settings: render only ever sees the action
    fn located(&self, file: &[u8]) -> Vec<u8> {
        let directory = absolute_directory(&self.settings.source_root);
        if file.first() == Some(&b'/') { file.to_vec() } else { [directory.as_bytes(), b"/", file].concat() }
    }

    // the terminal cannot be handed over until the queued frame, erase and show-cursor reach it, so the launch waits for the pump to drain: `edit_pending` ends the pump and `launch_editor` spawns the editor into its lane
    fn begin_handover(&self, argv: Vec<Vec<u8>>) -> Result<(), RunError> {
        erase_live(self.render())?;
        write_fd(1, SHOW_CURSOR)?;
        MOUSE_REPORTING.store(false, Ordering::SeqCst);
        if self.render().1.borrow_mut().pause_mouse() {
            write_fd(1, crate::render::review::MOUSE_OFF)?;
        }
        EDITOR_RESERVED.store(true, Ordering::SeqCst);
        *self.editing.borrow_mut() = Some(argv);
        Ok(())
    }

    pub(crate) fn edit_pending(&self) -> bool {
        self.editing.borrow().is_some() || self.request.borrow().is_some()
    }

    // one decision per press: a probe child is asked first when the editor can only be reused if it is already running, and its answer, or none, picks the commands
    pub(crate) fn launch_editor(&self, lanes: &Lanes) -> Result<bool, RunError> {
        if let Some((file, line, col)) = self.request.borrow_mut().take() {
            let sockets = crate::editor_launch::nvim_sockets(self.environment);
            return match crate::editor_launch::probe_argv(&self.settings.editor, self.environment, &sockets) {
                Some(argv) => self.begin_probe(lanes, &argv, (file, line, col)),
                None => self.dispatch(lanes, (file, line, col), None).map(|()| true),
            };
        }
        self.launch_handover(lanes)
    }

    fn begin_probe(&self, lanes: &Lanes, argv: &[Vec<u8>], request: (Vec<u8>, u32, u32)) -> Result<bool, RunError> {
        let directory = absolute_directory(&self.settings.source_root);
        *self.probe_output.borrow_mut() = (Vec::new(), None, false);
        match crate::editor_launch::spawn_detached(argv, self.environment, directory.as_bytes()) {
            Ok((group, child)) => {
                attach(&lanes[EDITOR_LANE], child)?;
                *self.probe.borrow_mut() = Some((group, monotonic_millis().saturating_add(self.settings.probe_ms)));
                *self.probe_site.borrow_mut() = request;
                Ok(true)
            }
            Err(_) => self.dispatch(lanes, request, None).map(|()| true),
        }
    }

    // the commands for one press, given the probe's answer: detached ones run on the exit lane while review keeps drawing, an attached one takes the terminal
    fn dispatch(&self, lanes: &Lanes, (file, line, col): (Vec<u8>, u32, u32), answer: Option<&[u8]>) -> Result<(), RunError> {
        let sockets = crate::editor_launch::nvim_sockets(self.environment);
        let (detached, mut commands) = crate::editor_launch::commands(&self.settings.editor, &file, (line, col), (self.environment, &sockets), answer);
        if !detached {
            return commands.pop().map_or(Ok(()), |argv| self.begin_handover(argv));
        }
        commands.reverse();
        *self.outside.borrow_mut() = commands;
        self.start_detached(lanes)
    }

    // the next queued detached command goes onto the exit lane; the one before it must have exited 0
    fn start_detached(&self, lanes: &Lanes) -> Result<(), RunError> {
        let Some(argv) = self.outside.borrow_mut().pop() else {
            self.detached_running.set(false);
            return Ok(());
        };
        *self.editor_program.borrow_mut() = argv.first().cloned().unwrap_or_default();
        let directory = absolute_directory(&self.settings.source_root);
        match crate::editor_launch::spawn_detached(&argv, self.environment, directory.as_bytes()) {
            Ok((_, child)) => {
                self.detached_running.set(true);
                attach(&lanes[EDITOR_LANE], child).map(|_| ())
            }
            Err(code) => self.detached_failed(Err(code)),
        }
    }

    fn detached_failed(&self, outcome: Result<i32, i32>) -> Result<(), RunError> {
        self.outside.borrow_mut().clear();
        self.detached_running.set(false);
        let mut notice = Line::<96>::new();
        crate::editor_launch::write_notice(&mut notice, outcome, true, &self.editor_program.borrow());
        emit(self.render(), &Event::Notice { what: notice.bytes() })
    }

    pub(crate) fn editor_output(&self, lanes: &Lanes, range: Range<u32>) -> Result<(), RunError> {
        if self.probe.borrow().is_none() {
            return Ok(());
        }
        let slot = lanes[EDITOR_LANE].0.borrow();
        let bytes = slot.as_deref().and_then(|stream| stream.window(range)).ok_or(RunError::Reactor)?;
        self.probe_output.borrow_mut().0.extend_from_slice(&bytes);
        Ok(())
    }

    pub(crate) fn editor_output_ended(&self, lanes: &Lanes) -> Result<(), RunError> {
        self.probe_output.borrow_mut().2 = true;
        self.settle_probe(lanes)
    }

    // the probe's answer is its stdout once it exited 0 and the stream ended; a probe past its deadline is killed and answers nothing
    fn settle_probe(&self, lanes: &Lanes) -> Result<(), RunError> {
        let (stdout, exited, ended) = self.probe_output.borrow().clone();
        let Some(succeeded) = exited.filter(|_| ended) else {
            return Ok(());
        };
        if self.probe.borrow_mut().take().is_none() {
            return Ok(());
        }
        let (file, line, col) = self.probe_site.take();
        self.dispatch(lanes, (file, line, col), succeeded.then_some(stdout.as_slice()))
    }

    pub(crate) fn expire_probe(&self) {
        let expired = self.probe.borrow().as_ref().is_some_and(|(_, deadline)| monotonic_millis() >= *deadline);
        if expired && let Some((group, _)) = self.probe.borrow().as_ref() {
            crate::editor_launch::terminate(*group);
            self.probe_output.borrow_mut().2 = true;
        }
    }

    fn launch_handover(&self, lanes: &Lanes) -> Result<bool, RunError> {
        let Some(argv) = self.editing.borrow_mut().take() else {
            return Ok(false);
        };
        let program = argv.first().cloned().unwrap_or_default();
        if self.signals.received().is_some() {
            EDITOR_RESERVED.store(false, Ordering::SeqCst);
            return Ok(false);
        }
        let directory = absolute_directory(&self.settings.source_root);
        crate::edge_output::leave_raw_mode(0);
        *self.editor_program.borrow_mut() = program;
        match reserve_slot().and_then(|()| crate::editor_launch::spawn(&argv, self.environment, directory.as_bytes()).inspect_err(|_| abandon_slot())) {
            Ok((group, exit)) => {
                track_hangup(group)?;
                EDITOR_GROUP.store(group, Ordering::SeqCst);
                *lanes[EDITOR_LANE].2.borrow_mut() = Some(exit);
                lanes[KEY_LANE].3.set(false);
                Ok(true)
            }
            Err(code) => self.editor_ended(lanes, Err(code)).map(|()| true),
        }
    }

    pub(crate) fn editor_exited(&self, lanes: &Lanes, status: io::Result<ExitStatus>) -> Result<(), RunError> {
        // the group is cleared before anything else: the reaped pid may be reused, and a tick that still saw it would continue a stranger's group
        let group = EDITOR_GROUP.swap(0, Ordering::SeqCst);
        if group == 0 {
            return self.detached_exited(lanes, status);
        }
        release_group(group);
        crate::editor_launch::reclaim_terminal();
        let outcome = status.map(crate::editor_launch::exit_code).map_err(|error| error.raw_os_error().unwrap_or(EXIT_FAULT_CODE));
        self.editor_ended(lanes, outcome)
    }

    // a probe or a detached editor ended: the probe records its status for `settle_probe`, a detached command hands over to the next or reports a failure in the footer
    fn detached_exited(&self, lanes: &Lanes, status: io::Result<ExitStatus>) -> Result<(), RunError> {
        if self.probe.borrow().is_some() {
            self.probe_output.borrow_mut().1 = Some(status.is_ok_and(|status| status.success()));
            return self.settle_probe(lanes);
        }
        match status.map(crate::editor_launch::exit_code).map_err(|error| error.raw_os_error().unwrap_or(EXIT_FAULT_CODE)) {
            Ok(0) => self.start_detached(lanes),
            outcome => self.detached_failed(outcome),
        }
    }


    // an editor that fails, exits non-zero or cannot start is a footer message on the same review, never an error exit
    fn editor_ended(&self, lanes: &Lanes, outcome: Result<i32, i32>) -> Result<(), RunError> {
        EDITOR_RESERVED.store(false, Ordering::SeqCst);
        lanes[KEY_LANE].3.set(true);
        let raw = crate::edge_output::enter_raw_mode(0);
        write_fd(1, HIDE_CURSOR)?;
        let mut notice = Line::<96>::new();
        crate::editor_launch::write_notice(&mut notice, outcome, raw, &self.editor_program.borrow());
        emit(self.render(), &Event::Notice { what: notice.bytes() })
    }


    // a finished rich run on a terminal with failures keeps the screen: review opens (a half-pressed quit is cancelled first) and the caller pumps keys until it is left
    pub(crate) fn open_review(&self) -> Result<bool, RunError> {
        let waiting = {
            let state = self.render().1.borrow();
            self.render().0 == OutputMode::Rich && keys_raw() && state.failures().next().is_some() && state.mode().is_live()
        };
        if !waiting {
            return Ok(false);
        }
        self.render().1.borrow_mut().hold_review();
        if self.render().1.borrow().mode() == Mode::QuitArmed {
            self.key(Key::Esc)?;
        }
        if self.render().1.borrow().mode() == Mode::Running {
            self.key(Key::Char(b'f'))?;
        }
        Ok(self.reviewing())
    }

    pub(crate) fn reviewing(&self) -> bool {
        self.render().1.borrow().mode().is_live()
    }

    // a forwarded signal only reaches children that honor it, so receiving one fires the root every session scope descends from
    pub(crate) fn interrupted(&self) -> bool {
        let received = self.signals.received().is_some();
        if received {
            quit_step(b"signal-received", 0);
            self.root.fire();
            abandon_editor();
        }
        received
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

    // gate: In = a launch about to happen, Out = (), Err = Interrupted once a signal arrived, else Tripped once the failure budget is spent
    pub(crate) fn admit(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> Result<(), RunError> {
        if let Some(signal) = self.signals.received() {
            self.root.fire();
            return Err(RunError::Interrupted(signal));
        }
        if self.tripped(ledger) { Err(RunError::Tripped) } else { Ok(()) }
    }

    // the one place a refused launch is counted: a tripped refusal puts its names in listed-not-run, an interrupt puts them in the interrupted run's not_run
    pub(crate) fn shed(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        names: usize,
    ) -> bool {
        match self.admit(ledger) {
            Ok(()) => false,
            Err(RunError::Tripped) => {
                (0..names).for_each(|_| self.policy.borrow_mut().listed_test_skipped());
                true
            }
            Err(_) => {
                self.interrupt_not_run.set(self.interrupt_not_run.get() + names);
                true
            }
        }
    }

    pub(crate) fn budget_spent(&self, failures: usize) -> bool {
        self.settings.max_fail.is_some_and(|max_fail| failures >= max_fail)
    }

    fn tripped(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> bool {
        let failures = ledger.snapshot().failed + self.docs.failed.get();
        if self.budget_spent(failures) && self.policy.borrow().report().is_none() {
            let doc = self.interrupt_doc_tests();
            self.policy
                .borrow_mut()
                .record_trip(self.started_sessions(ledger), doc);
            self.trip.fire();
        }
        self.trip.is_fired()
    }

    // a parked child is still frozen, so it never ran and is not in flight
    fn started_sessions(
        &self,
        ledger: &OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    ) -> usize {
        let frozen = self
            .ready
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

    // the doc cargo starts once the test build's cargo has output or exited: both are past its package-cache phases, so the two never contend for that lock
    pub(crate) fn start_doc(
        &self,
        lanes: &Lanes,
        launch: &mut Option<impl FnOnce() -> Result<Box<DocRun>, RunError>>,
    ) -> Result<(), RunError> {
        let Some(spawn) = launch.take() else {
            return Ok(());
        };
        self.doc_pending.set(false);
        if self.interrupted() || self.root.is_fired() {
            return Ok(());
        }
        let mut doc = spawn()?;
        pipeline_step(b"doc-cargo-spawned", b"");
        doc.scope = self.root.clone();
        if let Some(child) = doc.take_child() {
            attach(&lanes[DOC_LANE], child)?;
        }
        *self.doc.borrow_mut() = Some(doc);
        Ok(())
    }

    pub(crate) fn with_doc(
        &self,
        act: impl FnOnce(&mut DocRun, &DocObservation<'_>) -> Result<(), RunError>,
    ) -> Result<(), RunError> {
        match self.doc.borrow_mut().as_mut() {
            Some(doc) => act(doc, &self.docs),
            None => Ok(()),
        }
    }

    fn doc_tests_finished(&self) -> bool {
        !self.doc_pending.get() && self.doc.borrow().as_ref().is_none_or(|doc| doc.finished())
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
        ledger: &mut OutcomeLedger<'_, RunningChild<NAME_CAPACITY>, LIVE_CAPACITY>,
        watches: &mut Watches,
    ) -> Result<Flow, RunError> {
        let run = self.run;
        match step {
            ListingSourceStep::Launch(intent) => {
                if run.shed(ledger, 1) {
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
                let targets = run.targets.borrow();
                let id = (targets.id(self.target), name.as_bytes());
                emit(run.render(), &Event::Settled { id, outcome: Outcome::Ignored, at_ms: None, solo: false })?;
                drop(targets);
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

const fn cargo_term_color(color: bool) -> &'static [u8] {
    if color { b"CARGO_TERM_COLOR=always" } else { b"CARGO_TERM_COLOR=never" }
}

fn spawn_cargo(
    parser: &CliParser<'_>,
    color: bool,
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
        if color {
            b"--message-format=json-diagnostic-rendered-ansi".as_slice()
        } else {
            b"--message-format=json".as_slice()
        },
    ]);
    if let Some(directory) = directory {
        arguments.push(directory.argument());
    }
    arguments.extend_from_slice(forwarded);
    // cargo's stderr is a pipe, so its own colour choice follows CARGO_TERM_COLOR from the caller's environment, not ours
    let term_color = cargo_term_color(color);
    spawn(b"cargo", &arguments, environment, &[term_color], None).map_err(|_| RunError::Spawn)
}

const METADATA_VALUE_FLAGS: [&[u8]; 6] = [b"--manifest-path", b"-m", b"--config", b"-Z", b"--features", b"-F"];
const METADATA_BOOLEAN_FLAGS: [&[u8]; 5] = [b"--locked", b"--frozen", b"--offline", b"--all-features", b"--no-default-features"];

const HOST_TRIPLE: &str = match option_env!("CARGO_VAL_HOST_TRIPLE") {
    Some(triple) => triple,
    None => "",
};

// the bin set and the compile-unit prediction come from one `cargo metadata` filtered to the platform being built; a spawn failure just leaves build-finished as the signal
fn spawn_metadata(parser: &CliParser<'_>, environment: &[&[u8]], with_deps: bool) -> Result<Child, RunError> {
    let forwarded = parser.cargo_arguments();
    let platform = target_triple(forwarded).unwrap_or(HOST_TRIPLE.as_bytes());
    let mut arguments: Vec<&[u8]> = Vec::from([
        b"metadata".as_slice(),
        b"--format-version=1".as_slice(),
    ]);
    if !with_deps {
        arguments.push(b"--no-deps");
    } else if !platform.is_empty() {
        arguments.push(b"--filter-platform");
        arguments.push(platform);
    }
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
// a workspace with no Cargo.lock yet cannot be resolved under `--locked`, so a failed read retries once as `--no-deps`, which needs no lock and loses only the unit prediction
async fn read_metadata(child: Child, (parser, environment): (&CliParser<'_>, &[&[u8]])) -> Option<Box<MetadataFacts>> {
    if let Some(facts) = scan_metadata(child, parser).await {
        return Some(facts);
    }
    let retry = spawn_metadata(parser, environment, false).ok()?;
    scan_metadata(retry, parser).await
}

async fn scan_metadata(child: Child, parser: &CliParser<'_>) -> Option<Box<MetadataFacts>> {
    // the scanner's tens of KiB of text storage would otherwise sit in every enclosing future
    let mut scan = Box::new(MetadataScan::selecting(parser.cargo_arguments()));
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
    pending_listings: VecDeque<PendingListing>,
    listing_limit: usize,
    cargo_backlog: Option<(Range<u32>, usize)>,
    cargo_eof: bool,
    finishing: bool,
    reason: &'static [u8],
    discovery: DiscoveryFsm,
    parked: ParkedArtifacts,
    listed: Vec<(usize, OwnedArtifact)>,
    dropped: usize,
    lock_watch: LockWatch,
    diagnostics: Vec<u8>,
    agent_report: (Vec<u8>, Vec<u8>),
    workspace_root: PathBuf,
    stderr_held: Vec<u8>,
    diagnostics_reported: bool,
    output: OutputMode,
    built: u16,
    errors: u16,
    seen_diagnostics: HashSet<String>,
    unit_total: Option<(u16, bool)>,
    binaries_built: u16,
    build_ms: u64,
    listed_binaries: usize,
    test_targets: Option<usize>,
}

// the byte count of `pending` that ends on a newline, or all of it once the stream has ended with no newline to come
fn whole_lines_len(pending: &[u8], closed: bool) -> usize {
    if closed {
        return pending.len();
    }
    memchr::memrchr(b'\n', pending).map_or(0, |newline| newline + 1)
}

// a coloured run has cargo wrap "error" in SGR sequences, so the prefix only matches once escapes are skipped
fn visible(line: &[u8]) -> impl Iterator<Item = u8> {
    let mut inside_escape = false;
    line.iter().copied().filter(move |&byte| {
        if inside_escape {
            inside_escape = !byte.is_ascii_alphabetic();
            return false;
        }
        inside_escape = byte == 0x1b;
        !inside_escape
    })
}

fn begins_with_visible(line: &[u8], prefix: &[u8]) -> bool {
    visible(line).take(prefix.len()).eq(prefix.iter().copied())
}

fn begins_build_failure(line: &[u8]) -> bool {
    begins_with_visible(line, BUILD_FAILURE_PREFIX)
}

// both lines restate what the BUILD FAILED line and the diagnostics above it already say
fn is_failure_restatement(line: &[u8]) -> bool {
    begins_build_failure(line) || begins_with_visible(line, BUILD_WAITING_PREFIX)
}

// the lib and lib-test targets compile the same source, so rustc reports one error or warning once per target
fn diagnostic_key((level, rendered): (&str, &str), (file, line, column): (&str, u32, u32)) -> String {
    let headline = rendered.lines().next().unwrap_or_default();
    format!("{level}\u{0}{headline}\u{0}{file}:{line}:{column}")
}

// agent output has never carried rustc's failure-note, so only the other modes admit it
fn admit_diagnostic(seen: &mut HashSet<String>, agent_like: bool, message: (&str, &str), position: (&str, u32, u32)) -> bool {
    if message.0 == "failure-note" && agent_like {
        return false;
    }
    seen.insert(diagnostic_key(message, position))
}

// the live frame learns of the first error while cargo is still building, so its build bar can turn red
fn count_error(errors: &mut u16, level: &str, run: &ListingRun<'_>) -> Result<(), RunError> {
    if level != "error" {
        return Ok(());
    }
    *errors = errors.saturating_add(1);
    emit_live(run.render(), &Event::Failing { errors: *errors })
}

// the review's warnings count: one per distinct compiler warning, shown nowhere else until the closing block
fn count_warning(level: &str, rendered: &str, run: &ListingRun<'_>) -> Result<(), RunError> {
    if level != "warning" {
        return Ok(());
    }
    let headline = rendered.lines().next().unwrap_or_default();
    emit_live(run.render(), &Event::Warned { id: (&b"build"[..], headline.as_bytes()), what: b"compiler warning" })
}

fn keep_reported(held: &mut Vec<u8>, (settings, root): (&Settings, &Path), (level, rendered): (&str, &str), span: Option<(&str, u32, u32, u32, Option<&str>)>) {
    if level == "error" && settings.output == OutputMode::Rich {
        keep_rich_error(held, settings, root, rendered, span);
    } else {
        keep_diagnostic(held, level, rendered);
    }
}

const CARGO_PROGRESS: [&[u8]; 5] = [b"Compiling ", b"Finished ", b"Running ", b"Fresh ", b"Doc-tests "];

fn is_cargo_progress(line: &[u8]) -> bool {
    let trimmed = line.trim_ascii_start();
    CARGO_PROGRESS.iter().any(|prefix| trimmed.starts_with(prefix))
}

fn keep_for_mode(output: OutputMode, (held, report): (&mut Vec<u8>, &mut (Vec<u8>, Vec<u8>)), context: (&Settings, &Path), (level, rendered, package_id): (&str, &str, Option<&str>), span: Option<(&str, u32, u32, u32, Option<&str>)>) {
    if output == OutputMode::Agent {
        keep_agent_error(report, level, package_id, rendered, span);
    } else {
        keep_reported(held, context, (level, rendered), span);
    }
}

// an agent build failure is one block per rustc error and a rollup line for each, both built where the rendered text is still in hand
fn keep_agent_error((blocks, rollup): &mut (Vec<u8>, Vec<u8>), level: &str, package_id: Option<&str>, rendered: &str, span: Option<(&str, u32, u32, u32, Option<&str>)>) {
    if level != "error" {
        return;
    }
    let package = package_id.map_or("build", package_name);
    let headline = rendered.lines().next().unwrap_or_default();
    let site = span.map_or_else(|| String::from("absent"), |(file, line, column, ..)| format!("{file}:{line}:{column}"));
    blocks.extend_from_slice(format!("ERROR    {package}  {site}\n         {headline}\n").as_bytes());
    if let Some(label) = span.and_then(|(.., label)| label).filter(|label| !label.is_empty()) {
        blocks.extend_from_slice(format!("         {label}\n").as_bytes());
    }
    let line = span.map_or_else(|| String::from("absent"), |(file, line, ..)| format!("{file}:{line}"));
    rollup.extend_from_slice(format!("FAILED   {package} - {line}  {headline}\n").as_bytes());
}

// `path+file:///repo/parser#0.3.1` names its package by the directory, `registry+...#serde@1.0.228` and newer path ids by the fragment
fn package_name(package_id: &str) -> &str {
    let (source, fragment) = package_id.split_once('#').unwrap_or((package_id, ""));
    match fragment.split_once('@') {
        Some((name, _)) => name,
        None => source.rsplit('/').next().unwrap_or(source),
    }
}

fn keep_diagnostic(held: &mut Vec<u8>, level: &str, rendered: &str) {
    let kept = match level {
        "error" | "failure-note" => true,
        "warning" => held.len() < DIAGNOSTIC_CAPACITY,
        _ => false,
    };
    if kept {
        held.extend_from_slice(rendered.as_bytes());
    }
}

fn absolute_directory(root: &Path) -> String {
    let root = if root.as_os_str().is_empty() { Path::new(".") } else { root };
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf()).to_string_lossy().into_owned()
}

fn linked_location(settings: &Settings, root: &Path, rendered: &str, (file, line, column): (&str, u32, u32)) -> Option<String> {
    let template = settings.link_template.as_deref()?;
    let range = site_in(rendered.as_bytes(), (file.as_bytes(), line, column))?;
    let mut wrapped = Line::<1024>::new();
    let position = (usize::try_from(line).unwrap_or(0), usize::try_from(column).unwrap_or(0));
    write_link(&mut wrapped, true, template.as_bytes(), absolute_directory(root).as_bytes(), file.as_bytes(), position, &rendered.as_bytes()[range.clone()]);
    Some(format!("{}{}{}", &rendered[..range.start], String::from_utf8_lossy(wrapped.bytes()), &rendered[range.end..]))
}

fn keep_rich_error(held: &mut Vec<u8>, settings: &Settings, root: &Path, rendered: &str, span: Option<(&str, u32, u32, u32, Option<&str>)>) {
    let colour = settings.color && colour_allowed();
    let Some(located) = span else {
        return keep_diagnostic(held, "error", rendered);
    };
    let linked = linked_location(settings, root, rendered, (located.0, located.1, located.2));
    let rendered = linked.as_deref().unwrap_or(rendered);
    match for_span(root, colour, located) {
        Ok(rows) if splice_excerpt(held, rendered.as_bytes(), &rows) => {}
        Ok(rows) => {
            proxima_telemetry::debug!(file = %located.0, "rustc snippet block not recognised, diagnostic kept verbatim with the excerpt appended");
            keep_diagnostic(held, "error", rendered);
            held.extend_from_slice(&rows);
        }
        Err(reason) => {
            proxima_telemetry::debug!(file = %located.0, reason = %String::from_utf8_lossy(&reason), "no excerpt for the diagnostic span");
            keep_diagnostic(held, "error", rendered);
        }
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

// the up-front count of test binaries the run's flags select, which the end-to-end tests compare with the binaries actually listed
fn test_binaries_debug(enabled: bool, test_targets: Option<usize>) {
    if !enabled {
        return;
    }
    let mut line = Line::<64>::new();
    line.text(b"cargo-val: debug: metadata test-binaries=");
    match test_targets {
        Some(found) => line.number(found),
        None => line.text(b"unavailable"),
    }
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

// the total never trails the count: a unit past the expected total raises it and drops the exactness
fn announce_built(render: &Render<'_>, (built, binaries): (u16, u16), unit_total: Option<(u16, bool)>) -> Result<(), RunError> {
    let (total, exact) = unit_total.map_or((None, false), |(total, exact)| (Some(total.max(built)), exact && built <= total));
    emit_live(render, &Event::Built { done: built, total, exact, binaries, at_ms: monotonic_millis() })
}

fn record_build_script(linked: &mut LinkedDirectories<'_>, build_scripts: &mut BuildScriptOutputs<'_>, script: &BuildScript<'_>) -> Result<(), RunError> {
    for reported in script.linked_paths.iter() {
        linked.add(reported).map_err(|_| RunError::TestEnvironment)?;
    }
    build_scripts.add(script).map_err(|_| RunError::TestEnvironment)
}

fn note_built(built: &mut u16, binaries: &mut u16, unit_total: Option<(u16, bool)>, (run, test_binary): (&ListingRun<'_>, bool)) -> Result<(), RunError> {
    *built = built.saturating_add(1);
    *binaries = binaries.saturating_add(u16::from(test_binary));
    announce_built(run.render(), (*built, *binaries), unit_total)
}

impl CargoDriver<'_> {
    fn drained(&self, unaccounted: usize) -> bool {
        self.discovery.drained(unaccounted, self.parked.is_empty())
            && self.active.is_none()
            && self.pending_listings.is_empty()
    }

    /// Launches each failure the review marked RERUN that has no child yet, as a solo exact-test child through [`spawn_exact`],
    /// the same spawn the run's own tests use; a full job table leaves the row marked until a child settles.
    pub(crate) fn launch_reruns(&mut self, lanes: &Lanes, run: &ListingRun<'_>, ledger: &mut Ledger<'_>, watches: &mut Watches) -> Result<(), RunError> {
        let state = run.render().1.borrow();
        let wanted: Vec<(Vec<u8>, Vec<u8>)> = state.rerunning().map(|(binary, name)| (binary.to_vec(), name.to_vec())).collect();
        drop(state);
        quit_step(b"rerun-wanted", wanted.len());
        for (binary, name) in &wanted {
            let id = (binary.as_slice(), name.as_slice());
            if ledger.sessions().len() >= run.settings.jobs {
                break;
            }
            if !rerun_live(run, ledger, watches, id) {
                self.launch_rerun(lanes, run, (ledger, watches), id).or_else(|error| refuse_rerun(run, id, error))?;
            }
        }
        Ok(())
    }

    fn launch_rerun(&mut self, lanes: &Lanes, run: &ListingRun<'_>, (ledger, watches): (&mut Ledger<'_>, &mut Watches), id: (&[u8], &[u8])) -> Result<(), RunError> {
        let target = {
            let targets = run.targets.borrow();
            (0..targets.len()).find(|index| targets.id(*index) == id.0).ok_or(RunError::Outcome)?
        };
        let (_, owned) = self.listed.iter().find(|(listed, _)| *listed == target).ok_or(RunError::Outcome)?;
        let artifact = owned.as_artifact();
        let environment = &mut self.environments[slot_range(free_slot(self.active.as_ref(), &self.pending_listings))];
        let input = TestEnvironmentInput {
            artifact,
            linked_directories: self.linked.as_str(),
            library_directories: run.library_directories,
            inherited_dylib_path: run.inherited_dylib_path,
            home: run.home,
            build_script: self.build_scripts.find(artifact.package_id),
        };
        let layout = TestEnvironmentLayout::build(&input, environment).map_err(|_| RunError::TestEnvironment)?;
        let name = core::str::from_utf8(id.1).map_err(|_| RunError::Arguments)?;
        let scan = ListingScan { run, lanes, executable: artifact.executable, target, environment: &*environment, layout: &layout };
        let intent = ExactTestIntent::new(ExecutablePath::Borrowed(artifact.executable), name);
        let spawned = spawn_exact(&scan, ledger, watches, intent, Some(monotonic_millis())).map(|_| ());
        quit_step(&[b"rerun-spawned ".as_slice(), id.1].concat(), usize::from(spawned.is_ok()));
        spawned
    }

    pub(crate) fn finished(&self, run: &ListingRun<'_>, ledger: &Ledger<'_>) -> bool {
        self.drained(ledger.unaccounted()) && run.doc_tests_finished()
    }

    fn has_room(&self) -> bool {
        listings_in_flight(self.active.as_ref(), &self.pending_listings) < self.listing_limit
    }

    // cargo's stdout is read while an artifact has somewhere to wait, so a build never stalls on a full pipe behind running tests
    fn accepting(&self) -> bool {
        self.has_room() || !self.parked.is_full()
    }

    pub(crate) fn cargo_open(&self) -> bool {
        self.accepting() && self.cargo_backlog.is_none()
    }

    pub(crate) fn listing_open(&self, run: &ListingRun<'_>, ledger: &Ledger<'_>) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.ignoring || (active.backlog.is_none() && listing_wanted(run, ledger))
        })
    }

    fn queue_listing(
        &mut self,
        started: Option<PendingListing>,
        kept: &OwnedArtifact,
        lanes: &Lanes,
    ) -> Result<(), RunError> {
        let Some(next) = started else {
            return Ok(());
        };
        self.listed.push((next.target, *kept));
        if self.active.is_some() {
            self.pending_listings.push_back(next);
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
                free_slot(self.active.as_ref(), &self.pending_listings),
                &mut *self.environments,
            )?;
            let kept = OwnedArtifact::copy_of(&artifact).map_err(RunError::Parked)?;
            self.queue_listing(started, &kept, lanes)?;
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

    fn build_finished(
        &mut self,
        success: bool,
        (run, ledger, lanes): (&ListingRun<'_>, &Ledger<'_>, &Lanes),
    ) -> Result<(), RunError> {
        self.build_ms = monotonic_millis().saturating_sub(run.started_millis);
        pipeline_step(if success { b"build-finished-ok" } else { b"build-finished-failed" }, b"");
        emit_live(run.render(), &Event::Built { done: self.built, total: Some(self.built), exact: true, binaries: self.binaries_built, at_ms: monotonic_millis() })?;
        let (event, reason): (&[u8], &'static [u8]) = if success {
            (b" build-finished success=true", b"build-succeeded")
        } else {
            (b" build-finished success=false", b"build-failed")
        };
        debug_build_end(run.settings.debug, event, self.parked.len());
        self.discovery.build_finished(success);
        if !success && !self.output.is_agent_like() {
            self.announce_failure(run.render())?;
        }
        self.report_diagnostics(success, run.render())?;
        self.finishing = true;
        self.reason = reason;
        self.release_parked(run, ledger, lanes)
    }

    // erases the live frame once and ends it: the BUILD FAILED line is what the diagnostics, SUMMARY and EXIT follow
    fn announce_failure(&self, render: &Render<'_>) -> Result<(), RunError> {
        let of = self.test_targets.map(|found| found.max(usize::from(self.binaries_built)));
        let exit = u8::try_from(BUILD_FAILED_EXIT_CODE).unwrap_or(u8::MAX);
        emit(
            render,
            &Event::BuildFailed {
                diagnostic: b"",
                exit,
                errors: self.errors,
                built: self.binaries_built,
                of: of.and_then(|total| u16::try_from(total).ok()),
            },
        )
    }

    fn note_listed(&mut self, run: &ListingRun<'_>) -> Result<(), RunError> {
        self.listed_binaries += 1;
        let total = self.test_targets.map(|found| found.max(self.listed_binaries));
        let tests = run.targets.borrow().total().discovered;
        emit_live(run.render(), &Event::Listed { binaries: self.listed_binaries, of: total, tests })
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
            if !self.accepting() {
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
                    pipeline_step(b"artifact-arrived", artifact.target_name.as_bytes());
                    note_built(&mut self.built, &mut self.binaries_built, self.unit_total, (run, true))?;
                    let gated = matches!(artifact.target_kind, "test" | "bench")
                        && self.discovery.waits(artifact.package_id);
                    let crowded = !gated && listings_in_flight(self.active.as_ref(), &self.pending_listings) >= self.listing_limit;
                    if crowded {
                        self.reason = b"listing-room";
                    }
                    debug_artifact(run.settings.debug, gated || crowded, self.discovery.gate(), &artifact);
                    if gated || crowded {
                        self.parked.park(&artifact).map_err(RunError::Parked)?;
                    } else {
                        let started = start_listing(
                            run,
                            ledger,
                            &artifact,
                            &self.linked,
                            &self.build_scripts,
                            free_slot(self.active.as_ref(), &self.pending_listings),
                            &mut *self.environments,
                        )?;
                        let kept = OwnedArtifact::copy_of(&artifact).map_err(RunError::Parked)?;
                        self.queue_listing(started, &kept, lanes)?;
                    }
                }
                CargoSourceStep::Compiled { .. } => note_built(&mut self.built, &mut self.binaries_built, self.unit_total, (run, false))?,
                CargoSourceStep::BinBuilt {
                    package_id,
                    executable,
                } => {
                    note_built(&mut self.built, &mut self.binaries_built, self.unit_total, (run, false))?;
                    debug_bin_built(run.settings.debug, executable, package_id);
                    self.discovery.bin_built(package_id);
                    self.reason = b"bin-built";
                    self.release_parked(run, ledger, lanes)?;
                }
                CargoSourceStep::Diagnostic { level, rendered, span, package_id } => {
                    let position = span.map_or(("", 0, 0), |(file, line, column, ..)| (file, line, column));
                    if admit_diagnostic(&mut self.seen_diagnostics, self.output.is_agent_like(), (level, rendered), position) {
                        keep_for_mode(self.output, (&mut self.diagnostics, &mut self.agent_report), (run.settings, &self.workspace_root), (level, rendered, package_id), span);
                        count_error(&mut self.errors, level, run)?;
                        count_warning(level, rendered, run)?;
                    }
                }
                CargoSourceStep::BuildFinished { success } => self.build_finished(success, (run, ledger, lanes))?,
                CargoSourceStep::BuildScript(script) => record_build_script(&mut self.linked, &mut self.build_scripts, &script)?,
                CargoSourceStep::Read { .. }
                | CargoSourceStep::NeedMore
                | CargoSourceStep::Ignored => {}
                CargoSourceStep::Eof => {
                    self.discovery.stdout_eof();
                    self.report_diagnostics(false, run.render())?;
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

    fn report_diagnostics(&mut self, success: bool, render: &Render<'_>) -> Result<(), RunError> {
        let held = core::mem::take(&mut self.diagnostics);
        let (mut blocks, rollup) = core::mem::take(&mut self.agent_report);
        if !success && !blocks.is_empty() {
            blocks.extend_from_slice(&rollup);
            emit(render, &Event::BuildFailed { diagnostic: &blocks, exit: u8::try_from(BUILD_FAILED_EXIT_CODE).unwrap_or(u8::MAX), errors: self.errors, built: self.binaries_built, of: None })?;
        }
        if success {
            hold_warnings(render, &held);
        }
        if !success && !held.is_empty() {
            erase_live(render)?;
            write_fd(2, &held)?;
            if held.len() >= DIAGNOSTIC_CAPACITY {
                write_fd(2, b"cargo-val: compiler diagnostics were truncated\n")?;
            }
        }
        self.diagnostics_reported = true;
        let withheld = core::mem::take(&mut self.stderr_held);
        self.emit_whole_lines(&withheld, render)
    }

    fn emit_stderr(&mut self, bytes: &[u8], render: &Render<'_>) -> Result<(), RunError> {
        let restated = self.errors != 0 && self.output != OutputMode::Json;
        for line in bytes.split_inclusive(|byte| *byte == b'\n').filter(|line| !(restated && is_failure_restatement(line))) {
            if render.0 == OutputMode::Rich {
                summarize_status_lines(line, render)?;
            } else if !self.output.is_agent_like() || !is_cargo_progress(line) {
                write_fd(2, line)?;
            }
        }
        announce_lock_wait(&mut self.lock_watch, bytes, render)
    }

    // cargo writes one line in several pieces, so a piece written whole would let cargo-val's own lines land inside it
    fn emit_whole_lines(&mut self, bytes: &[u8], render: &Render<'_>) -> Result<(), RunError> {
        self.stderr_held.extend_from_slice(bytes);
        let pending = core::mem::take(&mut self.stderr_held);
        let end = whole_lines_len(&pending, self.discovery.stderr_closed());
        let emitted = if end == 0 { Ok(()) } else { self.emit_stderr(&pending[..end], render) };
        self.stderr_held = pending;
        self.stderr_held.drain(..end);
        emitted
    }

    pub(crate) fn forward_stderr(&mut self, bytes: &[u8], render: &Render<'_>) -> Result<(), RunError> {
        if self.diagnostics_reported {
            return self.emit_whole_lines(bytes, render);
        }
        self.stderr_held.extend_from_slice(bytes);
        if begins_build_failure(&self.stderr_held) {
            return Ok(());
        }
        self.release_ahead_of_failure(render)
    }

    // cargo's failure summary must follow rustc's rendered diagnostics, which arrive on the other pipe; lines before it stream through.
    fn release_ahead_of_failure(&mut self, render: &Render<'_>) -> Result<(), RunError> {
        let pending = core::mem::take(&mut self.stderr_held);
        let mut start = 0;
        while let Some(newline) = memchr::memchr(b'\n', &pending[start..]) {
            let end = start + newline + 1;
            if begins_build_failure(&pending[start..end]) {
                break;
            }
            self.emit_stderr(&pending[start..end], render)?;
            start = end;
        }
        self.stderr_held = pending;
        self.stderr_held.drain(..start);
        Ok(())
    }

    pub(crate) fn cargo_exited(&mut self, code: i32) {
        quit_step(b"build-cargo-exited", self.process_id as usize);
        release_child(self.process_id);
        self.discovery.exited(code);
    }

    pub(crate) fn stderr_ended(&mut self, render: &Render<'_>) -> Result<(), RunError> {
        self.discovery.stderr_eof();
        if self.diagnostics_reported {
            return self.emit_whole_lines(&[], render);
        }
        Ok(())
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
            if matches!(scan.apply(step, ledger, watches)?, Flow::Pause) {
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
            match scan.apply(step, ledger, watches)? {
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
            && self.accepting()
        {
            self.cargo_backlog = None;
            self.cargo_chunk(lanes, range, offset, run, ledger)?;
            return Ok(self.cargo_backlog.as_ref().is_none_or(|(_, moved)| *moved > offset));
        }
        if self.cargo_eof && !self.discovery.stdout_closed() && self.cargo_backlog.is_none() && self.accepting() {
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
        self.note_listed(run)?;
        if let Some(next) = self.pending_listings.pop_front() {
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


// the one arming site of the parent watch: a run that did not opt in has no extra arm to poll
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn watch_parent(signals: &SignalPipe, setting: ExitWithParent) -> Result<(), RunError> {
    match setting {
        ExitWithParent::On => signals.watch_parent().map_err(|_| RunError::Reactor),
        ExitWithParent::Off => Ok(()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn watch_parent(_signals: &SignalPipe, setting: ExitWithParent) -> Result<(), RunError> {
    match setting {
        ExitWithParent::On => Err(RunError::Settings),
        ExitWithParent::Off => Ok(()),
    }
}

fn emit_begin(render: &Render<'_>, settings: &Settings, environment: &Environment, at_ms: u64) -> Result<(), RunError> {
    emit(
        render,
        &Event::Begin {
            version: env!("CARGO_PKG_VERSION").as_bytes(),
            shard: settings.process == ProcessMode::Shard,
            jobs: settings.jobs,
            github: environment.get(b"GITHUB_ACTIONS") == Some(b"true".as_slice()),
            why: settings.output.is_agent_like().then_some(settings.output_why.as_bytes()),
            json: settings.output == OutputMode::Json,
            at_ms,
        },
    )
}

const fn explicit_root(mode: TargetDirMode<'_>) -> Option<&[u8]> {
    match mode {
        TargetDirMode::Path(root) => Some(root),
        TargetDirMode::Isolated | TargetDirMode::Shared => None,
    }
}

// edge read: runs once after the target directory is known, before any test starts; the record reaches the state as `Event::Previous`
fn open_last_run(
    render: &Render<'_>,
    settings: &Settings,
    location: &mut Located,
    (sources, fingerprint): ([Option<&[u8]>; 3], u64),
) -> Result<Option<(u16, bool)>, RunError> {
    location.0 = last_run::target_directory(sources);
    location.1[0] = fingerprint;
    let Some((wall_ms, tests, _, recorded, units, build_ms)) = location.0.as_deref().and_then(last_run::read) else {
        return Ok(None);
    };
    let units = u16::try_from(units).unwrap_or(u16::MAX);
    let matched = fingerprint != 0 && recorded == fingerprint;
    emit(render, &Event::Previous { wall_ms, tests, build: (units, build_ms), matched })?;
    if let (true, Some((wall_ms, tests))) = (settings.debug, render.1.borrow().previous()) {
        let mut line = Line::<256>::new();
        line.text(b"cargo-val: debug: last-run previous wall_ms=");
        line.number(usize::try_from(wall_ms).unwrap_or(usize::MAX));
        line.text(b" tests=");
        line.number(usize::try_from(tests).unwrap_or(usize::MAX));
        line.text(b"\n");
        let _ = line.write(2);
    }
    if settings.debug {
        let mut line = Line::<64>::new();
        line.text(b"cargo-val: debug: eta fingerprint-matched=");
        line.text(if matched { b"yes" } else { b"no" });
        line.text(b"\n");
        let _ = line.write(2);
    }
    Ok(matched.then_some((units, true)))
}

// only a run that produced a summary records; build failures, refusals and interrupts keep the earlier record
fn last_run_record(wall_ms: u64, [fingerprint, units, build_ms]: [u64; 3], outcome: &Result<RunSummary, RunError>) -> Option<last_run::Record> {
    match outcome {
        Ok(summary) => Some((wall_ms, u32::try_from(summary.discovered).unwrap_or(u32::MAX), exit_byte(summary.exit_code()), fingerprint, u32::try_from(units).unwrap_or(u32::MAX), build_ms)),
        Err(_) => None,
    }
}

fn exit_byte(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(u8::MAX)
}

// edge write: runs once when the run is over, outside the event loop
fn persist_last_run(location: &Located, wall_ms: u64, outcome: &Result<RunSummary, RunError>) {
    let (Some(target), Some(record)) = (location.0.as_deref(), last_run_record(wall_ms, location.1, outcome)) else {
        return;
    };
    quit_step(b"record-write-start", 0);
    if let Err(error) = last_run::write(target, record) {
        proxima_telemetry::warn!(error = %error, path = %target.display(), "last-run record not written");
    }
    quit_step(b"record-write-end", 0);
}

fn begin_rich(render: &Render<'_>, mouse: bool) -> Result<(), RunError> {
    if !crate::edge_output::enable_virtual_terminal(1) {
        return Err(RunError::Settings);
    }
    if crate::edge_output::is_terminal(0) && crate::edge_output::enter_raw_mode(0) {
        KEYS_RAW.store(true, Ordering::SeqCst);
    }
    render.1.borrow_mut().set_mouse(mouse && keys_raw());
    render.1.borrow_mut().set_keys_live(keys_raw());
    emit(render, &Event::Resize { cols: crate::edge_output::columns(1).unwrap_or(80) })
}

/// Runs the whole cargo-val run: setup, the awaited prime loop, and the summary.
///
/// # Errors
///
/// Returns [`RunError`] naming the stage that failed.
pub(crate) async fn run_body(parser: &CliParser<'_>) -> Result<RunSummary, RunError> {
    let started_millis = monotonic_millis();
    let mut location: Located = (None, [0; 3]);
    let outcome = run_located(parser, started_millis, &mut location).await;
    quit_step(b"store-closed", 0);
    persist_last_run(&location, monotonic_millis().saturating_sub(started_millis), &outcome);
    outcome
}

/// Where the last-run record lives and what this run adds to it: `[fingerprint, units, build_ms]`.
type Located = (Option<PathBuf>, [u64; 3]);

async fn run_located(parser: &CliParser<'_>, started_millis: u64, location: &mut Located) -> Result<RunSummary, RunError> {
    let harness = validated_harness(parser)?;
    let environment = unsafe { Environment::current() };
    let settings = runtime_settings(&environment, &harness, parser)?;
    let mut storage = run_storage(settings.process, timeline_arena_bytes(settings.output, parser.timeline()));
    let storage = &mut storage;
    let (cargo_storage, (libdir_bytes, inherited), (sessions, watches), (shard_names, sched_bytes, picked), (name_arena, ledger_slots, frame_line, inflight_slots, timeline_slots, arrival_slots), (read_storage, review_window)) = storage;
    let link_directory = absolute_directory(&settings.source_root);
    let link_template = settings.link_template.as_deref().unwrap_or("");
    let render: Render<'_> = (settings.output, RefCell::new(RenderState::new(name_arena, ledger_slots).with_inflight(inflight_slots).with_timeline(timeline_slots).with_arrivals(arrival_slots).with_window(review_window).with_heartbeat_ms(settings.heartbeat_ms).with_color(settings.color).with_debug(settings.debug).with_links(link_template.as_bytes(), link_directory.as_bytes())), RefCell::new(frame_line), RefCell::new(None));
    // the size must be known before the Begin frame, or that frame is drawn at the 80 column default
    if settings.output == OutputMode::Rich {
        begin_rich(&render, settings.mouse == Some(true))?;
    }
    emit_begin(&render, &settings, &environment, started_millis)?;
    let inherited_count = environment.collect(inherited).map_err(|_| RunError::Environment)?;
    let inherited = &inherited[..inherited_count];
    let configured_target = utf8_variable(&environment, b"CARGO_VAL_TARGET_DIR")?;
    let target_pairs = [("CARGO_VAL_TARGET_DIR", configured_target.unwrap_or(""))];
    let target_source = BorrowedSettings::new(&target_pairs[..usize::from(configured_target.is_some())]);
    let target_mode = resolve_target_dir(&target_source, parser)?;
    if parser.doc_selection() == DocSelection::Only {
        return run_doc_only(parser, (&settings, &render), target_mode, (started_millis, location), (inherited, utf8_variable(&environment, b"CARGO_TARGET_DIR")?)).await;
    }
    let queries = start_libdir_queries(&environment, parser, inherited)?;
    let inherited_dylib_path = utf8_variable(&environment, DYLIB_PATH_KEY.as_bytes())?;
    let home = utf8_variable(&environment, b"HOME")?;
    let filter = filter_text(parser)?;
    let signals = SignalPipe::install().map_err(|_| RunError::Reactor)?;
    watch_parent(&signals, settings.exit_with_parent)?;
    let StartedChildren {
        cargo: cargo_child,
        directories,
        bins,
        gate,
        test_targets,
        eta,
        lock_watch,
        metadata_directory,
        workspace_root,
    } = start_children(parser, &settings, target_mode, (inherited, environment.get(b"RUSTC"))).await?;
    let units = open_units(&render, &settings, location, [explicit_root(target_mode), utf8_variable(&environment, b"CARGO_TARGET_DIR")?.map(str::as_bytes), metadata_directory.as_deref()], eta)?;
    let shared_directory = lock_watch.directory().to_vec();
    let mut driver = new_driver(cargo_storage, cargo_child.process_id(), (bins, gate, test_targets), lock_watch, (workspace_root, settings.output, settings.jobs))?;
    announce_start(&render, &mut driver, units)?;
    let mut ledger = OutcomeLedger::new(sessions).map_err(|_| RunError::Outcome)?;
    let (library_directories, directory_count) = resolve_libdirs(queries, libdir_bytes)
        .await
        .inspect_err(|_| cargo_child.terminate())?;
    let prepared = held(settings.filterset.as_ref().map(Filterset::prepare));
    let doc_wanted = parser.doc_selection() == DocSelection::Included;
    let root = Signal::new();
    let store = open_store(&render, location.0.as_deref());
    let run = held(ListingRun {
        signals: &signals,
        store: Rc::clone(&store),
        reads: RefCell::new(read_storage), cursor: Cell::new([0; 5]), head: Cell::new(0),
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
        docs: DocObservation::new(&render),
        policy: RefCell::new(StopPolicy::new(settings.max_fail)),
        doc: RefCell::new(None),
        doc_pending: Cell::new(doc_wanted),
        launch_storage: RefCell::new(held_launch_storage()),
        ready: Cell::new(None),
        shards: RefCell::new(ShardArena::with_declared(shard_names, DECLARED_BYTES)),
        sched: RefCell::new(SchedTable::new(sched_bytes)),
        specials: RefCell::new(SpecialPlan::new(picked)),
        sched_target: Cell::new(None),
            interrupt_reported: Cell::new(0), interrupt_not_run: Cell::new(0), quit: Cell::new(None),
            editing: RefCell::new(None), editor_program: RefCell::new(Vec::new()), request: RefCell::new(None), outside: RefCell::new(Vec::new()), detached_running: Cell::new(false), probe: RefCell::new(None), probe_site: RefCell::new((Vec::new(), 0, 0)), probe_output: RefCell::new((Vec::new(), None, false)),
            trip: root.child(),
            root,
    });
    let mut launch_doc = doc_wanted.then_some(|| {
        let mut doc = spawn_doc_tests(parser, (settings.ignore, settings.output), inherited, directories.as_ref().map(|chosen| &chosen.doc))?;
        if directories.is_none() {
            doc.lock_watch.set_directory(&shared_directory);
        }
        Ok(doc)
    });
    observe_cargo(cargo_child, &mut driver, &run, &mut ledger, watches, &mut launch_doc).await?;
    location.1 = [eta.0, u64::from(driver.built), driver.build_ms];
    if driver.discovery.lost_artifacts_on_clean_exit(driver.dropped) {
        return Err(RunError::CargoStream);
    }
    let summary = finish_run(parser, &run, driver.discovery.exit_code(), driver.discovery.build_reported())?;
    Ok(summary)
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
    (Box<[u8]>, Box<[u32]>, Box<Line<FRAME_BYTES>>, Box<[u8]>, Box<[u8]>, Box<[u32]>),
    (Box<[u8]>, Box<[u8]>),
);

// run-sized fixed storage too large for any stack frame or future, allocated straight on the heap once at setup: the cargo and listing buffers, the session tables, and the render frame, failure ledger and name arena
fn run_storage<'environment>(process: ProcessMode, timeline_bytes: usize) -> RunStorage<'environment> {
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
        (zeroed(crate::view_defaults::NAME_BYTES), std::vec![0; FAILURES].into_boxed_slice(), zeroed_frame(), zeroed(crate::view_defaults::INFLIGHT_BYTES), zeroed(timeline_bytes), std::vec![0; 2 * crate::render::ARRIVALS].into_boxed_slice()),
        (zeroed(2 * WINDOW_BYTES + PAGE_BYTES), zeroed(WINDOW_BYTES)),
    )
}

// the review reads its windows from the same store the sessions seal into
fn open_store(render: &Render<'_>, target: Option<&Path>) -> Rc<edge_store::Store> {
    let store = Rc::new(edge_store::open(target));
    *render.3.borrow_mut() = Some(Rc::clone(&store));
    status_queue::spill_into(Rc::clone(&store));
    store
}

// rich keeps the arena for the review `s` key; plain only when --timeline asked for the view
const fn timeline_arena_bytes(output: OutputMode, requested: bool) -> usize {
    match (output, requested) {
        (OutputMode::Rich, _) | (OutputMode::Plain, true) => crate::view_defaults::TIMELINE_BYTES,
        _ => 0,
    }
}

fn zeroed(length: usize) -> Box<[u8]> {
    std::vec![0; length].into_boxed_slice()
}

// an all-zero Line is Line::new(); allocating it zeroed puts the frame buffer on the heap with no stack temporary
fn zeroed_frame() -> Box<Line<FRAME_BYTES>> {
    unsafe { std::boxed::Box::<Line<FRAME_BYTES>>::new_zeroed().assume_init() }
}

// the driver and the run own tens of KiB each of fixed parking and target tables, too large to sit in the root future
fn held<Value>(value: Value) -> std::boxed::Box<Value> {
    std::boxed::Box::new(value)
}

fn new_driver(
    (frame, scratch, linked, scripts, listing_frame, ignored_frame, environments, ignored): &mut CargoStorage,
    process_id: u32,
    (bins, gate, test_targets): (BinCounts, BinGate, Option<usize>),
    lock_watch: LockWatch,
    (workspace_root, output, jobs): (PathBuf, OutputMode, usize),
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
        pending_listings: VecDeque::with_capacity(LISTING_SLOTS),
        listing_limit: listing_limit(jobs),
        cargo_backlog: None,
        cargo_eof: false,
        finishing: false,
        reason: b"",
        discovery: DiscoveryFsm::new(bins, gate),
        parked: ParkedArtifacts::new(),
        listed: Vec::new(),
        dropped: 0,
        lock_watch,
        diagnostics: Vec::new(),
        agent_report: (Vec::new(), Vec::new()),
        workspace_root,
        stderr_held: Vec::new(),
        diagnostics_reported: false,
        output,
        built: 0,
        errors: 0,
        seen_diagnostics: HashSet::new(),
        unit_total: None,
        binaries_built: 0,
        build_ms: 0,
        listed_binaries: 0,
        test_targets,
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
    directories: Option<BuildDirectories>,
    bins: BinCounts,
    gate: BinGate,
    test_targets: Option<usize>,
    eta: (u64, Option<u16>),
    lock_watch: LockWatch,
    metadata_directory: Option<Box<[u8]>>,
    workspace_root: PathBuf,
}

// the unit total the first frame shows: the recorded exact count when the fingerprint matched, else the metadata prediction, else none
fn open_units(render: &Render<'_>, settings: &Settings, location: &mut Located, sources: [Option<&[u8]>; 3], (fingerprint, predicted): (u64, Option<u16>)) -> Result<Option<(u16, bool)>, RunError> {
    let expected = open_last_run(render, settings, location, (sources, fingerprint))?;
    if settings.debug {
        let mut line = Line::<64>::new();
        line.text(b"cargo-val: debug: eta units predicted=");
        match predicted {
            Some(total) => line.number(usize::from(total)),
            None => line.text(b"unavailable"),
        }
        line.text(b"\n");
        let _ = line.write(2);
    }
    Ok(expected.or(predicted.map(|total| (total, false))))
}

fn announce_start(render: &Render<'_>, driver: &mut CargoDriver<'_>, units: Option<(u16, bool)>) -> Result<(), RunError> {
    driver.unit_total = units;
    emit_live(render, &Event::Listed { binaries: 0, of: driver.test_targets, tests: 0 })?;
    announce_built(render, (0, 0), units)
}

// edge read: the bytes that decide a build's unit set; zero means the metadata was unavailable, so no record can match
fn build_fingerprint(facts: Option<&MetadataFacts>, parser: &CliParser<'_>, toolchain: &[u8]) -> u64 {
    let Some(found) = facts else {
        return 0;
    };
    let root = found.workspace_root().and_then(|bytes| core::str::from_utf8(bytes).ok()).map(Path::new);
    let lock = root.map(|directory| std::fs::read(directory.join("Cargo.lock")).unwrap_or_default());
    let manifests: Vec<Vec<u8>> = found
        .bins
        .member_directories()
        .filter_map(|bytes| core::str::from_utf8(bytes).ok())
        .map(|directory| std::fs::read(Path::new(directory).join("Cargo.toml")).unwrap_or_default())
        .collect();
    let arguments = parser.cargo_arguments().join(&0_u8);
    let mut inputs: Vec<&[u8]> = Vec::from([lock.as_deref().unwrap_or_default(), toolchain, b"test".as_slice(), arguments.as_slice()]);
    inputs.extend(manifests.iter().map(Vec::as_slice));
    last_run::fingerprint(&inputs)
}

// the toolchain's identity is `rustc -vV`; a rustc that will not run leaves it empty, which still fingerprints the rest
fn read_toolchain(rustc: Option<&[u8]>) -> Vec<u8> {
    let program = rustc.and_then(|bytes| core::str::from_utf8(bytes).ok()).unwrap_or("rustc");
    match std::process::Command::new(program).arg("-vV").output() {
        Ok(output) if output.status.success() => output.stdout,
        Ok(_) | Err(_) => Vec::new(),
    }
}

// the metadata is read before cargo starts: the full resolve takes cargo's package cache lock, which a running build would make it wait on
async fn start_children(
    parser: &CliParser<'_>,
    settings: &Settings,
    target_mode: TargetDirMode<'_>,
    (environment, environment_rustc): (&[&[u8]], Option<&[u8]>),
) -> Result<StartedChildren, RunError> {
    let isolating = target_mode == TargetDirMode::Isolated;
    let mut metadata_child = (settings.bin_gate || isolating || settings.output == OutputMode::Rich)
        .then(|| spawn_metadata(parser, environment, true))
        .and_then(Result::ok);
    let spawned = metadata_child.is_some();
    let mut facts = None;
    if let Some(child) = metadata_child.take() {
        facts = read_metadata(child, (parser, environment)).await;
    }
    let directories = build_directories(target_mode, facts.as_deref())?;
    let cargo = spawn_reserved(|| spawn_cargo(parser, settings.color, environment, directories.as_ref().map(|chosen| &chosen.test)))?;
    track(cargo.process_id())?;
    let mut lock_watch = LockWatch::new();
    match (&directories, facts.as_deref().and_then(MetadataFacts::target_directory)) {
        (Some(chosen), _) => lock_watch.set_directory(chosen.test.directory()),
        (None, Some(shared)) => {
            lock_watch.set_directory(shared);
        }
        (None, None) => {}
    }
    let workspace_root = facts.as_deref().and_then(MetadataFacts::workspace_root).and_then(|bytes| core::str::from_utf8(bytes).ok()).map_or_else(|| settings.source_root.clone(), PathBuf::from);
    let test_targets = facts.as_deref().and_then(|found| found.test_targets);
    test_binaries_debug(settings.debug, test_targets);
    let metadata_directory = facts.as_deref().and_then(MetadataFacts::target_directory).map(Box::from);
    let predicted_units = facts.as_deref().and_then(|found| found.bins.predicted_units()).and_then(|units| u16::try_from(units).ok());
    let toolchain = read_toolchain(environment_rustc);
    let fingerprint = build_fingerprint(facts.as_deref(), parser, &toolchain);
    let (bins, gate) = resolve_bin_gate(settings, spawned, facts);
    Ok(StartedChildren {
        cargo,
        directories,
        bins,
        gate,
        test_targets,
        eta: (fingerprint, predicted_units),
        lock_watch,
        metadata_directory,
        workspace_root,
    })
}

fn resolve_exit_with_parent(
    source: &BorrowedSettings<'_>,
    parser: &CliParser<'_>,
) -> Result<ExitWithParent, RunError> {
    EXIT_WITH_PARENT
        .build_default(Some(crate::build_defaults::EXIT_WITH_PARENT))
        .from_settings(source, parser.exit_with_parent())
        .resolve::<ExitWithParent>()
        .map_err(RunError::Setting)
        .and_then(|(setting, source)| match (setting, cfg!(windows)) {
            (ExitWithParent::On, true) => Err(RunError::Setting(exit_with_parent_unsupported(source))),
            _ => Ok(setting),
        })
}

fn resolve_output(
    source: &BorrowedSettings<'_>,
    parser: &CliParser<'_>,
) -> Result<(OutputMode, &'static str), RunError> {
    OUTPUT
        .build_default(Some(crate::build_defaults::OUTPUT))
        .from_settings(source, parser.output())
        .resolve::<OutputSetting>()
        .map(|(setting, origin)| {
            let terminal = crate::edge_output::is_terminal(STDOUT);
            (setting.mode(terminal, source), setting.why(origin, terminal, source))
        })
        .map_err(RunError::Setting)
}

fn resolve_target_dir<'value>(
    source: &'value BorrowedSettings<'value>,
    parser: &CliParser<'value>,
) -> Result<TargetDirMode<'value>, RunError> {
    TARGET_DIR
        .build_default(Some(crate::build_defaults::TARGET_DIR))
        .from_settings(source, parser.target_dir())
        .resolve::<TargetDirMode<'value>>()
        .map(|(mode, _)| mode)
        .map_err(RunError::Setting)
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

fn resolve_mouse(source: &BorrowedSettings<'_>) -> Result<bool, RunError> {
    match source.get("CARGO_VAL_MOUSE") {
        None => Ok(crate::build_defaults::MOUSE),
        Some("true" | "on" | "1") => Ok(true),
        Some("false" | "off" | "0") => Ok(false),
        Some(_) => Err(RunError::Settings),
    }
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

const QUIT_SIGNAL: i32 = 2;

fn finish_run(
    parser: &CliParser<'_>,
    run: &ListingRun<'_>,
    cargo_exit_code: Option<i32>,
    build_reported: bool,
) -> Result<RunSummary, RunError> {
    let cargo_failed = cargo_exit_code.is_some_and(|code| code != 0);
    let stopped = interrupted(run.signals).err().or_else(|| run.quit.get().map(RunError::Interrupted));
    if let Some(error) = stopped {
        agent_exit(run, error.exit_code())?;
        close_interrupted(parser, run, error.exit_code())?;
        return Err(error);
    }
    if cargo_failed {
        run.interrupt_doc_tests();
        let error = if build_reported { RunError::CargoFailed } else { RunError::CargoRefused };
        match (build_reported, run.settings.output.is_agent_like()) {
            (true, true) => {
                if run.settings.output == OutputMode::Json {
                    let exit = u8::try_from(error.exit_code()).unwrap_or(u8::MAX);
                    emit(run.render(), &Event::BuildFailed { diagnostic: b"diagnostics on stderr", exit, errors: 0, built: 0, of: None })?;
                }
                agent_exit(run, error.exit_code())?;
                if run.settings.output == OutputMode::Json {
                    write_fd(2, CARGO_FAILED_LINE)?;
                }
            }
            (true, false) => close_failed_build(parser, run, error.exit_code())?,
            (false, _) => agent_exit(run, error.exit_code())?,
        }
        return Err(error);
    }
    let doc_tests = run.finish_doc_tests(parser.doc_selection() == DocSelection::Included)?;
    let stop = run.policy.borrow().report();
    conclude(
        parser,
        run.settings,
        (&run.targets.borrow(), run.render()),
        run.started_millis,
        stop,
        doc_tests,
    )
}

// the closing EXIT line of an agent run that ends in an error; every other ending already printed its own
fn agent_exit(run: &ListingRun<'_>, code: i32) -> Result<(), RunError> {
    if !run.settings.output.is_agent_like() {
        return Ok(());
    }
    emit(run.render(), &Event::Exited { code: u8::try_from(code).unwrap_or(u8::MAX) })
}

pub(crate) fn monotonic_millis() -> u64 {
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
    scope: Signal,
    stderr_bytes: [u8; DOC_STDERR_CAPACITY],
    stderr_length: usize,
    lock_watch: LockWatch,
    backlog: Option<(Range<u32>, usize)>,
}

fn spawn_doc_tests(
    parser: &CliParser<'_>,
    (ignore, output): (IgnoreMode, OutputMode),
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
    let plain_color = [cargo_term_color(false)];
    let doc_term_color: &[&[u8]] = match output {
        OutputMode::Plain | OutputMode::Agent | OutputMode::Json => &plain_color,
        OutputMode::Rich => &[],
    };
    let child = spawn_reserved(|| {
        spawn_in_group(b"cargo", &arguments, environment, doc_term_color, None).map_err(|error| {
            if error.os_code == TABLE_FULL { RunError::SignalSlots } else { RunError::Spawn }
        })
    })?;
    let process_id = child.process_id();
    track_leader(process_id)?;
    if child.lower_priority().is_err() && output != OutputMode::Rich {
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
        scope: Signal::new(),
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
        observation: &DocObservation<'_>,
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
                record_doc_event(event, phase, observation)
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

    pub(crate) fn stderr_chunk(&mut self, bytes: &[u8], render: &Render<'_>) -> Result<(), RunError> {
        self.keep_stderr(bytes);
        announce_lock_wait(&mut self.lock_watch, bytes, render)
    }

    pub(crate) fn stderr_ended(&mut self) {
        self.stderr_eof = true;
    }

    pub(crate) fn exited(&mut self, code: i32) {
        quit_step(b"doc-cargo-exited", self.process_id as usize);
        release_group(self.process_id);
        // a fail-fast trip leaves executing doc tests to finish, but a root that fired before the exit ended them: the run is being cancelled, not reported
        if self.exit_code.is_none() && self.scope.is_fired() {
            self.phase = DocPhase::Interrupted;
        }
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

    fn conclude(&self, implicit: bool, render: &Render<'_>) -> Result<DocFinish, RunError> {
        let stderr = &self.stderr_bytes[..self.stderr_length];
        if self.phase == DocPhase::Interrupted {
            return Ok(DocFinish::Stopped);
        }
        if implicit && contains(stderr, NO_LIBRARY_MESSAGE) {
            return Ok(DocFinish::NoLibrary);
        }
        if render.0 != OutputMode::Rich {
            write_doc_stderr(stderr, render.0.is_agent_like())?;
        }
        Ok(DocFinish::Ran)
    }

    // a rich run keeps the doc cargo's output as a failure the review and the closing block show, never scrollback
    fn keep_for_review(&self, observation: &DocObservation<'_>) -> Result<(), RunError> {
        let stderr = &self.stderr_bytes[..self.stderr_length];
        let lines = observation.failure_lines.borrow();
        if stderr.is_empty() && lines.is_empty() {
            return Ok(());
        }
        let mut name = observation.first_failed.borrow().clone();
        if name.is_empty() {
            name.extend_from_slice(b"doc tests");
            let id = (&b"doc-tests"[..], name.as_slice());
            emit(observation.render, &Event::Settled { id, outcome: Outcome::Failed, at_ms: None, solo: false })?;
        }
        let id = (&b"doc-tests"[..], name.as_slice());
        let exit = Exit::Code(self.exit_code.unwrap_or(1));
        emit(observation.render, &Event::Captured { id, stdout: (lines.as_slice(), 0), stderr: (stderr, 0), exit, excerpt: Err(b"") })
    }
}

fn write_doc_stderr(stderr: &[u8], agent_like: bool) -> Result<(), RunError> {
    let repeated = |line: &[u8]| if agent_like { is_cargo_progress(line) } else { line.trim_ascii_start().starts_with(b"Finished ") };
    stderr.split_inclusive(|byte| *byte == b'\n').filter(|line| !repeated(line)).try_for_each(|line| write_fd(2, line))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn record_doc_event(
    event: DocEvent<'_>,
    phase: &mut DocPhase,
    observation: &DocObservation<'_>,
) -> Result<(), RunError> {
    match event {
        DocEvent::Running { count } => {
            if *phase == DocPhase::Starting {
                pipeline_step(b"doc-tests-running", b"");
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
        DocEvent::Result { name, outcome, .. } => report_doc_result(name, outcome, observation),
        DocEvent::FailureLine(line) if observation.render.0 == OutputMode::Rich => {
            let mut kept = observation.failure_lines.borrow_mut();
            kept.extend_from_slice(line);
            kept.push(b'\n');
            Ok(())
        }
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
    observation: &DocObservation<'_>,
) -> Result<(), RunError> {
    let (terminal, counter) = match outcome {
        DocOutcome::Passed => (TerminalOutcome::Passed, &observation.passed),
        DocOutcome::Failed => (TerminalOutcome::Failed, &observation.failed),
        DocOutcome::Ignored => (TerminalOutcome::Ignored, &observation.ignored),
    };
    counter.set(counter.get() + 1);
    if outcome == DocOutcome::Failed && observation.first_failed.borrow().is_empty() {
        observation.first_failed.borrow_mut().extend_from_slice(name);
    }
    let id = (&b"doc-tests"[..], name);
    emit(observation.render, &Event::Settled { id, outcome: rendered(terminal), at_ms: None, solo: false })
}

fn write_doc_notice(totals: &DocTotals, agent_like: bool) -> Result<(), RunError> {
    let mut line = LineBuffer::new();
    match totals.finish {
        DocFinish::NoLibrary => line.text(b"cargo-val: doc tests: no library target to test\n"),
        DocFinish::Stopped => line.text(b"cargo-val: doc tests: stopped before executing\n"),
        DocFinish::Ran if agent_like => return Ok(()),
        DocFinish::Ran if totals.passed + totals.failed + totals.ignored == 0 => {
            line.text(b"cargo-val: doc tests: running 0 tests (");
        }
        DocFinish::Ran => return Ok(()),
    }
    if totals.finish == DocFinish::Ran {
        line.number(totals.empty_blocks);
        line.text(b" of ");
        line.number(totals.blocks);
        line.text(b" runs empty)\n");
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
    observation: &DocObservation<'_>,
) -> Result<DocTotals, RunError> {
    let finish = doc.conclude(implicit, observation.render)?;
    if finish == DocFinish::Ran && observation.render.0 == OutputMode::Rich && (doc.exit_code != Some(0) || observation.failed.get() != 0) {
        doc.keep_for_review(observation)?;
    }
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
) -> Result<Counts, RunError> {
    let mut total = Counts::default();
    for target in 0..targets.len() {
        let counts = targets.counts(target);
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
        total = total.plus(&doc_counts(doc));
    }
    Ok(total)
}

fn conclude(
    parser: &CliParser<'_>,
    settings: &Settings,
    (targets, render): (&TargetTable, &Render<'_>),
    started_millis: u64,
    stop: Option<StopReport>,
    doc_tests: Option<DocTotals>,
) -> Result<RunSummary, RunError> {
    erase_frame(render)?;
    if let Some(report) = stop {
        write_stop_trailer(&report, settings.process == ProcessMode::Shard)?;
    }
    if let Some(doc) = &doc_tests {
        write_doc_notice(doc, settings.output.is_agent_like())?;
    }
    let total = write_summary(targets, doc_tests.as_ref())?;
    let empty = total.passed + total.failed == 0;
    let exit = match (total.failed, empty, settings.no_tests) {
        (0, true, NoTestsPolicy::Fail) => NO_TESTS_EXIT_CODE,
        (0, ..) => 0,
        _ => 1,
    };
    emit(render, &Event::Listed { binaries: targets.len(), of: Some(targets.len()), tests: total.discovered })?;
    emit_finished(render, &total, stop.map_or(0, |report| report.listed_not_run), (started_millis, parser.manifest_path(), exit))?;
    if parser.timeline() && matches!(settings.output, OutputMode::Plain | OutputMode::Rich) {
        emit_timeline(render)?;
    }
    if empty && settings.no_tests != NoTestsPolicy::Pass && !settings.output.is_agent_like() {
        write_no_tests(parser, settings.ignore)?;
    }
    close_cursor()?;
    Ok(RunSummary {
        passed: total.passed,
        failed: total.failed,
        stop,
        no_tests: empty.then_some(settings.no_tests),
        doc_tests,
        discovered: total.discovered,
    })
}

// an interrupted plain or rich run closes with the Finished event a finished run closes with; not_run is what was discovered and never settled
fn close_interrupted(parser: &CliParser<'_>, run: &ListingRun<'_>, exit: i32) -> Result<(), RunError> {
    if run.settings.output.is_agent_like() {
        return Ok(());
    }
    name_interrupt(run)?;
    let targets = run.targets.borrow();
    let not_run = run.interrupt_not_run.get();
    let settled = (0..targets.len()).fold(Counts::default(), |sum, target| sum.plus(&targets.counts(target)));
    // a withdrawn name left the discovered count when its child was ended, so the interrupted run puts it back as not_run
    let total = Counts { discovered: settled.discovered + not_run, ..settled };
    run.render().1.borrow_mut().apply(&Event::Listed { binaries: targets.len(), of: Some(targets.len()), tests: total.discovered });
    proxima_telemetry::debug!(exit = exit, not_run = not_run, discovered = total.discovered, "interrupted run erasing its frame to print the closing block");
    erase_frame(run.render())?;
    emit_finished(run.render(), &total, not_run, (run.started_millis, parser.manifest_path(), exit))?;
    close_cursor()
}

// a failed rich or plain build already printed BUILD FAILED and the diagnostics; this closes with the SUMMARY and EXIT lines the mockup ends on
fn close_failed_build(parser: &CliParser<'_>, run: &ListingRun<'_>, exit: i32) -> Result<(), RunError> {
    let settled = {
        let targets = run.targets.borrow();
        (0..targets.len()).fold(Counts::default(), |sum, target| sum.plus(&targets.counts(target)))
    };
    let not_run = settled.discovered.saturating_sub(settled.passed + settled.failed + settled.ignored);
    emit_finished(run.render(), &settled, not_run, (run.started_millis, parser.manifest_path(), exit))?;
    emit(run.render(), &Event::Exited { code: u8::try_from(exit).unwrap_or(u8::MAX) })?;
    close_cursor()
}

// a signal that arrived after the run loop stopped listening (post-run review, the last instant) is named here with no live groups
fn name_interrupt(run: &ListingRun<'_>) -> Result<(), RunError> {
    let Some(signal) = run.signals.received() else {
        return Ok(());
    };
    if run.interrupt_reported.get() != 0 {
        return Ok(());
    }
    abandon_editor();
    emit(run.render(), &Event::Interrupted { signal: u8::try_from(signal).unwrap_or(u8::MAX), groups: 0 })?;
    run.interrupt_reported.set(1);
    Ok(())
}

fn emit_finished(
    render: &Render<'_>,
    total: &Counts,
    not_run: usize,
    (started_millis, manifest, exit): (u64, Option<&[u8]>, i32),
) -> Result<(), RunError> {
    release_warnings(render)?;
    emit(
        render,
        &Event::Finished {
            passed: total.passed,
            failed: total.failed,
            ignored: total.ignored,
            not_run,
            wall_ms: monotonic_millis().saturating_sub(started_millis),
            manifest,
            discovered: total.discovered,
            filtered: total.filtered,
            exit: u8::try_from(exit).unwrap_or(u8::MAX),
        },
    )
}

async fn run_doc_only<'run>(
    parser: &CliParser<'_>,
    (settings, render): (&Settings, &'run Render<'run>),
    target_mode: TargetDirMode<'_>,
    (started_millis, location): (u64, &mut Located),
    (environment, target_variable): (&[&[u8]], Option<&str>),
) -> Result<RunSummary, RunError> {
    let signals = SignalPipe::install().map_err(|_| RunError::Reactor)?;
    watch_parent(&signals, settings.exit_with_parent)?;
    let metadata_child = (!matches!(target_mode, TargetDirMode::Path(_)))
        .then(|| spawn_metadata(parser, environment, true))
        .and_then(Result::ok);
    let facts = match metadata_child {
        Some(child) => read_metadata(child, (parser, environment)).await,
        None => None,
    };
    let metadata_directory = facts.as_deref().and_then(MetadataFacts::target_directory);
    open_last_run(render, settings, location, ([explicit_root(target_mode), target_variable.map(str::as_bytes), metadata_directory], 0))?;
    let directories = build_directories(target_mode, facts.as_deref())?;
    let mut doc = spawn_doc_tests(
        parser,
        (settings.ignore, settings.output),
        environment,
        directories.as_ref().map(|chosen| &chosen.doc),
    )?;
    if directories.is_none()
        && let Some(shared) = facts.as_deref().and_then(MetadataFacts::target_directory)
    {
        doc.lock_watch.set_directory(shared);
    }
    let observation = DocObservation::new(render);
    observe_doc(&mut doc, &signals, &observation).await?;
    interrupted(&signals)?;
    let totals = close_doc_tests(&doc, false, &observation)?;
    let summary = conclude(
        parser,
        settings,
        (&TargetTable::new(), render),
        started_millis,
        None,
        Some(totals),
    )?;
    Ok(summary)
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
    use super::{ERROR_LINE_CAPACITY, ErrorLine, LaunchCause, Located, OutcomeError, OutputMode, RunError, RunSummary, admit_diagnostic, is_failure_restatement, is_status_line, last_run, last_run_record, persist_last_run, settings_from, validated_harness, whole_lines_len};
    use conflaguration::BorrowedSettings;
    use std::collections::HashSet;
    use std::string::String;
    use std::vec::Vec;

    use crate::cli::CliParser;
    use crate::config::NoTestsPolicy;
    use crate::test_dir::tempdir;

    #[test]
    fn ten_thousand_held_warnings_sit_in_the_store_file_and_the_state_footprint_stays_flat() {
        let directory = tempfile::tempdir().expect("temporary target");
        let store = std::rc::Rc::new(crate::edge_store::open(Some(directory.path())));
        let (mut arena, mut ledger) = ([0_u8; 64], [0_u32; 4]);
        let mut frame = crate::line::Line::<{ crate::view_defaults::FRAME_BYTES }>::new();
        let render: super::Render<'_> = (OutputMode::Rich, core::cell::RefCell::new(super::RenderState::new(&mut arena, &mut ledger)), core::cell::RefCell::new(&mut frame), core::cell::RefCell::new(Some(std::rc::Rc::clone(&store))));
        let footprint = core::mem::size_of_val(&*render.1.borrow());
        let warning = b"warning: `fixture` (lib) generated 1 warning\n";

        (0..10_000).for_each(|_| super::hold_warnings(&render, warning));

        let held = render.1.borrow().held();
        assert_eq!(core::mem::size_of_val(&*render.1.borrow()), footprint, "the state is the same size however many warnings it holds");
        assert_ne!(held.0, 0, "the chain has a first page");
        let file_length = store.0.as_ref().expect("the store has a file").metadata().expect("the store file has metadata").len();
        assert!(file_length >= 10_000 * warning.len() as u64, "the store file holds every held byte, it is {file_length} long");
        let mut page = [0_u8; super::WARNING_PAGE_BYTES];
        let (_, length) = crate::edge_store::next_page(&store, (0, held.0), &mut page).expect("the first page reads back");
        assert_eq!(&page[..length], warning, "the chain reads back in append order from its first page");
    }

    #[test]
    fn only_runs_with_a_summary_map_to_a_record() {
        let ran = |failed, discovered, no_tests| RunSummary { passed: 5, failed, stop: None, no_tests, doc_tests: None, discovered };
        let cases = [
            (Ok(ran(0, 383, None)), Some((900, 383, 0, 77, 1204, 600))),
            (Ok(ran(2, 383, None)), Some((900, 383, 1, 77, 1204, 600))),
            (Ok(ran(0, 0, Some(NoTestsPolicy::Fail))), Some((900, 0, 4, 77, 1204, 600))),
            (Err(RunError::CargoFailed), None),
            (Err(RunError::CargoRefused), None),
            (Err(RunError::Spawn), None),
            (Err(RunError::Interrupted(2)), None),
            (Err(RunError::Interrupted(15)), None),
        ];
        for (outcome, expected) in cases {
            assert_eq!(last_run_record(900, [77, 1204, 600], &outcome), expected, "{outcome:?}");
        }
    }

    #[test]
    fn persisting_replaces_the_record_only_for_a_summary() {
        let target = tempdir().expect("tempdir is creatable");
        last_run::write(target.path(), (1, 2, 0, 3, 4, 5)).expect("earlier record is writable");
        let located: Located = (Some(target.path().to_path_buf()), [9, 120, 30]);
        persist_last_run(&located, 50, &Err(RunError::Interrupted(2)));
        assert_eq!(last_run::read(target.path()), Some((1, 2, 0, 3, 4, 5)));
        let summary = RunSummary { passed: 4, failed: 0, stop: None, no_tests: None, doc_tests: None, discovered: 4 };
        persist_last_run(&located, 50, &Ok(summary));
        assert_eq!(last_run::read(target.path()), Some((50, 4, 0, 9, 120, 30)));
        persist_last_run(&(None, [1, 1, 1]), 60, &Ok(summary));
        assert_eq!(last_run::read(target.path()), Some((50, 4, 0, 9, 120, 30)));
    }

    const RICH_WITH_COLOUR: [(&str, Option<&str>); 3] = [("CARGO_VAL_OUTPUT", Some("rich")), ("CARGO_VAL_COLOR", Some("true")), ("NO_COLOR", None)];

    fn link_template_under(extra: &[(&str, Option<&str>)]) -> Option<String> {
        let pairs: Vec<(&str, &str)> = RICH_WITH_COLOUR.iter().chain(extra).filter_map(|(key, value)| value.map(|text| (*key, text))).collect();
        let parser = CliParser::new();
        let harness = validated_harness(&parser).expect("an empty command line is a valid harness request");
        settings_from(&BorrowedSettings::new(&pairs), &harness, &parser).expect("settings resolve from the supplied pairs").link_template
    }

    fn mouse_under(value: Option<&str>) -> Result<bool, RunError> {
        let pairs = [("CARGO_VAL_MOUSE", value.unwrap_or(""))];
        let supplied = &pairs[..usize::from(value.is_some())];
        super::resolve_mouse(&conflaguration::BorrowedSettings::new(supplied))
    }

    #[proxima::test]
    async fn the_mouse_setting_defaults_to_on_and_accepts_the_boolean_spellings_of_the_other_switches() {
        assert!(mouse_under(None).expect("unset resolves to the build default"));
        for (text, expected) in [("true", true), ("on", true), ("1", true), ("false", false), ("off", false), ("0", false)] {
            assert_eq!(mouse_under(Some(text)).expect("a boolean spelling resolves"), expected, "CARGO_VAL_MOUSE={text}");
        }
    }

    #[proxima::test]
    async fn a_mouse_setting_that_is_not_a_boolean_is_refused_instead_of_read_as_off() {
        assert!(matches!(mouse_under(Some("sometimes")), Err(RunError::Settings)));
    }
    #[proxima::test]
    async fn the_editor_url_environment_layer_is_the_rich_link_template_over_a_derived_editor() {
        let resolved = link_template_under(&[("CARGO_VAL_EDITOR_URL", Some("idea://open?file={file}&line={line}")), ("VISUAL", Some("code"))]);

        assert_eq!(resolved.as_deref(), Some("idea://open?file={file}&line={line}"));
    }

    #[proxima::test]
    async fn visual_code_derives_the_vscode_template_when_no_layer_supplies_editor_url() {
        let resolved = link_template_under(&[("CARGO_VAL_EDITOR_URL", None), ("VISUAL", Some("code"))]);

        assert_eq!(resolved.as_deref(), Some("vscode://file{file}:{line}:{col}"));
    }

    #[proxima::test]
    async fn plain_and_uncoloured_runs_resolve_no_link_template() {
        let parser = CliParser::new();
        let harness = validated_harness(&parser).expect("an empty command line is a valid harness request");
        let pairs = [("CARGO_VAL_OUTPUT", "plain"), ("VISUAL", "code")];
        let plain = settings_from(&BorrowedSettings::new(&pairs), &harness, &parser).expect("settings resolve").link_template;

        assert_eq!(plain, None);
        assert_eq!(link_template_under(&[("NO_COLOR", Some("1")), ("VISUAL", Some("code"))]), None);
    }

    #[proxima::test]
    async fn cargo_status_lines_are_recognised_with_or_without_colour_and_diagnostics_are_not() {
        let status: [&[u8]; 4] = [b"   Compiling cargo-val v0.1.0 (/work)\n", b"    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.21s\n", b"    Blocking waiting for file lock on package cache\n", b"\x1b[1m\x1b[32m   Compiling\x1b[0m serde v1.0.0\n"];
        let kept: [&[u8]; 4] = [b"error[E0308]: mismatched types\n", b"warning: unused variable: `scale`\n", b"error: could not compile `build_error` (lib test) due to 1 previous error\n", b"  --> src/lib.rs:4:5\n"];

        assert!(status.iter().all(|line| is_status_line(line)));
        assert!(!kept.iter().any(|line| is_status_line(line)));
    }

    #[proxima::test]
    async fn the_timeline_arena_is_sized_only_where_the_view_can_be_shown() {
        let full = crate::view_defaults::TIMELINE_BYTES;

        assert_eq!(super::timeline_arena_bytes(OutputMode::Plain, false), 0, "plain without --timeline never reads the arena");
        assert_eq!(super::timeline_arena_bytes(OutputMode::Plain, true), full);
        assert_eq!(super::timeline_arena_bytes(OutputMode::Rich, false), full, "the review s key reads it without the flag");
        assert_eq!(super::timeline_arena_bytes(OutputMode::Rich, true), full);
        assert_eq!(super::timeline_arena_bytes(OutputMode::Agent, false), 0);
        assert_eq!(super::timeline_arena_bytes(OutputMode::Json, false), 0);
    }

    fn rendered(cause: LaunchCause) -> std::string::String {
        let line: ErrorLine = RunError::Launch(cause).render();
        std::string::String::from_utf8(line.bytes().to_vec()).expect("launch line is UTF-8")
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_exit_with_parent_from_the_environment_with_the_setting_named() {
        let pairs = [("CARGO_VAL_EXIT_WITH_PARENT", "1")];
        let source = conflaguration::BorrowedSettings::new(&pairs);

        let refused = super::resolve_exit_with_parent(&source, &crate::cli::CliParser::new());

        let Err(RunError::Setting(error)) = refused else {
            panic!("windows accepted exit-with-parent: {refused:?}");
        };
        assert_eq!(error.message(), b"cargo-val: CARGO_VAL_EXIT_WITH_PARENT=on is not supported on windows\n");
    }

    #[proxima::test]
    async fn a_cargo_error_line_split_after_its_label_is_held_until_its_newline() {
        let first_piece = b"error";
        let second_piece = b": could not compile `bin_gate` (bin \"gate-bin\") due to 1 previous error\n";
        let mut pending = first_piece.to_vec();

        assert_eq!(whole_lines_len(&pending, false), 0);

        pending.extend_from_slice(second_piece);
        assert_eq!(whole_lines_len(&pending, false), pending.len());
    }

    #[proxima::test]
    async fn a_trailing_partial_line_stays_held_behind_the_whole_lines_before_it() {
        let pending = b"warning: build failed, waiting for other jobs to finish...\nerror";

        assert_eq!(whole_lines_len(pending, false), pending.len() - b"error".len());
    }

    #[proxima::test]
    async fn an_ended_stream_releases_its_unterminated_tail() {
        let pending = b"error: could not compile `bin_gate`";

        assert_eq!(whole_lines_len(pending, true), pending.len());
        assert_eq!(whole_lines_len(b"", true), 0);
    }

    const E0308: &str = "error[E0308]: mismatched types\n --> src/lib.rs:4:5\n  |\n4 |     \"forty-two\"\n  |     ^^^^^^^^^^^ expected `u32`, found `&str`\n\n";
    const E0308_SPAN: (&str, u32, u32) = ("src/lib.rs", 4, 5);
    const NO_SPAN: (&str, u32, u32) = ("", 0, 0);
    const EXPLAIN: &str = "For more information about this error, try `rustc --explain E0308`.\n";

    #[test]
    fn the_same_error_from_the_lib_and_the_lib_test_target_is_admitted_once() {
        let mut seen = HashSet::new();

        let lib = admit_diagnostic(&mut seen, false, ("error", E0308), E0308_SPAN);
        let lib_test = admit_diagnostic(&mut seen, false, ("error", E0308), E0308_SPAN);

        assert_eq!((lib, lib_test), (true, false));
    }

    #[test]
    fn the_same_message_at_another_span_or_another_message_at_the_same_span_is_a_different_diagnostic() {
        let mut seen = HashSet::new();
        let elsewhere = ("src/other.rs", 4, 5);
        let other_message = "error[E0599]: no method named `answer` found\n --> src/lib.rs:4:5\n";

        let first = admit_diagnostic(&mut seen, false, ("error", E0308), E0308_SPAN);
        let moved = admit_diagnostic(&mut seen, false, ("error", E0308), elsewhere);
        let renamed = admit_diagnostic(&mut seen, false, ("error", other_message), E0308_SPAN);

        assert_eq!((first, moved, renamed), (true, true, true));
    }

    #[test]
    fn rustcs_failure_note_is_admitted_once_outside_agent_output_and_never_in_it() {
        let mut seen = HashSet::new();

        let first = admit_diagnostic(&mut seen, false, ("failure-note", EXPLAIN), NO_SPAN);
        let repeated = admit_diagnostic(&mut seen, false, ("failure-note", EXPLAIN), NO_SPAN);
        let agent = admit_diagnostic(&mut HashSet::new(), true, ("failure-note", EXPLAIN), NO_SPAN);

        assert_eq!((first, repeated, agent), (true, false, false));
    }

    #[test]
    fn cargos_two_restating_lines_are_recognised_with_or_without_colour_and_other_errors_are_not() {
        assert!(is_failure_restatement(b"error: could not compile `build_error` (lib) due to 1 previous error; 1 warning emitted\n"));
        assert!(is_failure_restatement(b"\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: build failed, waiting for other jobs to finish...\x1b[0m\n"));
        assert!(!is_failure_restatement(b"error: failed to run custom build command for `ring v0.17.8`\n"));
        assert!(!is_failure_restatement(b"warning: unused manifest key: package.foo\n"));
    }

    #[proxima::test]
    async fn launch_failure_line_names_each_cause() {
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

    #[proxima::test]
    async fn longest_launch_failure_line_fits_the_error_line() {
        let longest = LaunchCause::NameTooLong {
            required: usize::MAX,
            available: usize::MAX,
        };
        let line = rendered(longest);
        assert!(line.ends_with("are available\n"), "{line}");
        assert!(line.len() < ERROR_LINE_CAPACITY);
    }

    #[proxima::test]
    async fn libtest_result_lines_classify_the_exact_child() {
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

    #[proxima::test]
    async fn real_ignored_listing_fills_the_set_at_every_chunk_size() {
        const IGNORED: &[u8] = include_bytes!("../tests/fixtures/listing/ignore_modes_list_ignored.txt");
        for chunk_size in [1, 2, 7, 16, IGNORED.len()] {
            let set = collected(chunk_size, IGNORED).expect("real listing frames");

            assert_eq!(set.len(), 1, "chunk size {chunk_size}");
            assert!(set.contains(b"fixture::ignored_with_reason"), "chunk size {chunk_size}");
        }
    }

    #[proxima::test]
    async fn full_listing_names_every_test_and_skips_the_summary() {
        const LISTING: &[u8] = include_bytes!("../tests/fixtures/listing/ignore_modes_list.txt");
        let set = collected(5, LISTING).expect("real listing frames");

        assert_eq!(set.len(), 3);
        assert!(set.contains(b"fixture::passes"));
        assert!(set.contains(b"fixture::fails"));
    }

    #[proxima::test]
    async fn invalid_utf8_listing_line_is_refused() {
        let outcome = collected(4, b"fixture::\xff: test\n");

        assert!(matches!(outcome, Err(RunError::ListingStream)));
    }

    #[proxima::test]
    async fn listing_line_past_the_record_limit_is_refused() {
        let mut listing = std::vec![b'a'; super::RESULT_LINE_CAPACITY + 1];
        listing.extend_from_slice(b"\n");

        assert!(matches!(collected(64, &listing), Err(RunError::ListingStream)));
    }

    #[proxima::test]
    async fn running_tests_plus_listing_children_never_pass_the_signal_table_at_any_jobs_value() {
        for jobs in 1..=super::LIVE_CAPACITY {
            let children = 2 * super::listing_limit(jobs);
            assert!(jobs + super::SIGNAL_HOLDERS + children <= super::SIGNAL_SLOTS, "jobs {jobs} with {children} listing children overflows {} slots", super::SIGNAL_SLOTS);
        }
        assert_eq!(super::listing_limit(64), 2, "the default 72 slots leave room for two listings beside 64 tests, the build cargo, the doc test group and the editor");
        assert_eq!(super::listing_limit(10), 8, "the default jobs keep the full listing width");
    }

    #[proxima::test]
    async fn refusal_without_detail_renders_its_static_message() {
        let line = RunError::Reactor.render();
        assert_eq!(line.bytes(), RunError::Reactor.message());
    }
}

#[cfg(all(test, unix))]
pub(super) mod listing_allocation_tests {
    use core::cell::{Cell, RefCell};
    use std::vec::Vec;

    use proxima_core::signal::Signal;

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

    #[proxima::test]
    async fn spawning_a_listing_child_allocates_zero_times() {
        crate::edge_exit::fixture::in_single_threaded_process(spawning_a_listing_child_allocates_zero_times_in_isolation);
    }

    pub(super) fn with_run<Output>(body: impl FnOnce(&ListingRun<'_>) -> Output) -> Output {
        let inherited_storage = std::env::vars()
            .map(|(key, value)| std::format!("{key}={value}"))
            .collect::<std::vec::Vec<_>>();
        let inherited = inherited_storage.iter().map(std::string::String::as_bytes).collect::<std::vec::Vec<_>>();
        let settings = Settings {
            bin_gate: false,
            debug: false,
            ignore: IgnoreMode::Include,
            filterset: None,
            link_template: None,
            editor: std::vec::Vec::new(),
            probe_ms: 300,
            jobs: 2,
            max_fail: None,
            no_tests: NoTestsPolicy::Pass,
            process: crate::config::ProcessMode::Test,
            output: crate::config::OutputMode::Rich,
            output_why: "flag",
            color: false,
            mouse: None,
            exit_with_parent: crate::config::ExitWithParent::Off,
            test_limit_ms: None,
            heartbeat_ms: 10_000,
            source_root: std::path::PathBuf::new(),
        };
        let signals = SignalPipe::install().expect("signal pipe installs once in the unit test process");
        let mut arena = [0_u8; 64];
        let mut ledger = [0_u32; 4];
        let mut frame = crate::line::Line::new();
        let render = (crate::config::OutputMode::Rich, RefCell::new(super::RenderState::new(&mut arena, &mut ledger)), RefCell::new(&mut frame), RefCell::new(None));
        let root = Signal::new();
        let run = ListingRun {
            signals: &signals,
            store: std::rc::Rc::new(crate::edge_store::unavailable()),
            reads: RefCell::new(&mut []),
            cursor: Cell::new([0; 5]), head: Cell::new(0),
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
            docs: DocObservation::new(&render),
            policy: RefCell::new(StopPolicy::new(None)),
            doc: RefCell::new(None),
            doc_pending: Cell::new(false),
            launch_storage: RefCell::new(held_launch_storage()),
            ready: core::cell::Cell::new(None),
                shards: RefCell::new(crate::shard_arena::ShardArena::new(&mut [])),
            sched: RefCell::new(SchedTable::new(&mut [])),
            specials: RefCell::new(SpecialPlan::new(&mut [])),
            sched_target: Cell::new(None),
            interrupt_reported: Cell::new(0),
            interrupt_not_run: Cell::new(0),
            quit: Cell::new(None),
            editing: RefCell::new(None),
            editor_program: RefCell::new(Vec::new()), request: RefCell::new(None), outside: RefCell::new(Vec::new()), detached_running: Cell::new(false), probe: RefCell::new(None), probe_site: RefCell::new((Vec::new(), 0, 0)), probe_output: RefCell::new((Vec::new(), None, false)),
            trip: root.child(),
            root,
        };
        body(&run)
    }

    fn spawning_a_listing_child_allocates_zero_times_in_isolation() {
        let executable = std::env::current_exe().expect("test executable path is available");
        let executable = executable.to_str().expect("test executable path is UTF-8");
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let artifact = TestArtifact {
            executable,
            package_id: "path+file:///repo/parser#0.3.1",
            manifest_path: manifest,
            target_name: "cargo_val",
            target_kind: "lib",
        };
        let mut environments = super::zeroed(ENVIRONMENT_CAPACITY * LISTING_SLOTS);

        let (listing, allocations) = with_run(|run| crate::test_alloc::measure_allocations(|| spawn_listing(&artifact, run, "", None, 1, &mut environments)));

        let mut listing = listing.expect("the listing child starts");
        reap(&mut listing);
        std::println!("{{\"listing_spawn_allocations\":{allocations}}}");
        assert_eq!(allocations, 0);
    }
}

#[cfg(test)]
pub(super) mod doc_trip_tests {
    use proxima_core::signal::Signal;

    use super::{DOC_STDERR_CAPACITY, DocAtTrip, DocParser, DocPhase, DocRun, LockWatch};

    const ABSENT_GROUP: u32 = 2_000_000_000;

    pub(super) fn doc_run(phase: DocPhase, exit_code: Option<i32>) -> DocRun {
        DocRun {
            child: None,
            process_id: ABSENT_GROUP,
            parser: DocParser::new(),
            stdout_eof: false,
            stderr_eof: false,
            exit_code,
            phase,
            stderr_bytes: [0; DOC_STDERR_CAPACITY],
            stderr_length: 0,
            scope: Signal::new(),
            lock_watch: LockWatch::new(),
            backlog: None,
        }
    }

    #[proxima::test]
    async fn each_doc_phase_reports_what_actually_happened_at_the_trip() {
        let mut held = doc_run(DocPhase::Starting, None);
        let mut executing = doc_run(DocPhase::Executing, None);
        let mut completed = doc_run(DocPhase::Executing, Some(0));

        assert_eq!(held.state_for_trip(), DocAtTrip::InterruptedBeforeExecuting);
        assert_eq!(executing.state_for_trip(), DocAtTrip::ExecutingLeftToFinish);
        assert_eq!(completed.state_for_trip(), DocAtTrip::AlreadyFinished);
    }

    #[proxima::test]
    async fn only_the_held_doc_phase_is_interrupted_by_the_trip() {
        let mut held = doc_run(DocPhase::Starting, None);
        let mut completed = doc_run(DocPhase::Starting, Some(0));

        held.state_for_trip();
        completed.state_for_trip();

        assert_eq!(held.phase, DocPhase::Interrupted);
        assert_eq!(completed.phase, DocPhase::Starting);
    }
}

#[cfg(all(test, unix))]
mod close_sequence_tests {
    use core::cell::RefCell;
    use core::sync::atomic::Ordering;
    use core::future::poll_fn;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_runtime::CoreId;

    use super::{EDITOR_RESERVED, FRAME_BYTES, MOUSE_REPORTING, Render, RenderState, close_cursor, emit, erase_frame, erase_live, restore_cursor};
    use crate::config::OutputMode;
    use crate::edge_exit::fixture::in_single_threaded_process;
    use crate::failure_class::Exit;
    use crate::line::Line;
    use crate::render::{Event, UNREAD};
    use crate::status_queue::{Capture, Sinks};

    const STDOUT: i32 = 1;
    const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    const ERASE_TAIL: &[u8] = b"A\r\x1b[J";

    fn begin() -> Event<'static> {
        Event::Begin { version: b"0.1.0", shard: false, jobs: 10, github: false, why: None, json: false, at_ms: 0 }
    }

    fn finished_with_a_failure() -> Event<'static> {
        Event::Finished { passed: 1, failed: 1, ignored: 1, not_run: 0, wall_ms: 935, manifest: None, discovered: 3, filtered: 0, exit: 1 }
    }

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut ends = [-1; 2];
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "pipe is created");
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
    }

    fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    fn rposition(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).rposition(|window| window == needle)
    }

    // a tick drew the frame, then the close runs, then one more tick arrives: the rich close as the driver sequences it
    fn close_after_a_drawn_frame(late_tick: bool) -> Vec<u8> {
        let (reader, writer) = pipe_pair();
        let saved_stdout = unsafe { libc::fcntl(STDOUT, libc::F_DUPFD_CLOEXEC, 0) };
        assert!(saved_stdout >= 0, "stdout is duplicated for restore");
        assert!(unsafe { libc::dup2(writer.as_raw_fd(), STDOUT) } >= 0, "stdout points at the pipe");
        let outcome = run_on_this_thread(CoreId(0), async move {
            let _capture = Capture::enter();
            let (mut arena, mut ledger) = ([0_u8; 64], [0_u32; 4]);
            let mut frame = Line::<FRAME_BYTES>::new();
            let live: Render<'_> = (OutputMode::Rich, RefCell::new(RenderState::new(&mut arena, &mut ledger)), RefCell::new(&mut frame), RefCell::new(None));
            emit(&live, &begin()).expect("begin draws the first frame");
            emit(&live, &Event::Tick { now_ms: 250 }).expect("a tick redraws the frame");
            erase_frame(&live).expect("the close erases the frame");
            emit(&live, &finished_with_a_failure()).expect("the closing block queues");
            close_cursor().expect("the cursor return queues");
            if late_tick {
                emit(&live, &Event::Tick { now_ms: 500 }).expect("a late tick is accepted");
            }
            let mut sinks = Sinks::new();
            poll_fn(|context| sinks.poll_drain(context)).await.expect("the queue drains");
        });
        unsafe {
            libc::dup2(saved_stdout, STDOUT);
            libc::close(saved_stdout);
        }
        outcome.expect("root completes");
        drop(writer);
        let mut delivered = Vec::new();
        std::fs::File::from(reader).read_to_end(&mut delivered).expect("the pipe reads to its end");
        delivered
    }

    #[proxima::test]
    async fn a_close_after_a_drawn_frame_erases_before_it_shows_the_cursor_and_nothing_draws_after_the_erase() {
        in_single_threaded_process(|| {
            let delivered = close_after_a_drawn_frame(false);
            let text = std::string::String::from_utf8_lossy(&delivered);

            let erase = rposition(&delivered, ERASE_TAIL).expect("the drawn frame is erased");
            let show = position(&delivered, SHOW_CURSOR).expect("the cursor is shown");
            assert!(erase < show, "the erase precedes the show-cursor:\n{text:?}");
            assert_eq!(delivered.windows(SHOW_CURSOR.len()).filter(|window| *window == SHOW_CURSOR).count(), 1, "the cursor is shown once:\n{text:?}");
            assert!(delivered.ends_with(SHOW_CURSOR), "the cursor returns after the closing block:\n{text:?}");
            let after_erase = &delivered[erase..];
            assert!(position(after_erase, HIDE_CURSOR).is_none(), "no frame is drawn once the erase has begun:\n{text:?}");
            assert!(position(after_erase, b"SUMMARY  passed=1 failed=1 ignored=1").is_some_and(|summary| summary < after_erase.len() - SHOW_CURSOR.len()), "the closing block sits between the erase and the cursor:\n{text:?}");
        });
    }

    const FAILURE_BLOCK: &[u8] = b"------ stdout: cli_run fixture::fails_when_requested ------";

    // the editor opens over a drawn frame, a tick and a failure settle while it owns the terminal, then it exits and the frame is redrawn
    fn failure_settles_while_the_editor_is_open() -> Vec<u8> {
        let (reader, writer) = pipe_pair();
        let saved_stdout = unsafe { libc::fcntl(STDOUT, libc::F_DUPFD_CLOEXEC, 0) };
        assert!(saved_stdout >= 0, "stdout is duplicated for restore");
        assert!(unsafe { libc::dup2(writer.as_raw_fd(), STDOUT) } >= 0, "stdout points at the pipe");
        let outcome = run_on_this_thread(CoreId(0), async move {
            let _capture = Capture::enter();
            let (mut arena, mut ledger) = ([0_u8; 64], [0_u32; 4]);
            let mut frame = Line::<FRAME_BYTES>::new();
            let live: Render<'_> = (OutputMode::Rich, RefCell::new(RenderState::new(&mut arena, &mut ledger)), RefCell::new(&mut frame), RefCell::new(None));
            emit(&live, &begin()).expect("begin draws the first frame");
            erase_live(&live).expect("the editor launch erases the frame");
            EDITOR_RESERVED.store(true, Ordering::SeqCst);
            emit(&live, &Event::Tick { now_ms: 250 }).expect("a tick under the editor is accepted");
            let captured = Event::Captured { id: (b"cli_run", b"fixture::fails_when_requested"), stdout: (b"requested fixture failure", 0), stderr: (b"", 0), exit: Exit::Code(101), excerpt: UNREAD };
            emit(&live, &captured).expect("the failure block queues under the editor");
            EDITOR_RESERVED.store(false, Ordering::SeqCst);
            emit(&live, &Event::Tick { now_ms: 500 }).expect("the tick after the editor redraws");
            let mut sinks = Sinks::new();
            poll_fn(|context| sinks.poll_drain(context)).await.expect("the queue drains");
        });
        unsafe {
            libc::dup2(saved_stdout, STDOUT);
            libc::close(saved_stdout);
        }
        outcome.expect("root completes");
        drop(writer);
        let mut delivered = Vec::new();
        std::fs::File::from(reader).read_to_end(&mut delivered).expect("the pipe reads to its end");
        delivered
    }

    #[proxima::test]
    async fn a_failure_that_settles_while_the_editor_is_open_prints_no_block_and_the_frame_redraws() {
        in_single_threaded_process(|| {
            let delivered = failure_settles_while_the_editor_is_open();
            let text = std::string::String::from_utf8_lossy(&delivered);

            let frames = delivered.windows(HIDE_CURSOR.len()).filter(|window| *window == HIDE_CURSOR).count();
            assert_eq!(frames, 2, "the tick under the editor drew nothing, the first frame and the redraw did:\n{text:?}");
            assert!(position(&delivered, FAILURE_BLOCK).is_none(), "a rich run writes no failure block above its frame:\n{text:?}");
        });
    }

    #[proxima::test]
    async fn a_tick_that_arrives_after_the_close_draws_nothing() {
        in_single_threaded_process(|| {
            let closed = close_after_a_drawn_frame(false);
            let with_late_tick = close_after_a_drawn_frame(true);

            assert_eq!(with_late_tick, closed, "a late tick adds no bytes after the cursor returned:\n{:?}", std::string::String::from_utf8_lossy(&with_late_tick));
        });
    }

    fn restored_output(mouse_reporting: bool) -> Vec<u8> {
        let (reader, writer) = pipe_pair();
        let saved_stdout = unsafe { libc::fcntl(STDOUT, libc::F_DUPFD_CLOEXEC, 0) };
        assert!(saved_stdout >= 0, "stdout is duplicated for restore");
        assert!(unsafe { libc::dup2(writer.as_raw_fd(), STDOUT) } >= 0, "stdout points at the pipe");
        MOUSE_REPORTING.store(mouse_reporting, Ordering::SeqCst);
        restore_cursor();
        unsafe {
            libc::dup2(saved_stdout, STDOUT);
            libc::close(saved_stdout);
        }
        drop(writer);
        let mut delivered = Vec::new();
        std::fs::File::from(reader).read_to_end(&mut delivered).expect("the pipe reads to its end");
        delivered
    }

    #[proxima::test]
    async fn restore_cursor_writes_mouse_off_exactly_when_the_reporting_flag_is_set() {
        in_single_threaded_process(|| {
            let reporting = restored_output(true);
            let quiet = restored_output(false);

            assert_eq!(reporting, crate::render::review::MOUSE_OFF, "a set flag writes the disable sequence once:\n{:?}", std::string::String::from_utf8_lossy(&reporting));
            assert!(quiet.is_empty(), "a clear flag writes nothing:\n{:?}", std::string::String::from_utf8_lossy(&quiet));
        });
    }
}

#[cfg(all(test, unix))]
mod quit_registry_tests {
    use core::sync::atomic::Ordering;
    use std::format;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::string::String;
    use std::vec::Vec;

    use super::doc_trip_tests::doc_run;
    use super::listing_allocation_tests::with_run;
    use super::{DocPhase, EDITOR_GROUP, Key, ListingRun, RunError, release_child, spawn_list, spawn_reserved, track, track_hangup, track_leader};

    const KINDS: [&str; 5] = ["build", "doc", "listing", "test", "editor"];

    // a child that stamps its kind on the signal its kind receives, then exits: the editor takes a hangup, every other kind the interrupt the signal table sends
    fn body(kind: &str) -> String {
        let signal = if kind == "editor" { "HUP" } else { "INT" };
        format!("trap 'echo stamp-{kind}; kill $held; exit 0' {signal}; sleep 30 >/dev/null & held=$!; echo ready; wait $held")
    }

    fn await_ready(stdout: &mut File, kind: &str) {
        let mut ready = [0_u8; 6];
        stdout.read_exact(&mut ready).expect("the child announces itself");
        assert_eq!(&ready, b"ready\n", "{kind} installed its trap before the quit");
    }

    // build cargo, doc cargo, tests and the editor run real programs (cargo, the tty) that a unit test cannot hold, so a shell stands in and is spawned through the same reservation and tracked by the same calls the driver makes
    fn held(kind: &str) -> Result<(u32, File), RunError> {
        let mut child = Command::new("sh").args(["-c", &body(kind)]).stdin(Stdio::null()).stdout(Stdio::piped()).process_group(0).spawn().map_err(|_| RunError::Spawn)?;
        let mut stdout = File::from(OwnedFd::from(child.stdout.take().expect("the child's stdout is piped")));
        await_ready(&mut stdout, kind);
        Ok((child.id(), stdout))
    }

    // the listing is the one kind whose real spawn path runs a program the test chooses: spawn_list runs `<executable> --list`, so the executable is a script
    fn held_listing(run: &ListingRun<'_>) -> Result<(u32, File), RunError> {
        let directory = tempfile::tempdir().map_err(|_| RunError::Spawn)?;
        let executable = directory.path().join("listing.sh");
        std::fs::write(&executable, format!("#!/bin/sh\n{}\n", body("listing"))).map_err(|_| RunError::Spawn)?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).map_err(|_| RunError::Spawn)?;
        let path = executable.to_str().ok_or(RunError::Spawn)?;
        let (process, stdout, _) = spawn_list(&mut run.launch_storage.borrow_mut(), path, false, run, &[], directory.path().to_str().ok_or(RunError::Spawn)?)?.into_parts();
        let mut stdout = File::from(stdout);
        // the driver reads its pipes without blocking; the test waits on this one
        unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, 0) };
        await_ready(&mut stdout, "listing");
        Ok((process, stdout))
    }

    fn register(kind: &str, process: u32) {
        match kind {
            "build" => track(process).expect("a free slot tracks the child"),
            "test" | "doc" => track_leader(process).expect("a free slot tracks the group"),
            "editor" => {
                track_hangup(process).expect("a free slot tracks the group");
                EDITOR_GROUP.store(process, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    fn quit_with_every_kind_alive(untracked: &str) {
        with_run(|run| {
            let _capture = crate::status_queue::Capture::enter();
            let mut children: Vec<(&str, u32, File)> = Vec::new();
            for kind in KINDS {
                let (process, stdout) = match kind {
                    "listing" => held_listing(run),
                    _ if kind == untracked => held(kind),
                    _ => spawn_reserved(|| held(kind)),
                }
                .expect("the held child starts");
                if kind == "doc" {
                    let mut doc = doc_run(DocPhase::Starting, None);
                    doc.process_id = process;
                    doc.scope = run.root.clone();
                    run.doc.replace(Some(std::boxed::Box::new(doc)));
                }
                match kind == untracked {
                    true if kind == "listing" => release_child(process),
                    true => {}
                    false => register(kind, process),
                }
                children.push((kind, process, stdout));
            }

            run.key(Key::Char(b'q')).expect("the first q arms the quit");
            run.key(Key::Char(b'q')).expect("the second q quits");

            for (kind, process, stdout) in &mut children {
                if *kind == untracked {
                    let mut status = 0;
                    let alive = unsafe { libc::waitpid(process.cast_signed(), &raw mut status, libc::WNOHANG) };
                    assert_eq!(alive, 0, "{kind} left the registry, so nothing reached it");
                    unsafe { libc::kill(process.cast_signed(), libc::SIGKILL) };
                } else {
                    let mut stamp = String::new();
                    stdout.read_to_string(&mut stamp).expect("the stamp is readable until the child exits");
                    assert_eq!(stamp, format!("stamp-{kind}\n"), "{kind} was reached by the quit");
                }
                unsafe { libc::waitpid(process.cast_signed(), core::ptr::null_mut(), 0) };
            }
            EDITOR_GROUP.store(0, Ordering::SeqCst);
            assert!(run.root.is_fired(), "the quit fired the root once");
            assert_eq!(run.quit.get(), Some(super::QUIT_SIGNAL), "the quit was recorded where finish_run reads it");
            run.doc.borrow_mut().as_mut().expect("the doc run exists").exited(130);
            assert_eq!(run.doc.borrow().as_ref().map(|doc| doc.phase), Some(DocPhase::Interrupted), "the doc phase moved to interrupted by observing the root, with no poke from the key");
        });
    }

    #[proxima::test]
    async fn a_quit_with_one_of_every_child_kind_alive_stamps_and_reaps_all_five() {
        crate::edge_exit::fixture::in_single_threaded_process(|| quit_with_every_kind_alive(""));
    }

    #[proxima::test]
    #[case::without_the_build_cargo("build")]
    #[case::without_the_doc_cargo("doc")]
    #[case::without_a_listing("listing")]
    #[case::without_a_test("test")]
    #[case::without_the_editor("editor")]
    async fn a_kind_missing_from_the_registry_is_not_reached_while_the_rest_still_are(#[case] kind: &'static str) {
        crate::edge_exit::fixture::in_single_threaded_process(move || quit_with_every_kind_alive(kind));
    }
}
