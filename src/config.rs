//! Every setting cargo-val resolves, each with its precedence order, refusal message, and builder.

use conflaguration::SettingsSource;

use crate::line::Line;

const HARNESS_FLAG: &[u8] = b"--test-threads";
const IGNORED_FLAG: &[u8] = b"--ignored";
const INCLUDE_IGNORED_FLAG: &[u8] = b"--include-ignored";
const RUST_TEST_THREADS_KEY: &str = "RUST_TEST_THREADS";
const CONFIGURED_JOBS_KEY: &str = "CARGO_VAL_JOBS";

/// Which layer supplied a value, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CommandLine,
    Configured,
    BuildDefault,
}

/// A refused value, the layer that supplied it, and the message naming that layer's flag or key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingError {
    pub source: Source,
    message: &'static [u8],
}

impl SettingError {
    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        self.message
    }
}

/// The layers of one setting that is a parsed value: command line over a [`SettingsSource`] key over a build default.
///
/// A family contributes its key and its three refusal messages through [`Layers::setting`] and its
/// value type through `TryFrom<&[u8]>`; precedence, parsing and refusals are written here once.
/// Built from settings or in code with the same result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layers<'value> {
    key: &'static str,
    refusals: [&'static [u8]; 3],
    command_line: Option<&'value [u8]>,
    configured: Option<&'value str>,
    build_default: Option<&'value str>,
}

impl<'value> Layers<'value> {
    /// `refusals` are the messages for a refused command-line, configured, and build-default value, in that order.
    #[must_use]
    pub const fn setting(key: &'static str, refusals: [&'static [u8]; 3]) -> Self {
        Self {
            key,
            refusals,
            command_line: None,
            configured: None,
            build_default: None,
        }
    }

    #[must_use]
    pub const fn command_line(mut self, value: Option<&'value [u8]>) -> Self {
        self.command_line = value;
        self
    }

    #[must_use]
    pub const fn configured(mut self, value: Option<&'value str>) -> Self {
        self.configured = value;
        self
    }

    #[must_use]
    pub const fn build_default(mut self, value: Option<&'value str>) -> Self {
        self.build_default = value;
        self
    }

    /// Reads the setting's key from a conflaguration source.
    #[must_use]
    pub fn from_settings(
        self,
        settings: &'value impl SettingsSource,
        command_line: Option<&'value [u8]>,
    ) -> Self {
        self.command_line(command_line)
            .configured(settings.get(self.key))
    }

    /// The highest-precedence supplied text and its layer, unparsed.
    #[must_use]
    pub const fn supplied(self) -> Option<(Source, &'value [u8])> {
        match (self.command_line, self.configured, self.build_default) {
            (Some(bytes), _, _) => Some((Source::CommandLine, bytes)),
            (None, Some(text), _) => Some((Source::Configured, text.as_bytes())),
            (None, None, Some(text)) => Some((Source::BuildDefault, text.as_bytes())),
            (None, None, None) => None,
        }
    }

    /// Picks the highest-precedence supplied layer and parses only that value.
    ///
    /// # Errors
    ///
    /// Returns [`SettingError`] naming the winning layer when its value does not parse, or the
    /// build-default layer when nothing supplied a value.
    pub fn resolve<Mode: TryFrom<&'value [u8]>>(self) -> Result<(Mode, Source), SettingError> {
        let Some((source, bytes)) = self.supplied() else {
            return Err(self.refusal(Source::BuildDefault));
        };
        Mode::try_from(bytes)
            .map(|mode| (mode, source))
            .map_err(|_| self.refusal(source))
    }

    const fn refusal(self, source: Source) -> SettingError {
        let message = match source {
            Source::CommandLine => self.refusals[0],
            Source::Configured => self.refusals[1],
            Source::BuildDefault => self.refusals[2],
        };
        SettingError { source, message }
    }
}

/// Where a concurrency value came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadsSource {
    HarnessArgument,
    RustTestThreads,
    ConfiguredJobs,
    BuildDefault,
}

/// Why a supplied concurrency value was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadsFault {
    Malformed,
    Zero,
    AboveCapacity,
}

/// A refused concurrency value together with the source that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadsError {
    pub source: ThreadsSource,
    pub fault: ThreadsFault,
}

impl ThreadsError {
    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        match (self.source, self.fault) {
            (ThreadsSource::HarnessArgument, ThreadsFault::Malformed) => {
                b"cargo-val: --test-threads is not a positive integer\n"
            }
            (ThreadsSource::HarnessArgument, ThreadsFault::Zero) => {
                b"cargo-val: --test-threads must be at least 1\n"
            }
            (ThreadsSource::HarnessArgument, ThreadsFault::AboveCapacity) => {
                b"cargo-val: --test-threads exceeds the live child capacity\n"
            }
            (ThreadsSource::RustTestThreads, ThreadsFault::Malformed) => {
                b"cargo-val: RUST_TEST_THREADS is not a positive integer\n"
            }
            (ThreadsSource::RustTestThreads, ThreadsFault::Zero) => {
                b"cargo-val: RUST_TEST_THREADS must be at least 1\n"
            }
            (ThreadsSource::RustTestThreads, ThreadsFault::AboveCapacity) => {
                b"cargo-val: RUST_TEST_THREADS exceeds the live child capacity\n"
            }
            (ThreadsSource::ConfiguredJobs, ThreadsFault::Malformed) => {
                b"cargo-val: CARGO_VAL_JOBS is not a positive integer\n"
            }
            (ThreadsSource::ConfiguredJobs, ThreadsFault::Zero) => {
                b"cargo-val: CARGO_VAL_JOBS must be at least 1\n"
            }
            (ThreadsSource::ConfiguredJobs, ThreadsFault::AboveCapacity) => {
                b"cargo-val: CARGO_VAL_JOBS exceeds the live child capacity\n"
            }
            (ThreadsSource::BuildDefault, ThreadsFault::Malformed) => {
                b"cargo-val: the build-time jobs default is not a positive integer\n"
            }
            (ThreadsSource::BuildDefault, ThreadsFault::Zero) => {
                b"cargo-val: the build-time jobs default must be at least 1\n"
            }
            (ThreadsSource::BuildDefault, ThreadsFault::AboveCapacity) => {
                b"cargo-val: the build-time jobs default exceeds the live child capacity\n"
            }
        }
    }
}

/// The accepted concurrency and the source that won.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadsChoice {
    pub value: usize,
    pub source: ThreadsSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessRequest<'argument> {
    /// Set by `--ignored` or `--include-ignored`; unset leaves the configured mode in force.
    pub ignore: Option<IgnoreMode>,
    pub test_threads: Option<&'argument [u8]>,
}

/// What a run does with `#[ignore]`d tests, with libtest's flag meanings.
/// What a run does with `#[ignore]`d tests, with libtest's flag meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreMode {
    /// Ignored tests are listed and reported IGNORED, never launched.
    Skip,
    /// `--ignored`: only ignored tests run.
    Only,
    /// `--include-ignored`: ignored and regular tests run.
    Include,
}

impl TryFrom<&[u8]> for IgnoreMode {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        match text {
            b"skip" => Ok(Self::Skip),
            b"only" => Ok(Self::Only),
            b"include" => Ok(Self::Include),
            _ => Err(()),
        }
    }
}

const IGNORE_REFUSAL: &[u8] = b"cargo-val: CARGO_VAL_IGNORED must be skip, only, or include\n";

/// `CARGO_VAL_IGNORED`; the harness flags are typed by [`HarnessRequest`], so no command-line text reaches it.
pub const IGNORE: Layers<'static> =
    Layers::setting("CARGO_VAL_IGNORED", [IGNORE_REFUSAL; 3]).build_default(Some("skip"));

/// `CARGO_VAL_FILTERSET`; the command line wins, and no layer is validated here.
pub const FILTERSET: Layers<'static> = Layers::setting("CARGO_VAL_FILTERSET", [b""; 3]);

/// Where cargo val's own cargo children build, a value on the environment axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDirMode<'value> {
    /// A subtree of cargo's resolved target directory keyed by workspace, so no other cargo process shares its lock.
    Isolated,
    /// Cargo's resolved target directory unchanged: every cargo process on it shares one build lock.
    Shared,
    /// A directory the caller chose; cargo val builds in its `test` and `doc` subdirectories.
    Path(&'value [u8]),
}

impl<'value> TryFrom<&'value [u8]> for TargetDirMode<'value> {
    type Error = ();

    fn try_from(text: &'value [u8]) -> Result<Self, ()> {
        match text {
            b"" => Err(()),
            b"isolated" => Ok(Self::Isolated),
            b"shared" => Ok(Self::Shared),
            path => Ok(Self::Path(path)),
        }
    }
}

/// `CARGO_VAL_TARGET_DIR` / `--target-dir`; the build default is supplied by the caller.
pub const TARGET_DIR: Layers<'static> = Layers::setting(
    "CARGO_VAL_TARGET_DIR",
    [
        b"cargo-val: --target-dir must be isolated, shared, or a non-empty path\n",
        b"cargo-val: CARGO_VAL_TARGET_DIR must be isolated, shared, or a non-empty path\n",
        b"cargo-val: the build-time target dir default must be isolated, shared, or a non-empty path\n",
    ],
);

/// What a run does when its selection matches no tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTestsPolicy {
    Fail,
    Warn,
    Pass,
}

impl TryFrom<&[u8]> for NoTestsPolicy {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        match text {
            b"fail" => Ok(Self::Fail),
            b"warn" => Ok(Self::Warn),
            b"pass" => Ok(Self::Pass),
            _ => Err(()),
        }
    }
}

/// `CARGO_VAL_NO_TESTS` / `--no-tests`, defaulting to `fail`.
pub const NO_TESTS: Layers<'static> = Layers::setting(
    "CARGO_VAL_NO_TESTS",
    [
        b"cargo-val: --no-tests must be fail, warn, or pass\n",
        b"cargo-val: CARGO_VAL_NO_TESTS must be fail, warn, or pass\n",
        b"cargo-val: the default no-tests policy is invalid\n",
    ],
)
.build_default(Some("fail"));

/// How a test binary's selected tests are given to OS processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessMode {
    /// Contiguous groups of tests share one libtest child, so a binary launches about `jobs` children.
    Shard,
    /// One exact test per child.
    Test,
}

/// What the user asked for: a forced mode, or `auto`, which lets the target OS decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSetting {
    /// Shard on a Windows target, where process creation dominates; one test per child elsewhere.
    Auto,
    Shard,
    Test,
}

impl TryFrom<&[u8]> for ProcessSetting {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        match text {
            b"auto" => Ok(Self::Auto),
            b"shard" => Ok(Self::Shard),
            b"test" => Ok(Self::Test),
            _ => Err(()),
        }
    }
}

impl ProcessSetting {
    /// The mode this setting runs as for a target; a forced setting ignores the target.
    #[must_use]
    pub const fn mode(self, windows_target: bool) -> ProcessMode {
        match self {
            Self::Shard => ProcessMode::Shard,
            Self::Auto if windows_target => ProcessMode::Shard,
            Self::Auto | Self::Test => ProcessMode::Test,
        }
    }
}

/// `CARGO_VAL_PROCESS` / `--process`; the build default is supplied by the caller.
pub const PROCESS: Layers<'static> = Layers::setting(
    "CARGO_VAL_PROCESS",
    [
        b"cargo-val: --process must be auto, shard, or test\n",
        b"cargo-val: CARGO_VAL_PROCESS must be auto, shard, or test\n",
        b"cargo-val: the build default process setting is invalid\n",
    ],
);

/// How progress and results are written: a redrawing display for people, or append-only lines for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Rich,
    /// One complete line per event, never redrawn, never colored; what a CI log or a pipe reads back.
    Plain,
    /// Counts, failures with class and location, and a rerun command; no per-test lines, no escape bytes.
    Agent,
    /// One JSON object per line, one record per event: the agent projection's fields as keys.
    Json,
}

impl OutputMode {
    /// Whether the mode is a machine projection (agent or json): no per-test lines, no escape bytes, cargo progress withheld.
    #[must_use]
    pub const fn is_agent_like(self) -> bool {
        matches!(self, Self::Agent | Self::Json)
    }
}

/// What the user asked for: a forced mode, or `auto`, which lets the terminal and the CI environment decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSetting {
    Auto,
    Rich,
    Plain,
    Agent,
    Json,
}

impl TryFrom<&[u8]> for OutputSetting {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        match text {
            b"auto" => Ok(Self::Auto),
            b"rich" => Ok(Self::Rich),
            b"plain" => Ok(Self::Plain),
            b"agent" => Ok(Self::Agent),
            b"json" => Ok(Self::Json),
            _ => Err(()),
        }
    }
}

/// Environment variables an agent harness sets for its child processes, with the `why=` text each earns.
pub const HARNESS_MARKERS: [(&str, &str); 4] = [
    ("CLAUDECODE", "env:CLAUDECODE"),
    ("CODEX_SANDBOX", "env:CODEX_SANDBOX"),
    ("CURSOR_TRACE_ID", "env:CURSOR_TRACE_ID"),
    ("GEMINI_CLI", "env:GEMINI_CLI"),
];

fn harness_reason(environment: &impl SettingsSource) -> Option<&'static str> {
    HARNESS_MARKERS
        .iter()
        .find(|(name, _)| environment.get(name).is_some_and(|value| !value.is_empty()))
        .map(|(_, reason)| *reason)
}

impl OutputSetting {
    /// The mode this setting runs as; a forced setting ignores the terminal and the environment. `auto` picks by the first match of: an agent-harness marker, `CI` or `TERM=dumb`, an interactive terminal, else plain.
    ///
    /// `environment` supplies the harness markers, `CI` (any non-empty value) and `TERM` (`dumb`); `terminal` is whether standard output is interactive.
    #[must_use]
    pub fn mode(self, terminal: bool, environment: &impl SettingsSource) -> OutputMode {
        let in_ci = environment.get("CI").is_some_and(|value| !value.is_empty());
        let dumb = environment.get("TERM") == Some("dumb");
        match self {
            Self::Auto if harness_reason(environment).is_some() => OutputMode::Agent,
            Self::Auto if terminal && !in_ci && !dumb => OutputMode::Rich,
            Self::Auto | Self::Plain => OutputMode::Plain,
            Self::Rich => OutputMode::Rich,
            Self::Agent => OutputMode::Agent,
            Self::Json => OutputMode::Json,
        }
    }

    /// Why `mode` came out as it did: the layer that supplied a forced setting, or the first `auto` rule that matched.
    #[must_use]
    pub fn why(self, source: Source, terminal: bool, environment: &impl SettingsSource) -> &'static str {
        match (self, source) {
            (Self::Auto, _) => Self::auto_reason(terminal, environment),
            (_, Source::CommandLine) => "flag",
            (_, Source::Configured) => "env:CARGO_VAL_OUTPUT",
            (_, Source::BuildDefault) => "build-default",
        }
    }

    fn auto_reason(terminal: bool, environment: &impl SettingsSource) -> &'static str {
        if let Some(reason) = harness_reason(environment) {
            return reason;
        }
        if environment.get("CI").is_some_and(|value| !value.is_empty()) {
            return "env:CI";
        }
        if environment.get("TERM") == Some("dumb") {
            return "env:TERM";
        }
        if terminal { "tty" } else { "not-a-tty" }
    }
}

/// `CARGO_VAL_OUTPUT` / `--output`; the build default is supplied by the caller.
pub const OUTPUT: Layers<'static> = Layers::setting(
    "CARGO_VAL_OUTPUT",
    [
        b"cargo-val: --output must be auto, rich, plain, agent, or json\n",
        b"cargo-val: CARGO_VAL_OUTPUT must be auto, rich, plain, agent, or json\n",
        b"cargo-val: the build default output setting is invalid\n",
    ],
);

/// A test's declared deadline in whole seconds, or none; unknown stays unknown and is never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestTimeout {
    Unset,
    Seconds(u32),
}

impl TestTimeout {
    /// The deadline in milliseconds as the render core carries it; `None` is no declared deadline.
    #[must_use]
    pub const fn limit_ms(self) -> Option<u32> {
        match self {
            Self::Unset => None,
            Self::Seconds(seconds) => Some(seconds * 1000),
        }
    }
}

impl TryFrom<&[u8]> for TestTimeout {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        if text == b"none" {
            return Ok(Self::Unset);
        }
        let seconds = core::str::from_utf8(text).map_err(|_| ())?.parse::<u32>().map_err(|_| ())?;
        match seconds {
            1..=MAX_TIMEOUT_SECONDS => Ok(Self::Seconds(seconds)),
            _ => Err(()),
        }
    }
}

const MAX_TIMEOUT_SECONDS: u32 = u32::MAX / 1000;

/// `CARGO_VAL_TEST_TIMEOUT`: the deadline every test is declared to have, in seconds; defaults to `none`.
/// The scheduling record a `#[proxima::test(timeout = ..)]` writes (`sched_binary::Entry`) carries no timeout field, so this setting is the only deadline source.
pub const TEST_TIMEOUT: Layers<'static> = Layers::setting(
    "CARGO_VAL_TEST_TIMEOUT",
    [
        b"cargo-val: --test-timeout must be none or a whole number of seconds between 1 and 4294967\n",
        b"cargo-val: CARGO_VAL_TEST_TIMEOUT must be none or a whole number of seconds between 1 and 4294967\n",
        b"cargo-val: the default test timeout is invalid\n",
    ],
)
.build_default(Some("none"));

/// The interval between `RUNNING` heartbeats in whole seconds; at least one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatSecs(u32);

impl HeartbeatSecs {
    #[must_use]
    pub fn millis(self) -> u64 {
        u64::from(self.0) * 1000
    }
}

impl TryFrom<&[u8]> for HeartbeatSecs {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        let seconds = core::str::from_utf8(text).map_err(|_| ())?.parse::<u32>().map_err(|_| ())?;
        match seconds {
            1..=MAX_TIMEOUT_SECONDS => Ok(Self(seconds)),
            _ => Err(()),
        }
    }
}

/// `CARGO_VAL_HEARTBEAT_SECS` / `--heartbeat`: seconds between `RUNNING` lines; the build default is the `heartbeat_secs` sizing constant.
pub const HEARTBEAT: Layers<'static> = Layers::setting(
    "CARGO_VAL_HEARTBEAT_SECS",
    [
        b"cargo-val: --heartbeat must be a whole number of seconds between 1 and 4294967\n",
        b"cargo-val: CARGO_VAL_HEARTBEAT_SECS must be a whole number of seconds between 1 and 4294967\n",
        b"cargo-val: the default heartbeat is invalid\n",
    ],
);

/// Resolves the heartbeat interval in milliseconds, the sizing constant standing in as the build-default layer.
///
/// # Errors
///
/// Returns [`SettingError`] naming the winning layer when its value is not a whole number of seconds from 1 to 4294967.
pub fn heartbeat_millis(settings: &impl SettingsSource, command_line: Option<&[u8]>) -> Result<u64, SettingError> {
    let mut default = Line::<24>::new();
    default.number(crate::view_defaults::HEARTBEAT_SECS);
    let default = core::str::from_utf8(default.bytes()).ok();
    HEARTBEAT
        .build_default(default)
        .from_settings(settings, command_line)
        .resolve::<HeartbeatSecs>()
        .map(|(seconds, _)| seconds.millis())
}

/// Whether the run ends when its parent process exits; off, a run outlives its parent as a backgrounded run must.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitWithParent {
    Off,
    On,
}

impl TryFrom<&[u8]> for ExitWithParent {
    type Error = ();

    fn try_from(text: &[u8]) -> Result<Self, ()> {
        match text {
            b"on" | b"1" => Ok(Self::On),
            b"off" | b"0" => Ok(Self::Off),
            _ => Err(()),
        }
    }
}

/// `CARGO_VAL_EXIT_WITH_PARENT` / `--exit-with-parent`; the build default is supplied by the caller.
pub const EXIT_WITH_PARENT: Layers<'static> = Layers::setting(
    "CARGO_VAL_EXIT_WITH_PARENT",
    [
        b"cargo-val: --exit-with-parent takes no value\n",
        b"cargo-val: CARGO_VAL_EXIT_WITH_PARENT must be on, off, 1, or 0\n",
        b"cargo-val: the build default exit-with-parent setting is invalid\n",
    ],
);

/// The refusal when a layer turns `exit-with-parent` on where the target has no parent watch, naming that layer's flag or key.
#[must_use]
pub const fn exit_with_parent_unsupported(source: Source) -> SettingError {
    let message: &'static [u8] = match source {
        Source::CommandLine => b"cargo-val: --exit-with-parent is not supported on windows (CARGO_VAL_EXIT_WITH_PARENT)\n",
        Source::Configured => b"cargo-val: CARGO_VAL_EXIT_WITH_PARENT=on is not supported on windows\n",
        Source::BuildDefault => b"cargo-val: the build default exit-with-parent setting is not supported on windows\n",
    };
    SettingError { source, message }
}

impl<'argument> HarnessRequest<'argument> {
    /// The ignore mode a harness flag selected, else the `CARGO_VAL_IGNORED` setting, else skip.
    ///
    /// # Errors
    ///
    /// Returns [`SettingError`] when the configured value is not `skip`, `only`, or `include`.
    pub fn ignore_mode(self, settings: &impl SettingsSource) -> Result<IgnoreMode, SettingError> {
        match self.ignore {
            Some(mode) => Ok(mode),
            None => IGNORE
                .from_settings(settings, None)
                .resolve::<IgnoreMode>()
                .map(|(mode, _)| mode),
        }
    }

    /// Accepts `--ignored`, `--include-ignored` (mutually exclusive, as in libtest) and `--test-threads N` / `--test-threads=N`; the last `--test-threads` wins.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessFault`] for any other argument or a `--test-threads` with no value.
    pub fn parse(arguments: &[&'argument [u8]]) -> Result<Self, HarnessFault> {
        let mut request = Self {
            ignore: None,
            test_threads: None,
        };
        let mut remaining = arguments.iter().copied();
        while let Some(argument) = remaining.next() {
            if argument == IGNORED_FLAG && request.ignore.is_none() {
                request.ignore = Some(IgnoreMode::Only);
            } else if argument == INCLUDE_IGNORED_FLAG && request.ignore.is_none() {
                request.ignore = Some(IgnoreMode::Include);
            } else if argument == HARNESS_FLAG {
                let value = remaining.next().ok_or(HarnessFault::MissingThreadsValue)?;
                request.test_threads = Some(value);
            } else if let Some(value) = argument
                .strip_prefix(HARNESS_FLAG)
                .and_then(|rest| rest.strip_prefix(b"="))
            {
                request.test_threads = Some(value);
            } else {
                return Err(HarnessFault::Unsupported);
            }
        }
        Ok(request)
    }
}

/// Why the harness arguments were refused before any value was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessFault {
    Unsupported,
    MissingThreadsValue,
}

/// Every layer that can supply concurrency, built from settings or in code with the same result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadSources<'value> {
    harness: Option<&'value [u8]>,
    rust_test_threads: Option<&'value str>,
    configured_jobs: Option<&'value str>,
    default_jobs: usize,
}

impl<'value> ThreadSources<'value> {
    #[must_use]
    pub const fn builder(default_jobs: usize) -> Self {
        Self {
            harness: None,
            rust_test_threads: None,
            configured_jobs: None,
            default_jobs,
        }
    }

    #[must_use]
    pub const fn harness(mut self, value: Option<&'value [u8]>) -> Self {
        self.harness = value;
        self
    }

    #[must_use]
    pub const fn rust_test_threads(mut self, value: Option<&'value str>) -> Self {
        self.rust_test_threads = value;
        self
    }

    #[must_use]
    pub const fn configured_jobs(mut self, value: Option<&'value str>) -> Self {
        self.configured_jobs = value;
        self
    }

    /// Reads `RUST_TEST_THREADS` and `CARGO_VAL_JOBS` from a conflaguration source.
    #[must_use]
    pub fn from_settings(
        settings: &'value impl SettingsSource,
        harness: Option<&'value [u8]>,
        default_jobs: usize,
    ) -> Self {
        Self::builder(default_jobs)
            .harness(harness)
            .rust_test_threads(settings.get(RUST_TEST_THREADS_KEY))
            .configured_jobs(settings.get(CONFIGURED_JOBS_KEY))
    }

    /// Picks the highest-precedence supplied source and validates only that value.
    ///
    /// # Errors
    ///
    /// Returns [`ThreadsError`] naming the winning source when its value is not in `1..=capacity`.
    pub fn resolve(self, capacity: usize) -> Result<ThreadsChoice, ThreadsError> {
        let harness = self.harness.map(|bytes| {
            (
                ThreadsSource::HarnessArgument,
                core::str::from_utf8(bytes).ok(),
            )
        });
        let rust_test_threads = self
            .rust_test_threads
            .map(|text| (ThreadsSource::RustTestThreads, Some(text)));
        let configured_jobs = self
            .configured_jobs
            .map(|text| (ThreadsSource::ConfiguredJobs, Some(text)));
        let (source, text) = harness
            .or(rust_test_threads)
            .or(configured_jobs)
            .unwrap_or((ThreadsSource::BuildDefault, None));
        let value = match (source, text) {
            (ThreadsSource::BuildDefault, _) => self.default_jobs,
            (_, text) => text
                .and_then(|text| text.parse::<usize>().ok())
                .ok_or(ThreadsError {
                    source,
                    fault: ThreadsFault::Malformed,
                })?,
        };
        let fault = match value {
            0 => Some(ThreadsFault::Zero),
            above if above > capacity => Some(ThreadsFault::AboveCapacity),
            _ => None,
        };
        match fault {
            Some(fault) => Err(ThreadsError { source, fault }),
            None => Ok(ThreadsChoice { value, source }),
        }
    }
}

const FAIL_FAST_KEY: &str = "CARGO_VAL_FAIL_FAST";
const MAX_FAIL_KEY: &str = "CARGO_VAL_MAX_FAIL";

/// The command-line spelling that selected a stop policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailFastArgument<'argument> {
    FailFast,
    NoFailFast,
    MaxFail(&'argument [u8]),
}

/// Where the winning stop policy came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailFastSource {
    CommandLine,
    ConfiguredMaxFail,
    ConfiguredFailFast,
    Default,
}

/// A refused stop policy together with the source that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailFastError {
    pub source: FailFastSource,
}

impl FailFastError {
    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        match self.source {
            FailFastSource::CommandLine => {
                b"cargo-val: --max-fail is not an integer of at least 1\n"
            }
            FailFastSource::ConfiguredMaxFail => {
                b"cargo-val: CARGO_VAL_MAX_FAIL is not an integer of at least 1\n"
            }
            FailFastSource::ConfiguredFailFast => {
                b"cargo-val: CARGO_VAL_FAIL_FAST is not a boolean\n"
            }
            FailFastSource::Default => b"cargo-val: the default stop policy is invalid\n",
        }
    }
}

/// The accepted stop policy: `None` runs every selected test, `Some(count)` stops launching at `count` failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailFastChoice {
    pub max_fail: Option<usize>,
    pub source: FailFastSource,
}

/// Every layer that can supply the stop policy, built from settings or in code with the same result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailFastSources<'value> {
    command_line: Option<FailFastArgument<'value>>,
    configured_max_fail: Option<&'value str>,
    configured_fail_fast: Option<&'value str>,
}

impl<'value> FailFastSources<'value> {
    #[must_use]
    pub const fn builder() -> Self {
        Self {
            command_line: None,
            configured_max_fail: None,
            configured_fail_fast: None,
        }
    }

    #[must_use]
    pub const fn command_line(mut self, value: Option<FailFastArgument<'value>>) -> Self {
        self.command_line = value;
        self
    }

    #[must_use]
    pub const fn configured_max_fail(mut self, value: Option<&'value str>) -> Self {
        self.configured_max_fail = value;
        self
    }

    #[must_use]
    pub const fn configured_fail_fast(mut self, value: Option<&'value str>) -> Self {
        self.configured_fail_fast = value;
        self
    }

    /// Reads `CARGO_VAL_MAX_FAIL` and `CARGO_VAL_FAIL_FAST` from a conflaguration source.
    #[must_use]
    pub fn from_settings(
        settings: &'value impl SettingsSource,
        command_line: Option<FailFastArgument<'value>>,
    ) -> Self {
        Self::builder()
            .command_line(command_line)
            .configured_max_fail(settings.get(MAX_FAIL_KEY))
            .configured_fail_fast(settings.get(FAIL_FAST_KEY))
    }

    /// Picks the highest-precedence supplied source and validates only that value.
    ///
    /// # Errors
    ///
    /// Returns [`FailFastError`] naming the winning source when its value is malformed or below 1.
    pub fn resolve(self) -> Result<FailFastChoice, FailFastError> {
        let choice = |max_fail, source| Ok(FailFastChoice { max_fail, source });
        if let Some(argument) = self.command_line {
            return match argument {
                FailFastArgument::FailFast => choice(Some(1), FailFastSource::CommandLine),
                FailFastArgument::NoFailFast => choice(None, FailFastSource::CommandLine),
                FailFastArgument::MaxFail(bytes) => core::str::from_utf8(bytes)
                    .ok()
                    .and_then(positive)
                    .map(Some)
                    .ok_or(FailFastError {
                        source: FailFastSource::CommandLine,
                    })
                    .and_then(|max_fail| choice(max_fail, FailFastSource::CommandLine)),
            };
        }
        if let Some(text) = self.configured_max_fail {
            return positive(text)
                .ok_or(FailFastError {
                    source: FailFastSource::ConfiguredMaxFail,
                })
                .and_then(|count| choice(Some(count), FailFastSource::ConfiguredMaxFail));
        }
        if let Some(text) = self.configured_fail_fast {
            return text
                .parse::<bool>()
                .map_err(|_| FailFastError {
                    source: FailFastSource::ConfiguredFailFast,
                })
                .and_then(|enabled| {
                    choice(enabled.then_some(1), FailFastSource::ConfiguredFailFast)
                });
        }
        choice(Some(1), FailFastSource::Default)
    }
}

fn positive(text: &str) -> Option<usize> {
    text.parse::<usize>().ok().filter(|count| *count >= 1)
}

///
/// The multiple takes up to three decimals; anything else is refused.
#[must_use]
pub fn shard_jobs(expression: &str, cores: usize) -> Option<usize> {
    let (number, multiplier) = match expression.strip_suffix('x') {
        Some(number) => (number, true),
        None => (expression, false),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() || fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole: usize = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    if !multiplier {
        return (fraction.is_empty() && whole > 0).then_some(whole);
    }
    let milli: usize = format_milli(fraction)?;
    Some((cores.checked_mul(whole.checked_mul(1000)?.checked_add(milli)?)? / 1000).max(1))
}

fn format_milli(fraction: &str) -> Option<usize> {
    let digits: usize = if fraction.is_empty() { 0 } else { fraction.parse().ok()? };
    Some(digits * 10_usize.pow(3 - u32::try_from(fraction.len()).ok()?))
}

#[cfg(test)]
mod shard_jobs_tests {
    use super::shard_jobs;

    #[proxima::test]
    async fn counts_and_multiples_of_the_core_count_resolve_to_children() {
        let table = [
            ("2x", 8, Some(16)),
            ("1x", 8, Some(8)),
            ("0.5x", 8, Some(4)),
            ("0.5x", 1, Some(1)),
            ("1.5x", 6, Some(9)),
            ("6", 64, Some(6)),
            ("0", 8, None),
            ("x", 8, None),
            ("2.5", 8, None),
            ("0.0001x", 8, None),
            ("two", 8, None),
        ];
        for (expression, cores, expected) in table {
            assert_eq!(shard_jobs(expression, cores), expected, "{expression} on {cores} cores");
        }
    }
}

#[cfg(test)]
mod layers_tests {
    use super::*;
    use conflaguration::BorrowedSettings;

    type Resolved<Mode> = Result<(Mode, Source), SettingError>;
    type AutoRow = (bool, &'static [(&'static str, &'static str)], OutputMode);
    const TERMINAL: bool = true;
    const PIPE: bool = false;

    fn refused(layers: Layers<'_>, source: Source) -> SettingError {
        layers.refusal(source)
    }

    fn supplied_text(layers: Layers<'_>) -> Option<&[u8]> {
        layers.supplied().map(|(_, text)| text)
    }

    #[proxima::test]
    async fn no_tests_precedence_table() {
        let table: [(Layers<'_>, Resolved<NoTestsPolicy>); 6] = [
            (NO_TESTS, Ok((NoTestsPolicy::Fail, Source::BuildDefault))),
            (NO_TESTS.configured(Some("warn")), Ok((NoTestsPolicy::Warn, Source::Configured))),
            (NO_TESTS.command_line(Some(b"pass")), Ok((NoTestsPolicy::Pass, Source::CommandLine))),
            (
                NO_TESTS.command_line(Some(b"pass")).configured(Some("warn")),
                Ok((NoTestsPolicy::Pass, Source::CommandLine)),
            ),
            (NO_TESTS.command_line(Some(b"ignore")), Err(refused(NO_TESTS, Source::CommandLine))),
            (NO_TESTS.configured(Some("")), Err(refused(NO_TESTS, Source::Configured))),
        ];
        for (layers, expected) in table {
            assert_eq!(layers.resolve::<NoTestsPolicy>(), expected, "{layers:?}");
        }
    }

    #[proxima::test]
    async fn refusals_name_the_layer_that_supplied_the_value() {
        assert_eq!(
            NO_TESTS.command_line(Some(b"ignore")).resolve::<NoTestsPolicy>().map_err(SettingError::message),
            Err(&b"cargo-val: --no-tests must be fail, warn, or pass\n"[..])
        );
        assert_eq!(
            NO_TESTS.configured(Some("")).resolve::<NoTestsPolicy>().map_err(SettingError::message),
            Err(&b"cargo-val: CARGO_VAL_NO_TESTS must be fail, warn, or pass\n"[..])
        );
        assert_eq!(
            NO_TESTS.build_default(Some("maybe")).resolve::<NoTestsPolicy>().map_err(SettingError::message),
            Err(&b"cargo-val: the default no-tests policy is invalid\n"[..])
        );
    }

    #[proxima::test]
    async fn a_setting_with_no_layer_and_no_default_is_refused_as_a_build_default() {
        let bare = TARGET_DIR.build_default(None);

        assert_eq!(bare.resolve::<TargetDirMode<'_>>(), Err(refused(TARGET_DIR, Source::BuildDefault)));
    }

    #[proxima::test]
    async fn process_precedence_is_command_line_then_environment_then_build_default() {
        let table: [(Layers<'_>, Resolved<ProcessSetting>); 6] = [
            (PROCESS.build_default(Some("auto")), Ok((ProcessSetting::Auto, Source::BuildDefault))),
            (PROCESS.build_default(Some("test")), Ok((ProcessSetting::Test, Source::BuildDefault))),
            (
                PROCESS.build_default(Some("auto")).configured(Some("shard")),
                Ok((ProcessSetting::Shard, Source::Configured)),
            ),
            (
                PROCESS.build_default(Some("auto")).configured(Some("shard")).command_line(Some(b"test")),
                Ok((ProcessSetting::Test, Source::CommandLine)),
            ),
            (
                PROCESS.build_default(Some("auto")).configured(Some("batch")),
                Err(refused(PROCESS, Source::Configured)),
            ),
            (
                PROCESS.build_default(Some("auto")).command_line(Some(&[0xff, 0xfe])),
                Err(refused(PROCESS, Source::CommandLine)),
            ),
        ];
        for (layers, expected) in table {
            assert_eq!(layers.resolve::<ProcessSetting>(), expected, "{layers:?}");
        }
    }

    #[proxima::test]
    async fn target_dir_command_line_beats_setting_beats_build_default_and_each_variant_parses() {
        let isolated = TARGET_DIR.build_default(Some("isolated"));
        let table: [(Layers<'_>, Resolved<TargetDirMode<'_>>); 8] = [
            (isolated, Ok((TargetDirMode::Isolated, Source::BuildDefault))),
            (TARGET_DIR.build_default(Some("shared")), Ok((TargetDirMode::Shared, Source::BuildDefault))),
            (isolated.configured(Some("shared")), Ok((TargetDirMode::Shared, Source::Configured))),
            (
                isolated.configured(Some("/var/cache/val")),
                Ok((TargetDirMode::Path(b"/var/cache/val"), Source::Configured)),
            ),
            (
                isolated.configured(Some("shared")).command_line(Some(b"target/own")),
                Ok((TargetDirMode::Path(b"target/own"), Source::CommandLine)),
            ),
            (
                isolated.configured(Some("/var/cache/val")).command_line(Some(b"isolated")),
                Ok((TargetDirMode::Isolated, Source::CommandLine)),
            ),
            (isolated.configured(Some("")), Err(refused(TARGET_DIR, Source::Configured))),
            (TARGET_DIR.build_default(Some("")), Err(refused(TARGET_DIR, Source::BuildDefault))),
        ];
        for (layers, expected) in table {
            assert_eq!(layers.resolve::<TargetDirMode<'_>>(), expected, "{layers:?}");
        }
    }

    #[proxima::test]
    async fn the_built_in_default_resolves_to_shared_from_the_build_default_source() {
        let layers = TARGET_DIR.build_default(Some(crate::build_defaults::TARGET_DIR));

        assert_eq!(layers.resolve::<TargetDirMode<'_>>(), Ok((TargetDirMode::Shared, Source::BuildDefault)));
    }

    #[test]
    fn output_precedence_is_command_line_then_environment_then_build_default() {
        let auto = OUTPUT.build_default(Some("auto"));
        let table: [(Layers<'_>, Resolved<OutputSetting>); 6] = [
            (auto, Ok((OutputSetting::Auto, Source::BuildDefault))),
            (OUTPUT.build_default(Some("plain")), Ok((OutputSetting::Plain, Source::BuildDefault))),
            (auto.configured(Some("rich")), Ok((OutputSetting::Rich, Source::Configured))),
            (
                auto.configured(Some("rich")).command_line(Some(b"plain")),
                Ok((OutputSetting::Plain, Source::CommandLine)),
            ),
            (auto.configured(Some("fancy")), Err(refused(OUTPUT, Source::Configured))),
            (auto.command_line(Some(&[0xff, 0xfe])), Err(refused(OUTPUT, Source::CommandLine))),
        ];
        for (layers, expected) in table {
            assert_eq!(layers.resolve::<OutputSetting>(), expected, "{layers:?}");
        }
    }

    #[test]
    fn auto_resolves_from_the_terminal_and_the_ci_environment() {
        let table: [AutoRow; 9] = [
            (TERMINAL, &[], OutputMode::Rich),
            (TERMINAL, &[("TERM", "xterm-256color")], OutputMode::Rich),
            (PIPE, &[], OutputMode::Plain),
            (TERMINAL, &[("CI", "true")], OutputMode::Plain),
            (TERMINAL, &[("CI", "")], OutputMode::Rich),
            (TERMINAL, &[("TERM", "dumb")], OutputMode::Plain),
            (PIPE, &[("TERM", "xterm-256color")], OutputMode::Plain),
            (TERMINAL, &[("GITHUB_ACTIONS", "true")], OutputMode::Rich),
            (PIPE, &[("CI", "1"), ("TERM", "dumb")], OutputMode::Plain),
        ];
        for (terminal, pairs, expected) in table {
            let environment = BorrowedSettings::new(pairs);
            assert_eq!(OutputSetting::Auto.mode(terminal, &environment), expected, "terminal={terminal} {pairs:?}");
        }
    }

    #[test]
    fn a_forced_output_setting_ignores_the_terminal_and_the_environment() {
        let ci = [("CI", "true"), ("TERM", "dumb")];
        let environment = BorrowedSettings::new(&ci);
        for terminal in [TERMINAL, PIPE] {
            assert_eq!(OutputSetting::Rich.mode(terminal, &environment), OutputMode::Rich);
            assert_eq!(OutputSetting::Plain.mode(terminal, &environment), OutputMode::Plain);
        }
    }

    #[test]
    fn output_settings_and_builder_resolve_the_same_choice() {
        let pairs = [("CARGO_VAL_OUTPUT", "plain")];
        let source = BorrowedSettings::new(&pairs);
        let auto = OUTPUT.build_default(Some("auto"));

        let from_settings = auto.from_settings(&source, None);
        let from_builder = auto.configured(Some("plain"));

        assert_eq!(from_settings, from_builder);
        assert_eq!(
            from_settings.resolve::<OutputSetting>().map(|(setting, _)| setting.mode(TERMINAL, &source)),
            Ok(OutputMode::Plain)
        );
    }

    #[test]
    fn exit_with_parent_precedence_is_command_line_then_environment_then_build_default() {
        let off = EXIT_WITH_PARENT.build_default(Some("off"));
        let table: [(Layers<'_>, Resolved<ExitWithParent>); 6] = [
            (off, Ok((ExitWithParent::Off, Source::BuildDefault))),
            (EXIT_WITH_PARENT.build_default(Some("on")), Ok((ExitWithParent::On, Source::BuildDefault))),
            (off.configured(Some("1")), Ok((ExitWithParent::On, Source::Configured))),
            (off.configured(Some("1")).command_line(Some(b"0")), Ok((ExitWithParent::Off, Source::CommandLine))),
            (off.configured(Some("yes")), Err(refused(EXIT_WITH_PARENT, Source::Configured))),
            (EXIT_WITH_PARENT.build_default(Some("maybe")), Err(refused(EXIT_WITH_PARENT, Source::BuildDefault))),
        ];
        for (layers, expected) in table {
            assert_eq!(layers.resolve::<ExitWithParent>(), expected, "{layers:?}");
        }
    }

    #[test]
    fn an_unsupported_exit_with_parent_names_what_was_refused_and_the_layer_that_supplied_it() {
        let table: [(Source, &[u8]); 3] = [
            (Source::CommandLine, b"cargo-val: --exit-with-parent is not supported on windows (CARGO_VAL_EXIT_WITH_PARENT)\n"),
            (Source::Configured, b"cargo-val: CARGO_VAL_EXIT_WITH_PARENT=on is not supported on windows\n"),
            (Source::BuildDefault, b"cargo-val: the build default exit-with-parent setting is not supported on windows\n"),
        ];
        for (source, message) in table {
            let error = exit_with_parent_unsupported(source);
            assert_eq!((error.source, error.message()), (source, message));
        }
    }

    #[test]
    fn exit_with_parent_settings_and_builder_resolve_the_same_choice() {
        let pairs = [("CARGO_VAL_EXIT_WITH_PARENT", "1")];
        let source = BorrowedSettings::new(&pairs);
        let off = EXIT_WITH_PARENT.build_default(Some("off"));

        let from_settings = off.from_settings(&source, None);
        let from_builder = off.configured(Some("1"));

        assert_eq!(from_settings, from_builder);
        assert_eq!(from_settings.resolve::<ExitWithParent>(), Ok((ExitWithParent::On, Source::Configured)));
    }

    #[proxima::test]
    async fn auto_shards_on_a_windows_target_and_runs_one_test_per_child_on_unix() {
        assert_eq!(ProcessSetting::Auto.mode(true), ProcessMode::Shard);
        assert_eq!(ProcessSetting::Auto.mode(false), ProcessMode::Test);
    }

    #[proxima::test]
    async fn a_forced_setting_overrides_auto_on_every_target() {
        for windows_target in [true, false] {
            assert_eq!(ProcessSetting::Shard.mode(windows_target), ProcessMode::Shard);
            assert_eq!(ProcessSetting::Test.mode(windows_target), ProcessMode::Test);
        }
    }

    #[proxima::test]
    async fn settings_and_builder_construct_the_same_layers_for_every_family() {
        let pairs = [
            ("CARGO_VAL_PROCESS", "shard"),
            ("CARGO_VAL_NO_TESTS", "warn"),
            ("CARGO_VAL_TARGET_DIR", "shared"),
            ("CARGO_VAL_IGNORED", "include"),
            ("CARGO_VAL_FILTERSET", "test(alpha)"),
            ("CARGO_VAL_TEST_TIMEOUT", "120"),
            ("CARGO_VAL_HEARTBEAT_SECS", "3"),
        ];
        let settings = BorrowedSettings::new(&pairs);
        let empty = BorrowedSettings::new(&[]);

        assert_eq!(
            PROCESS.build_default(Some("auto")).from_settings(&settings, None),
            PROCESS.build_default(Some("auto")).configured(Some("shard"))
        );
        assert_eq!(NO_TESTS.from_settings(&settings, None), NO_TESTS.configured(Some("warn")));
        assert_eq!(
            TARGET_DIR.build_default(Some("isolated")).from_settings(&empty, Some(b"out")),
            TARGET_DIR.build_default(Some("isolated")).command_line(Some(b"out"))
        );
        assert_eq!(IGNORE.from_settings(&settings, None), IGNORE.configured(Some("include")));
        assert_eq!(FILTERSET.from_settings(&settings, None), FILTERSET.configured(Some("test(alpha)")));
        assert_eq!(TEST_TIMEOUT.from_settings(&settings, None), TEST_TIMEOUT.configured(Some("120")));
        assert_eq!(
            TEST_TIMEOUT.from_settings(&settings, None).resolve::<TestTimeout>(),
            Ok((TestTimeout::Seconds(120), Source::Configured))
        );
        assert_eq!(TEST_TIMEOUT.from_settings(&empty, None).resolve::<TestTimeout>(), Ok((TestTimeout::Unset, Source::BuildDefault)));
        assert_eq!(TestTimeout::Seconds(120).limit_ms(), Some(120_000));
        assert_eq!(TestTimeout::Unset.limit_ms(), None);
        assert_eq!(TEST_TIMEOUT.from_settings(&empty, Some(b"90")).resolve::<TestTimeout>(), Ok((TestTimeout::Seconds(90), Source::CommandLine)));
        assert_eq!(TEST_TIMEOUT.from_settings(&settings, Some(b"none")).resolve::<TestTimeout>(), Ok((TestTimeout::Unset, Source::CommandLine)));
        assert_eq!(HEARTBEAT.from_settings(&settings, None), HEARTBEAT.configured(Some("3")));
        assert_eq!(heartbeat_millis(&settings, None), Ok(3_000));
        assert_eq!(heartbeat_millis(&settings, Some(b"1")), Ok(1_000));
        assert_eq!(heartbeat_millis(&empty, None), Ok(u64::try_from(crate::view_defaults::HEARTBEAT_SECS).expect("the sizing constant fits u64") * 1000));
        for refused_text in ["0", "-1", "often", "4294968", ""] {
            assert!(heartbeat_millis(&empty, Some(refused_text.as_bytes())).is_err(), "{refused_text:?}");
            assert_eq!(
                HEARTBEAT.configured(Some(refused_text)).resolve::<HeartbeatSecs>().map_err(SettingError::message),
                Err(HEARTBEAT.refusal(Source::Configured).message())
            );
        }
        for refused_text in ["0", "-3", "soon", "4294968", ""] {
            assert!(TEST_TIMEOUT.configured(Some(refused_text)).resolve::<TestTimeout>().is_err(), "{refused_text:?}");
        }
        assert_eq!(
            PROCESS.build_default(Some("auto")).from_settings(&settings, None).resolve::<ProcessSetting>(),
            Ok((ProcessSetting::Shard, Source::Configured))
        );
        assert_eq!(
            NO_TESTS.from_settings(&settings, Some(b"pass")).resolve::<NoTestsPolicy>(),
            Ok((NoTestsPolicy::Pass, Source::CommandLine))
        );
    }

    #[proxima::test]
    async fn harness_flags_select_the_ignore_mode_and_beat_the_setting() {
        let include: [&[u8]; 1] = [b"--include-ignored"];
        assert_eq!(
            HarnessRequest::parse(&include).map(|request| request.ignore),
            Ok(Some(IgnoreMode::Include))
        );
        let table: [(Layers<'_>, Resolved<IgnoreMode>); 4] = [
            (IGNORE, Ok((IgnoreMode::Skip, Source::BuildDefault))),
            (IGNORE.configured(Some("only")), Ok((IgnoreMode::Only, Source::Configured))),
            (IGNORE.configured(Some("include")), Ok((IgnoreMode::Include, Source::Configured))),
            (IGNORE.configured(Some("all")), Err(refused(IGNORE, Source::Configured))),
        ];
        for (layers, expected) in table {
            assert_eq!(layers.resolve::<IgnoreMode>(), expected, "{layers:?}");
        }
    }

    #[proxima::test]
    async fn a_harness_flag_beats_the_ignored_setting_and_the_setting_beats_skip() {
        let settings = BorrowedSettings::new(&[("CARGO_VAL_IGNORED", "include")]);
        let only = HarnessRequest::parse(&[&b"--ignored"[..]]).expect("--ignored is a supported harness flag");
        let none = HarnessRequest::parse(&[]).expect("no harness arguments is valid");
        let unset = BorrowedSettings::new(&[]);
        let invalid = BorrowedSettings::new(&[("CARGO_VAL_IGNORED", "all")]);

        assert_eq!(only.ignore_mode(&settings), Ok(IgnoreMode::Only));
        assert_eq!(none.ignore_mode(&settings), Ok(IgnoreMode::Include));
        assert_eq!(none.ignore_mode(&unset), Ok(IgnoreMode::Skip));
        assert_eq!(none.ignore_mode(&invalid), Err(refused(IGNORE, Source::Configured)));
    }

    #[proxima::test]
    async fn filterset_command_line_wins_over_the_setting_and_nothing_supplied_is_none() {
        let pairs = [("CARGO_VAL_FILTERSET", "test(alpha)")];
        let settings = BorrowedSettings::new(&pairs);

        assert_eq!(supplied_text(FILTERSET.from_settings(&settings, None)), Some(&b"test(alpha)"[..]));
        assert_eq!(supplied_text(FILTERSET.from_settings(&settings, Some(b"all()"))), Some(&b"all()"[..]));
        assert_eq!(supplied_text(FILTERSET), None);
    }
}

#[cfg(test)]
mod concurrency_tests {
    use super::*;
    use conflaguration::BorrowedSettings;

    const CAPACITY: usize = 64;
    const DEFAULT: usize = 8;

    fn chosen(value: usize, source: ThreadsSource) -> ThreadsChoice {
        ThreadsChoice { value, source }
    }

    fn refused(source: ThreadsSource, fault: ThreadsFault) -> Result<ThreadsChoice, ThreadsError> {
        Err(ThreadsError { source, fault })
    }

    #[proxima::test]
    async fn c68_precedence_table_resolves_each_source_alone_and_in_combination() {
        let table: [(ThreadSources<'_>, Result<ThreadsChoice, ThreadsError>); 9] = [
            (
                ThreadSources::builder(DEFAULT),
                Ok(chosen(DEFAULT, ThreadsSource::BuildDefault)),
            ),
            (
                ThreadSources::builder(DEFAULT).configured_jobs(Some("5")),
                Ok(chosen(5, ThreadsSource::ConfiguredJobs)),
            ),
            (
                ThreadSources::builder(DEFAULT).rust_test_threads(Some("3")),
                Ok(chosen(3, ThreadsSource::RustTestThreads)),
            ),
            (
                ThreadSources::builder(DEFAULT).harness(Some(b"2")),
                Ok(chosen(2, ThreadsSource::HarnessArgument)),
            ),
            (
                ThreadSources::builder(DEFAULT)
                    .rust_test_threads(Some("3"))
                    .configured_jobs(Some("5")),
                Ok(chosen(3, ThreadsSource::RustTestThreads)),
            ),
            (
                ThreadSources::builder(DEFAULT)
                    .harness(Some(b"2"))
                    .rust_test_threads(Some("3"))
                    .configured_jobs(Some("5")),
                Ok(chosen(2, ThreadsSource::HarnessArgument)),
            ),
            (
                ThreadSources::builder(DEFAULT).harness(Some(b"0")),
                refused(ThreadsSource::HarnessArgument, ThreadsFault::Zero),
            ),
            (
                ThreadSources::builder(DEFAULT).rust_test_threads(Some("65")),
                refused(ThreadsSource::RustTestThreads, ThreadsFault::AboveCapacity),
            ),
            (
                ThreadSources::builder(DEFAULT).harness(Some(b"x")),
                refused(ThreadsSource::HarnessArgument, ThreadsFault::Malformed),
            ),
        ];

        for (sources, expected) in table {
            assert_eq!(sources.resolve(CAPACITY), expected, "{sources:?}");
        }
    }

    #[proxima::test]
    async fn c68_capacity_boundaries_and_every_source_fault_name_the_supplier() {
        let capacity_edge = ThreadSources::builder(DEFAULT).configured_jobs(Some("64"));
        assert_eq!(
            capacity_edge.resolve(CAPACITY),
            Ok(chosen(64, ThreadsSource::ConfiguredJobs))
        );
        let zero_config = ThreadSources::builder(DEFAULT).configured_jobs(Some("0"));
        assert_eq!(
            zero_config.resolve(CAPACITY),
            refused(ThreadsSource::ConfiguredJobs, ThreadsFault::Zero)
        );
        let zero_default = ThreadSources::builder(0);
        assert_eq!(
            zero_default.resolve(CAPACITY),
            refused(ThreadsSource::BuildDefault, ThreadsFault::Zero)
        );
        let negative_environment = ThreadSources::builder(DEFAULT).rust_test_threads(Some("-1"));
        assert_eq!(
            negative_environment.resolve(CAPACITY),
            refused(ThreadsSource::RustTestThreads, ThreadsFault::Malformed)
        );
        let invalid_utf8 = ThreadSources::builder(DEFAULT).harness(Some(&[0xff, 0xfe]));
        assert_eq!(
            invalid_utf8.resolve(CAPACITY),
            refused(ThreadsSource::HarnessArgument, ThreadsFault::Malformed)
        );
        let shadowed_fault = ThreadSources::builder(DEFAULT)
            .harness(Some(b"2"))
            .rust_test_threads(Some("0"));
        assert_eq!(
            shadowed_fault.resolve(CAPACITY),
            Ok(chosen(2, ThreadsSource::HarnessArgument))
        );
    }

    #[proxima::test]
    async fn c68_harness_request_accepts_ignored_and_both_test_threads_forms() {
        let separated: [&[u8]; 3] = [b"--ignored", b"--test-threads", b"4"];
        let joined: [&[u8]; 1] = [b"--test-threads=1"];
        let empty: [&[u8]; 0] = [];

        assert_eq!(
            HarnessRequest::parse(&separated),
            Ok(HarnessRequest {
                ignore: Some(IgnoreMode::Only),
                test_threads: Some(b"4"),
            })
        );
        assert_eq!(
            HarnessRequest::parse(&joined),
            Ok(HarnessRequest {
                ignore: None,
                test_threads: Some(b"1"),
            })
        );
        assert_eq!(
            HarnessRequest::parse(&empty),
            Ok(HarnessRequest {
                ignore: None,
                test_threads: None,
            })
        );
    }

    #[proxima::test]
    async fn c68_harness_request_rejects_every_other_libtest_flag_and_a_dangling_value() {
        let rejected: [&[u8]; 8] = [
            b"--nocapture",
            b"--exact",
            b"--include-ignored=true",
            b"--skip",
            b"--quiet",
            b"--test-threadsX",
            b"--ignored=true",
            b"fixture::passes",
        ];
        for argument in rejected {
            assert_eq!(
                HarnessRequest::parse(&[argument]),
                Err(HarnessFault::Unsupported),
                "{}",
                core::str::from_utf8(argument).unwrap_or("non-utf8")
            );
        }
        let duplicate_ignored: [&[u8]; 2] = [b"--ignored", b"--ignored"];
        assert_eq!(
            HarnessRequest::parse(&duplicate_ignored),
            Err(HarnessFault::Unsupported)
        );
        let both: [&[u8]; 2] = [b"--ignored", b"--include-ignored"];
        assert_eq!(HarnessRequest::parse(&both), Err(HarnessFault::Unsupported));
        let dangling: [&[u8]; 1] = [b"--test-threads"];
        assert_eq!(
            HarnessRequest::parse(&dangling),
            Err(HarnessFault::MissingThreadsValue)
        );
    }

    #[proxima::test]
    async fn c68_settings_and_builder_construct_the_same_sources() {
        let pairs = [
            ("RUST_TEST_THREADS", "3"),
            ("CARGO_VAL_JOBS", "5"),
            ("CARGO_VAL_COLOR", "never"),
        ];
        let settings = BorrowedSettings::new(&pairs);
        let harness: Option<&[u8]> = Some(b"2");

        let from_settings = ThreadSources::from_settings(&settings, harness, DEFAULT);
        let from_builder = ThreadSources::builder(DEFAULT)
            .harness(harness)
            .rust_test_threads(Some("3"))
            .configured_jobs(Some("5"));

        assert_eq!(from_settings, from_builder);
        assert_eq!(
            from_settings.resolve(CAPACITY),
            from_builder.resolve(CAPACITY)
        );
        assert_eq!(
            from_settings.resolve(CAPACITY),
            Ok(chosen(2, ThreadsSource::HarnessArgument))
        );

        let without_harness = ThreadSources::from_settings(&settings, None, DEFAULT);
        assert_eq!(
            without_harness.resolve(CAPACITY),
            Ok(chosen(3, ThreadsSource::RustTestThreads))
        );
        let empty = BorrowedSettings::new(&[]);
        assert_eq!(
            ThreadSources::from_settings(&empty, None, DEFAULT),
            ThreadSources::builder(DEFAULT)
        );
    }
}

#[cfg(test)]
mod fail_fast_tests {
    use super::*;
    use conflaguration::BorrowedSettings;

    fn chosen(max_fail: Option<usize>, source: FailFastSource) -> FailFastChoice {
        FailFastChoice { max_fail, source }
    }

    fn refused(source: FailFastSource) -> Result<FailFastChoice, FailFastError> {
        Err(FailFastError { source })
    }

    #[proxima::test]
    async fn c68_stop_policy_precedence_table() {
        let table: [(FailFastSources<'_>, Result<FailFastChoice, FailFastError>); 10] = [
            (
                FailFastSources::builder(),
                Ok(chosen(Some(1), FailFastSource::Default)),
            ),
            (
                FailFastSources::builder().command_line(Some(FailFastArgument::NoFailFast)),
                Ok(chosen(None, FailFastSource::CommandLine)),
            ),
            (
                FailFastSources::builder().command_line(Some(FailFastArgument::FailFast)),
                Ok(chosen(Some(1), FailFastSource::CommandLine)),
            ),
            (
                FailFastSources::builder().command_line(Some(FailFastArgument::MaxFail(b"2"))),
                Ok(chosen(Some(2), FailFastSource::CommandLine)),
            ),
            (
                FailFastSources::builder().configured_max_fail(Some("3")),
                Ok(chosen(Some(3), FailFastSource::ConfiguredMaxFail)),
            ),
            (
                FailFastSources::builder().configured_fail_fast(Some("false")),
                Ok(chosen(None, FailFastSource::ConfiguredFailFast)),
            ),
            (
                FailFastSources::builder()
                    .configured_max_fail(Some("3"))
                    .configured_fail_fast(Some("false")),
                Ok(chosen(Some(3), FailFastSource::ConfiguredMaxFail)),
            ),
            (
                FailFastSources::builder()
                    .command_line(Some(FailFastArgument::FailFast))
                    .configured_max_fail(Some("3"))
                    .configured_fail_fast(Some("false")),
                Ok(chosen(Some(1), FailFastSource::CommandLine)),
            ),
            (
                FailFastSources::builder().command_line(Some(FailFastArgument::MaxFail(b"0"))),
                refused(FailFastSource::CommandLine),
            ),
            (
                FailFastSources::builder().configured_fail_fast(Some("maybe")),
                refused(FailFastSource::ConfiguredFailFast),
            ),
        ];

        for (sources, expected) in table {
            assert_eq!(sources.resolve(), expected, "{sources:?}");
        }
    }

    #[proxima::test]
    async fn c68_malformed_max_fail_names_the_supplier() {
        let command_line =
            FailFastSources::builder().command_line(Some(FailFastArgument::MaxFail(b"x")));
        let configured = FailFastSources::builder().configured_max_fail(Some("0"));
        let negative =
            FailFastSources::builder().command_line(Some(FailFastArgument::MaxFail(b"-2")));

        assert_eq!(command_line.resolve(), refused(FailFastSource::CommandLine));
        assert_eq!(
            configured.resolve(),
            refused(FailFastSource::ConfiguredMaxFail)
        );
        assert_eq!(negative.resolve(), refused(FailFastSource::CommandLine));
    }

    #[proxima::test]
    async fn c68_settings_and_builder_construct_the_same_stop_policy() {
        let pairs = [
            ("CARGO_VAL_MAX_FAIL", "4"),
            ("CARGO_VAL_FAIL_FAST", "false"),
        ];
        let settings = BorrowedSettings::new(&pairs);

        let from_settings = FailFastSources::from_settings(&settings, None);
        let from_builder = FailFastSources::builder()
            .configured_max_fail(Some("4"))
            .configured_fail_fast(Some("false"));

        assert_eq!(from_settings, from_builder);
        assert_eq!(
            from_settings.resolve(),
            Ok(chosen(Some(4), FailFastSource::ConfiguredMaxFail))
        );
        let overridden =
            FailFastSources::from_settings(&settings, Some(FailFastArgument::NoFailFast));
        assert_eq!(
            overridden.resolve(),
            Ok(chosen(None, FailFastSource::CommandLine))
        );
    }
}

