use crate::config::FailFastArgument;

/// Action selected by parsing the cargo-val argument prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliAction {
    /// Run the test selection represented by the remaining arguments.
    Run,
    /// Print command usage.
    Help,
    /// Print the package version.
    Version,
}

const MAX_FORWARDED_ARGUMENTS: usize = 64;
const FAULT_MANIFEST_PATH: u8 = 1;
const FAULT_CARGO_OPTION_VALUE: u8 = 2;
const FAULT_POSITIONAL_ARGUMENT: u8 = 4;
const FAULT_ARGUMENT_STORAGE: u8 = 8;
const FAULT_FILTERSET: u8 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    First,
    Options,
    Harness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expectation {
    Nothing,
    ManifestPath,
    CargoOptionValue,
    MaxFailValue,
    NoTestsValue,
    FiltersetValue,
}

/// Which doc-test run the selection asks for, following Cargo's target rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSelection {
    /// A target flag such as `--lib` or `--test` was given without `--doc`: no doc tests.
    Excluded,
    /// No target flag: the default selection, which includes the library's doc tests.
    Included,
    /// `--doc`: only the doc tests.
    Only,
}

const TARGET_SELECTORS: [&[u8]; 10] = [
    b"--lib",
    b"--bins",
    b"--bin",
    b"--examples",
    b"--example",
    b"--tests",
    b"--test",
    b"--benches",
    b"--bench",
    b"--all-targets",
];

fn selects_target(argument: &[u8]) -> bool {
    TARGET_SELECTORS.iter().any(|selector| {
        argument == *selector
            || argument
                .strip_prefix(*selector)
                .is_some_and(|rest| rest.first() == Some(&b'='))
    })
}

/// Incremental sans-IO parser for cargo-val's own top-level options.
///
/// Feed arguments in order. Parsing stops at `--`, leaving later arguments to
/// the selected test executable. The parser borrows the manifest path and test
/// filter from argv and stores no argument copies.
#[derive(Debug)]
pub struct CliParser<'argument> {
    action: Option<CliAction>,
    position: Position,
    manifest_path: Option<&'argument [u8]>,
    filter: Option<&'argument [u8]>,
    exact: bool,
    fail_fast: Option<FailFastArgument<'argument>>,
    no_tests: Option<&'argument [u8]>,
    filterset: Option<&'argument [u8]>,
    doc: bool,
    target_selected: bool,
    expectation: Expectation,
    faults: u8,
    cargo_arguments: [&'argument [u8]; MAX_FORWARDED_ARGUMENTS],
    cargo_argument_count: usize,
    harness_arguments: [&'argument [u8]; MAX_FORWARDED_ARGUMENTS],
    harness_argument_count: usize,
}

impl<'argument> CliParser<'argument> {
    /// Creates a parser with no selected top-level action.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            action: None,
            position: Position::First,
            manifest_path: None,
            filter: None,
            exact: false,
            fail_fast: None,
            no_tests: None,
            filterset: None,
            doc: false,
            target_selected: false,
            expectation: Expectation::Nothing,
            faults: 0,
            cargo_arguments: [&[]; MAX_FORWARDED_ARGUMENTS],
            cargo_argument_count: 0,
            harness_arguments: [&[]; MAX_FORWARDED_ARGUMENTS],
            harness_argument_count: 0,
        }
    }

    /// Consumes one borrowed argument without allocating.
    pub fn push(&mut self, argument: &'argument [u8]) {
        match self.position {
            Position::First => {
                self.position = Position::Options;
                if argument == b"val" {
                    return;
                }
            }
            Position::Harness => {
                self.push_harness_argument(argument);
                return;
            }
            Position::Options => {}
        }
        if self.consume_expected(argument) || self.action.is_some() {
            return;
        }
        if selects_target(argument) {
            self.target_selected = true;
        }
        match argument {
            b"--" => self.position = Position::Harness,
            b"--help" | b"-h" => self.action = Some(CliAction::Help),
            b"--version" | b"-V" => self.action = Some(CliAction::Version),
            b"--exact" => self.exact = true,
            b"--doc" => self.doc = true,
            b"--fail-fast" => self.fail_fast = Some(FailFastArgument::FailFast),
            b"--no-fail-fast" => {
                self.fail_fast = Some(FailFastArgument::NoFailFast);
                self.push_cargo_argument(argument);
            }
            b"--max-fail" => self.expectation = Expectation::MaxFailValue,
            b"--no-tests" => self.expectation = Expectation::NoTestsValue,
            b"--filterset" | b"-E" => self.begin_filterset(),
            b"--manifest-path" | b"-m" => {
                self.expectation = Expectation::ManifestPath;
                self.push_cargo_argument(argument);
            }
            b"--package" | b"-p" | b"--exclude" | b"--test" | b"--bin" | b"--example"
            | b"--bench" | b"--target" | b"--target-dir" | b"--profile" | b"--features" | b"-F"
            | b"-Z" | b"--config" | b"--color" | b"--message-format" | b"--filter-platform"
            | b"--jobs" | b"-j" => {
                self.expectation = Expectation::CargoOptionValue;
                self.push_cargo_argument(argument);
            }
            value if value.starts_with(b"--filterset=") => {
                self.set_filterset(&value[b"--filterset=".len()..]);
            }
            value if value.starts_with(b"-E") && !value.starts_with(b"--") => {
                let text = value[2..].strip_prefix(b"=").unwrap_or(&value[2..]);
                self.set_filterset(text);
            }
            value if value.starts_with(b"--no-tests=") => {
                self.no_tests = Some(&value[b"--no-tests=".len()..]);
            }
            value if value.starts_with(b"--max-fail=") => {
                self.fail_fast = Some(FailFastArgument::MaxFail(&value[b"--max-fail=".len()..]));
            }
            value if value.starts_with(b"-m") && !value.starts_with(b"--") => {
                let path = value[2..].strip_prefix(b"=").unwrap_or(&value[2..]);
                if path.is_empty() {
                    self.faults |= FAULT_MANIFEST_PATH;
                } else {
                    self.manifest_path = Some(path);
                }
                self.push_cargo_argument(argument);
            }
            value if value.starts_with(b"--manifest-path=") => {
                let path = &value[b"--manifest-path=".len()..];
                if path.is_empty() {
                    self.faults |= FAULT_MANIFEST_PATH;
                } else {
                    self.manifest_path = Some(path);
                }
                self.push_cargo_argument(argument);
            }
            value if value.first().is_some_and(|byte| *byte != b'-') => {
                if self.filter.is_none() {
                    self.filter = Some(value);
                } else {
                    self.faults |= FAULT_POSITIONAL_ARGUMENT;
                }
            }
            _ => self.push_cargo_argument(argument),
        }
    }

    fn consume_expected(&mut self, argument: &'argument [u8]) -> bool {
        match core::mem::replace(&mut self.expectation, Expectation::Nothing) {
            Expectation::Nothing => false,
            Expectation::ManifestPath => {
                let accepted = self.accept_option_value(argument, FAULT_MANIFEST_PATH);
                if accepted && !argument.is_empty() {
                    self.manifest_path = Some(argument);
                }
                accepted
            }
            Expectation::CargoOptionValue => {
                self.accept_option_value(argument, FAULT_CARGO_OPTION_VALUE)
            }
            Expectation::MaxFailValue => {
                self.fail_fast = Some(FailFastArgument::MaxFail(argument));
                true
            }
            Expectation::NoTestsValue => {
                self.no_tests = Some(argument);
                true
            }
            Expectation::FiltersetValue => {
                self.set_filterset(argument);
                true
            }
        }
    }

    fn begin_filterset(&mut self) {
        self.expectation = Expectation::FiltersetValue;
    }

    // a second expression is a fault: nextest ORs them, which a single parsed expression cannot carry
    fn set_filterset(&mut self, text: &'argument [u8]) {
        if self.filterset.is_some() || text.is_empty() {
            self.faults |= FAULT_FILTERSET;
        } else {
            self.filterset = Some(text);
        }
    }

    // a value that looks like another option is a fault and is parsed again as an option
    fn accept_option_value(&mut self, argument: &'argument [u8], fault: u8) -> bool {
        if argument.is_empty() {
            self.faults |= fault;
            return true;
        }
        if argument.first().is_some_and(|byte| *byte == b'-') {
            self.faults |= fault;
            return false;
        }
        self.push_cargo_argument(argument);
        true
    }

    fn push_cargo_argument(&mut self, argument: &'argument [u8]) {
        if self.cargo_argument_count == self.cargo_arguments.len() {
            self.faults |= FAULT_ARGUMENT_STORAGE;
        } else {
            self.cargo_arguments[self.cargo_argument_count] = argument;
            self.cargo_argument_count += 1;
        }
    }

    fn push_harness_argument(&mut self, argument: &'argument [u8]) {
        if self.harness_argument_count == self.harness_arguments.len() {
            self.faults |= FAULT_ARGUMENT_STORAGE;
        } else {
            self.harness_arguments[self.harness_argument_count] = argument;
            self.harness_argument_count += 1;
        }
    }

    /// Returns the selected action, defaulting to test execution.
    #[must_use]
    pub const fn action(&self) -> CliAction {
        match self.action {
            Some(action) => action,
            None => CliAction::Run,
        }
    }

    /// Borrowed manifest path selected by `--manifest-path`.
    #[must_use]
    pub const fn manifest_path(&self) -> Option<&'argument [u8]> {
        self.manifest_path
    }

    /// First positional filter passed to Cargo's test harness.
    #[must_use]
    pub const fn filter(&self) -> Option<&'argument [u8]> {
        self.filter
    }

    /// Borrowed Cargo flags and values, excluding cargo-val's filter.
    #[must_use]
    pub fn cargo_arguments(&self) -> &[&'argument [u8]] {
        &self.cargo_arguments[..self.cargo_argument_count]
    }

    /// Borrowed test-harness arguments after Cargo's `--` separator.
    #[must_use]
    pub fn harness_arguments(&self) -> &[&'argument [u8]] {
        &self.harness_arguments[..self.harness_argument_count]
    }

    /// The last stop-policy option seen; a `--max-fail` with no value reports an empty value.
    #[must_use]
    pub const fn fail_fast(&self) -> Option<FailFastArgument<'argument>> {
        if matches!(self.expectation, Expectation::MaxFailValue) {
            Some(FailFastArgument::MaxFail(&[]))
        } else {
            self.fail_fast
        }
    }

    /// The last `--no-tests` value; one with no value reports an empty value.
    #[must_use]
    pub const fn no_tests(&self) -> Option<&'argument [u8]> {
        if matches!(self.expectation, Expectation::NoTestsValue) {
            Some(&[])
        } else {
            self.no_tests
        }
    }

    /// Whether doc tests are skipped, included with the other targets, or the only target.
    #[must_use]
    pub const fn doc_selection(&self) -> DocSelection {
        if self.doc {
            DocSelection::Only
        } else if self.target_selected {
            DocSelection::Excluded
        } else {
            DocSelection::Included
        }
    }

    /// The `-E` / `--filterset` expression text, borrowed from argv.
    #[must_use]
    pub const fn filterset(&self) -> Option<&'argument [u8]> {
        self.filterset
    }

    /// Whether `-E` was given twice, empty, or without its value.
    #[must_use]
    pub const fn has_invalid_filterset(&self) -> bool {
        self.faults & FAULT_FILTERSET != 0 || matches!(self.expectation, Expectation::FiltersetValue)
    }

    /// Whether the test filter must match the full test name.
    #[must_use]
    pub const fn exact(&self) -> bool {
        self.exact
    }

    /// Whether the input ended while a manifest path was required or supplied it empty.
    #[must_use]
    pub const fn has_invalid_manifest_path(&self) -> bool {
        self.faults & FAULT_MANIFEST_PATH != 0
            || matches!(self.expectation, Expectation::ManifestPath)
    }

    /// Whether a Cargo option ended without its required value.
    #[must_use]
    pub const fn has_invalid_cargo_option_value(&self) -> bool {
        self.faults & FAULT_CARGO_OPTION_VALUE != 0
            || matches!(self.expectation, Expectation::CargoOptionValue)
    }

    /// Whether more than one positional test filter was supplied.
    #[must_use]
    pub const fn has_invalid_positional_argument(&self) -> bool {
        self.faults & FAULT_POSITIONAL_ARGUMENT != 0
    }

    /// Whether caller-visible fixed argument storage overflowed.
    #[must_use]
    pub const fn has_argument_storage_overflow(&self) -> bool {
        self.faults & FAULT_ARGUMENT_STORAGE != 0
    }
}

impl Default for CliParser<'_> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::string::String;
    use std::vec::Vec;

    use super::{CliAction, CliParser};

    fn invocation_is_valid(parser: &CliParser<'_>) -> bool {
        parser.manifest_path().is_some()
            && parser.filter().is_some()
            && !parser.has_invalid_manifest_path()
            && !parser.has_invalid_cargo_option_value()
            && !parser.has_invalid_positional_argument()
            && !parser.has_argument_storage_overflow()
    }

    const VALUE_OPTIONS: [(&str, &str); 19] = [
        ("--message-format", "json"),
        ("--color", "never"),
        ("--config", "build.jobs=2"),
        ("-Z", "unstable-options"),
        ("-p", "cargo_val_fixture"),
        ("--package", "cargo_val_fixture"),
        ("--exclude", "other_package"),
        ("--bin", "cargo-val"),
        ("--example", "demo"),
        ("--test", "integration_fixture"),
        ("--bench", "throughput"),
        ("--target", "aarch64-apple-darwin"),
        ("--target-dir", "target/c68"),
        ("--profile", "release"),
        ("-F", "extra"),
        ("--features", "extra,other"),
        ("-j", "2"),
        ("--jobs", "2"),
        ("-m", "tests/fixtures/workspace/Cargo.toml"),
    ];

    #[test]
    fn c68_every_value_taking_option_forwards_its_value_and_never_makes_it_the_filter() {
        let mut checked = 0;
        for (option, value) in VALUE_OPTIONS {
            let separated = [option.as_bytes(), value.as_bytes(), b"fixture::passes"];
            let mut parser = CliParser::new();
            for argument in separated {
                parser.push(argument);
            }

            assert_eq!(
                parser.cargo_arguments(),
                &[option.as_bytes(), value.as_bytes()],
                "{option} {value}"
            );
            assert_eq!(parser.filter(), Some(&b"fixture::passes"[..]), "{option}");
            assert!(!parser.has_invalid_cargo_option_value(), "{option}");
            assert!(!parser.has_invalid_positional_argument(), "{option}");
            checked += 1;
        }
        assert_eq!(checked, VALUE_OPTIONS.len());
    }

    #[test]
    fn c68_long_options_also_forward_in_the_equals_joined_form() {
        let joined = [
            "--message-format=json",
            "--color=never",
            "--config=build.jobs=2",
            "--package=cargo_val_fixture",
            "--exclude=other_package",
            "--bin=cargo-val",
            "--example=demo",
            "--test=integration_fixture",
            "--bench=throughput",
            "--target=aarch64-apple-darwin",
            "--target-dir=target/c68",
            "--profile=release",
            "--features=extra,other",
            "--jobs=2",
            "--manifest-path=tests/fixtures/workspace/Cargo.toml",
        ];
        let mut checked = 0;
        for argument in joined {
            let mut parser = CliParser::new();
            parser.push(argument.as_bytes());
            parser.push(b"fixture::passes");

            assert_eq!(
                parser.cargo_arguments(),
                &[argument.as_bytes()],
                "{argument}"
            );
            assert_eq!(parser.filter(), Some(&b"fixture::passes"[..]), "{argument}");
            checked += 1;
        }
        assert_eq!(checked, joined.len());
    }

    #[test]
    fn c68_short_manifest_path_forms_record_the_manifest_path() {
        for (arguments, expected) in [
            (
                &[b"-m".as_slice(), b"a/Cargo.toml"][..],
                b"a/Cargo.toml".as_slice(),
            ),
            (
                &[b"-ma/Cargo.toml".as_slice()][..],
                b"a/Cargo.toml".as_slice(),
            ),
            (
                &[b"-m=a/Cargo.toml".as_slice()][..],
                b"a/Cargo.toml".as_slice(),
            ),
        ] {
            let mut parser = CliParser::new();
            for argument in arguments {
                parser.push(argument);
            }

            assert_eq!(parser.manifest_path(), Some(expected));
            assert_eq!(parser.filter(), None);
            assert!(!parser.has_invalid_manifest_path());
        }
        let mut dangling = CliParser::new();
        dangling.push(b"-m");
        assert!(dangling.has_invalid_manifest_path());
    }

    #[test]
    fn c68_stop_policy_and_no_tests_options_are_consumed_not_forwarded() {
        let mut parser = CliParser::new();
        for argument in [
            b"--max-fail".as_slice(),
            b"3",
            b"--no-tests=warn",
            b"--no-fail-fast",
            b"filter",
        ] {
            parser.push(argument);
        }

        assert_eq!(
            parser.fail_fast(),
            Some(crate::config::FailFastArgument::NoFailFast)
        );
        assert_eq!(parser.no_tests(), Some(&b"warn"[..]));
        assert_eq!(parser.cargo_arguments(), &[b"--no-fail-fast".as_slice()]);
        assert_eq!(parser.filter(), Some(&b"filter"[..]));

        let mut max_fail = CliParser::new();
        max_fail.push(b"--max-fail");
        max_fail.push(b"3");
        assert_eq!(
            max_fail.fail_fast(),
            Some(crate::config::FailFastArgument::MaxFail(b"3"))
        );
        assert_eq!(max_fail.cargo_arguments(), [] as [&[u8]; 0]);
    }

    #[test]
    fn c48d_retains_external_cargo_and_harness_arguments_by_reference() {
        let cargo_arguments = serde_json::from_str::<Vec<String>>(include_str!(
            "../specs/cargo-val/evidence/c48d-cli/cargo-argv.json"
        ))
        .expect("recorded cargo val argv is valid JSON");
        let arguments = cargo_arguments
            .iter()
            .map(String::as_bytes)
            .collect::<Vec<_>>();
        let mut parser = CliParser::new();
        for argument in &arguments {
            parser.push(argument);
        }

        assert_eq!(parser.action(), CliAction::Run);
        assert_eq!(parser.manifest_path(), Some(arguments[2]));
        assert_eq!(parser.filter(), Some(arguments[5]));
        let expected_cargo_arguments = [arguments[1], arguments[2], arguments[3], arguments[4]];
        let expected_harness_arguments = [arguments[7], arguments[8]];
        assert_eq!(parser.cargo_arguments(), &expected_cargo_arguments);
        assert_eq!(parser.harness_arguments(), &expected_harness_arguments);
        assert!(invocation_is_valid(&parser));
        let copied_argument_bytes = parser
            .cargo_arguments()
            .iter()
            .zip(expected_cargo_arguments)
            .filter(|(observed, expected)| !core::ptr::eq(observed.as_ptr(), expected.as_ptr()))
            .count()
            + parser
                .harness_arguments()
                .iter()
                .zip(expected_harness_arguments)
                .filter(|(observed, expected)| !core::ptr::eq(observed.as_ptr(), expected.as_ptr()))
                .count()
            + usize::from(!core::ptr::eq(
                parser
                    .filter()
                    .expect("the invocation has a filter")
                    .as_ptr(),
                arguments[5].as_ptr(),
            ));
        assert_eq!(copied_argument_bytes, 0);
        std::println!(
            "{{\"cargo_external_prefixes\":1,\"manifest_pairs\":1,\"package_pairs\":1,\"filters\":1,\"harness_tail_arguments\":2,\"copied_argument_bytes\":0}}"
        );
    }

    #[test]
    fn c48d_rejects_a_missing_cargo_option_value_before_spawn() {
        let mut rejected_invocations = 0;
        let missing_trailing_value = [
            b"val".as_slice(),
            b"--manifest-path",
            b"workspace/Cargo.toml",
            b"--package",
        ];
        let mut parser = CliParser::new();
        for argument in missing_trailing_value {
            parser.push(argument);
        }

        assert_eq!(parser.action(), CliAction::Run);
        assert!(!invocation_is_valid(&parser));
        rejected_invocations += 1;
        assert!(parser.has_invalid_cargo_option_value());
        assert_eq!(
            parser.cargo_arguments(),
            &[
                missing_trailing_value[1],
                missing_trailing_value[2],
                missing_trailing_value[3]
            ]
        );

        let missing_before_option = [
            b"val".as_slice(),
            b"--manifest-path",
            b"--package",
            b"cargo_val_fixture",
            b"fixture::passes",
        ];
        let mut parser = CliParser::new();
        for argument in missing_before_option {
            parser.push(argument);
        }
        assert_eq!(parser.manifest_path(), None);
        assert_eq!(parser.cargo_arguments(), &missing_before_option[1..4]);
        assert!(!invocation_is_valid(&parser));
        rejected_invocations += 1;

        let missing_before_separator = [b"val".as_slice(), b"--package", b"--", b"--nocapture"];
        let mut parser = CliParser::new();
        for argument in missing_before_separator {
            parser.push(argument);
        }
        assert!(parser.has_invalid_cargo_option_value());
        assert_eq!(parser.harness_arguments(), &[missing_before_separator[3]]);
        assert!(!invocation_is_valid(&parser));
        rejected_invocations += 1;

        let duplicate_filters = [b"val".as_slice(), b"first", b"second"];
        let mut parser = CliParser::new();
        for argument in duplicate_filters {
            parser.push(argument);
        }
        assert!(parser.has_invalid_positional_argument());
        assert!(!invocation_is_valid(&parser));
        rejected_invocations += 1;
        assert_eq!(rejected_invocations, 4);

        std::println!(
            "{{\"missing_trailing_value\":1,\"missing_before_option\":1,\"missing_before_separator\":1,\"duplicate_filters_rejected\":1,\"invalid_invocations_rejected\":{rejected_invocations}}}"
        );
    }
}
