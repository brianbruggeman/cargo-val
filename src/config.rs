//! Every setting cargo-val resolves, each with its precedence order, refusal message, and builder.

use conflaguration::SettingsSource;

const HARNESS_FLAG: &[u8] = b"--test-threads";
const IGNORED_FLAG: &[u8] = b"--ignored";
const INCLUDE_IGNORED_FLAG: &[u8] = b"--include-ignored";
const IGNORED_KEY: &str = "CARGO_VAL_IGNORED";
const FILTERSET_KEY: &str = "CARGO_VAL_FILTERSET";
const RUST_TEST_THREADS_KEY: &str = "RUST_TEST_THREADS";
const CONFIGURED_JOBS_KEY: &str = "CARGO_VAL_JOBS";

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

/// Harness arguments after `--` that cargo val understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessRequest<'argument> {
    /// Set by `--ignored` or `--include-ignored`; unset leaves the configured mode in force.
    pub ignore: Option<IgnoreMode>,
    pub test_threads: Option<&'argument [u8]>,
}

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

impl IgnoreMode {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "skip" => Some(Self::Skip),
            "only" => Some(Self::Only),
            "include" => Some(Self::Include),
            _ => None,
        }
    }
}

/// Where the ignore mode came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreSource {
    HarnessArgument,
    Configured,
    Default,
}

/// A refused `CARGO_VAL_IGNORED` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IgnoreError;

impl IgnoreError {
    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        b"cargo-val: CARGO_VAL_IGNORED must be skip, only, or include\n"
    }
}

/// Every layer that can supply the ignore mode, built from settings or in code with the same result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IgnoreSources<'value> {
    harness: Option<IgnoreMode>,
    configured: Option<&'value str>,
}

impl<'value> IgnoreSources<'value> {
    #[must_use]
    pub const fn builder() -> Self {
        Self {
            harness: None,
            configured: None,
        }
    }

    #[must_use]
    pub const fn harness(mut self, value: Option<IgnoreMode>) -> Self {
        self.harness = value;
        self
    }

    #[must_use]
    pub const fn configured(mut self, value: Option<&'value str>) -> Self {
        self.configured = value;
        self
    }

    /// Reads `CARGO_VAL_IGNORED` from a conflaguration source.
    #[must_use]
    pub fn from_settings(settings: &'value impl SettingsSource, harness: Option<IgnoreMode>) -> Self {
        Self::builder()
            .harness(harness)
            .configured(settings.get(IGNORED_KEY))
    }

    /// Picks the highest-precedence supplied source and validates only that value.
    ///
    /// # Errors
    ///
    /// Returns [`IgnoreError`] when the configured value is not `skip`, `only`, or `include`.
    pub fn resolve(self) -> Result<(IgnoreMode, IgnoreSource), IgnoreError> {
        match (self.harness, self.configured) {
            (Some(mode), _) => Ok((mode, IgnoreSource::HarnessArgument)),
            (None, Some(text)) => IgnoreMode::parse(text)
                .map(|mode| (mode, IgnoreSource::Configured))
                .ok_or(IgnoreError),
            (None, None) => Ok((IgnoreMode::Skip, IgnoreSource::Default)),
        }
    }
}

/// Every layer that can supply the filterset text; the command line wins over `CARGO_VAL_FILTERSET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FiltersetSources<'value> {
    command_line: Option<&'value [u8]>,
    configured: Option<&'value str>,
}

impl<'value> FiltersetSources<'value> {
    #[must_use]
    pub const fn builder() -> Self {
        Self {
            command_line: None,
            configured: None,
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

    /// Reads `CARGO_VAL_FILTERSET` from a conflaguration source.
    #[must_use]
    pub fn from_settings(
        settings: &'value impl SettingsSource,
        command_line: Option<&'value [u8]>,
    ) -> Self {
        Self::builder()
            .command_line(command_line)
            .configured(settings.get(FILTERSET_KEY))
    }

    /// The winning expression text, if any layer supplied one.
    #[must_use]
    pub fn resolve(self) -> Option<&'value [u8]> {
        self.command_line
            .or_else(|| self.configured.map(str::as_bytes))
    }
}

impl<'argument> HarnessRequest<'argument> {
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

const NO_TESTS_KEY: &str = "CARGO_VAL_NO_TESTS";

/// What a run does when its selection matches no tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTestsPolicy {
    Fail,
    Warn,
    Pass,
}

impl NoTestsPolicy {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "fail" => Some(Self::Fail),
            "warn" => Some(Self::Warn),
            "pass" => Some(Self::Pass),
            _ => None,
        }
    }
}

/// Where the winning policy came from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTestsSource {
    CommandLine,
    Configured,
    Default,
}

/// A refused policy value together with the source that supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoTestsError {
    pub source: NoTestsSource,
}

impl NoTestsError {
    #[must_use]
    pub const fn message(self) -> &'static [u8] {
        match self.source {
            NoTestsSource::CommandLine => b"cargo-val: --no-tests must be fail, warn, or pass\n",
            NoTestsSource::Configured => {
                b"cargo-val: CARGO_VAL_NO_TESTS must be fail, warn, or pass\n"
            }
            NoTestsSource::Default => b"cargo-val: the default no-tests policy is invalid\n",
        }
    }
}

/// The accepted policy and the source that won.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoTestsChoice {
    pub policy: NoTestsPolicy,
    pub source: NoTestsSource,
}

/// Every layer that can supply the policy, built from settings or in code with the same result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoTestsSources<'value> {
    command_line: Option<&'value [u8]>,
    configured: Option<&'value str>,
}

impl<'value> NoTestsSources<'value> {
    #[must_use]
    pub const fn builder() -> Self {
        Self {
            command_line: None,
            configured: None,
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

    /// Reads `CARGO_VAL_NO_TESTS` from a conflaguration source.
    #[must_use]
    pub fn from_settings(
        settings: &'value impl SettingsSource,
        command_line: Option<&'value [u8]>,
    ) -> Self {
        Self::builder()
            .command_line(command_line)
            .configured(settings.get(NO_TESTS_KEY))
    }

    /// Picks the highest-precedence supplied source and validates only that value.
    ///
    /// # Errors
    ///
    /// Returns [`NoTestsError`] naming the winning source when its value is not `fail`, `warn`, or `pass`.
    pub fn resolve(self) -> Result<NoTestsChoice, NoTestsError> {
        let (source, text) = match (self.command_line, self.configured) {
            (Some(bytes), _) => (NoTestsSource::CommandLine, core::str::from_utf8(bytes).ok()),
            (None, Some(text)) => (NoTestsSource::Configured, Some(text)),
            (None, None) => (NoTestsSource::Default, Some("fail")),
        };
        text.and_then(NoTestsPolicy::parse)
            .map(|policy| NoTestsChoice { policy, source })
            .ok_or(NoTestsError { source })
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

    #[test]
    fn c68_precedence_table_resolves_each_source_alone_and_in_combination() {
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

    #[test]
    fn c68_capacity_boundaries_and_every_source_fault_name_the_supplier() {
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

    #[test]
    fn c68_harness_request_accepts_ignored_and_both_test_threads_forms() {
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

    #[test]
    fn c68_harness_request_rejects_every_other_libtest_flag_and_a_dangling_value() {
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

    #[test]
    fn c68_settings_and_builder_construct_the_same_sources() {
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

    #[test]
    fn c68_stop_policy_precedence_table() {
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

    #[test]
    fn c68_malformed_max_fail_names_the_supplier() {
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

    #[test]
    fn c68_settings_and_builder_construct_the_same_stop_policy() {
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

#[cfg(test)]
mod no_tests_tests {
    use super::*;
    use conflaguration::BorrowedSettings;

    fn chosen(policy: NoTestsPolicy, source: NoTestsSource) -> NoTestsChoice {
        NoTestsChoice { policy, source }
    }

    #[test]
    fn c68_no_tests_precedence_table() {
        let table: [(NoTestsSources<'_>, Result<NoTestsChoice, NoTestsError>); 6] = [
            (
                NoTestsSources::builder(),
                Ok(chosen(NoTestsPolicy::Fail, NoTestsSource::Default)),
            ),
            (
                NoTestsSources::builder().configured(Some("warn")),
                Ok(chosen(NoTestsPolicy::Warn, NoTestsSource::Configured)),
            ),
            (
                NoTestsSources::builder().command_line(Some(b"pass")),
                Ok(chosen(NoTestsPolicy::Pass, NoTestsSource::CommandLine)),
            ),
            (
                NoTestsSources::builder()
                    .command_line(Some(b"pass"))
                    .configured(Some("warn")),
                Ok(chosen(NoTestsPolicy::Pass, NoTestsSource::CommandLine)),
            ),
            (
                NoTestsSources::builder().command_line(Some(b"ignore")),
                Err(NoTestsError {
                    source: NoTestsSource::CommandLine,
                }),
            ),
            (
                NoTestsSources::builder().configured(Some("")),
                Err(NoTestsError {
                    source: NoTestsSource::Configured,
                }),
            ),
        ];

        for (sources, expected) in table {
            assert_eq!(sources.resolve(), expected, "{sources:?}");
        }
    }

    #[test]
    fn c68_settings_and_builder_construct_the_same_no_tests_policy() {
        let pairs = [("CARGO_VAL_NO_TESTS", "warn")];
        let settings = BorrowedSettings::new(&pairs);

        let from_settings = NoTestsSources::from_settings(&settings, None);
        let from_builder = NoTestsSources::builder().configured(Some("warn"));

        assert_eq!(from_settings, from_builder);
        assert_eq!(
            from_settings.resolve(),
            Ok(chosen(NoTestsPolicy::Warn, NoTestsSource::Configured))
        );
        assert_eq!(
            NoTestsSources::from_settings(&settings, Some(b"pass")).resolve(),
            Ok(chosen(NoTestsPolicy::Pass, NoTestsSource::CommandLine))
        );
    }
}

#[cfg(test)]
mod ignore_and_filterset_tests {
    use super::*;

    type Resolved = Result<(IgnoreMode, IgnoreSource), IgnoreError>;
    use conflaguration::BorrowedSettings;

    #[test]
    fn harness_flags_select_the_ignore_mode_and_beat_the_setting() {
        let include: [&[u8]; 1] = [b"--include-ignored"];
        assert_eq!(
            HarnessRequest::parse(&include).map(|request| request.ignore),
            Ok(Some(IgnoreMode::Include))
        );
        let table: [(IgnoreSources<'_>, Resolved); 5] = [
            (IgnoreSources::builder(), Ok((IgnoreMode::Skip, IgnoreSource::Default))),
            (
                IgnoreSources::builder().configured(Some("only")),
                Ok((IgnoreMode::Only, IgnoreSource::Configured)),
            ),
            (
                IgnoreSources::builder().configured(Some("include")),
                Ok((IgnoreMode::Include, IgnoreSource::Configured)),
            ),
            (
                IgnoreSources::builder()
                    .harness(Some(IgnoreMode::Only))
                    .configured(Some("include")),
                Ok((IgnoreMode::Only, IgnoreSource::HarnessArgument)),
            ),
            (IgnoreSources::builder().configured(Some("all")), Err(IgnoreError)),
        ];
        for (sources, expected) in table {
            assert_eq!(sources.resolve(), expected, "{sources:?}");
        }
    }

    #[test]
    fn settings_and_builder_construct_the_same_ignore_mode_and_filterset() {
        let pairs = [
            ("CARGO_VAL_IGNORED", "include"),
            ("CARGO_VAL_FILTERSET", "test(alpha)"),
        ];
        let settings = BorrowedSettings::new(&pairs);

        assert_eq!(
            IgnoreSources::from_settings(&settings, None),
            IgnoreSources::builder().configured(Some("include"))
        );
        assert_eq!(
            FiltersetSources::from_settings(&settings, None),
            FiltersetSources::builder().configured(Some("test(alpha)"))
        );
        assert_eq!(
            FiltersetSources::from_settings(&settings, None).resolve(),
            Some(&b"test(alpha)"[..])
        );
        assert_eq!(
            FiltersetSources::from_settings(&settings, Some(b"all()")).resolve(),
            Some(&b"all()"[..])
        );
        assert_eq!(FiltersetSources::builder().resolve(), None);
    }
}
