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

/// Incremental sans-IO parser for cargo-val's own top-level options.
///
/// Feed arguments in order. Parsing stops at `--`, leaving later arguments to
/// the selected test executable. The parser borrows the manifest path and test
/// filter from argv and stores no argument copies.
#[derive(Debug, Default)]
pub struct CliParser<'argument> {
    action: Option<CliAction>,
    after_separator: bool,
    manifest_path: Option<&'argument [u8]>,
    filter: Option<&'argument [u8]>,
    exact: bool,
    expects_manifest_path: bool,
    invalid_manifest_path: bool,
    expects_cargo_option_value: bool,
    invalid_cargo_option_value: bool,
}

impl<'argument> CliParser<'argument> {
    /// Creates a parser with no selected top-level action.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            action: None,
            after_separator: false,
            manifest_path: None,
            filter: None,
            exact: false,
            expects_manifest_path: false,
            invalid_manifest_path: false,
            expects_cargo_option_value: false,
            invalid_cargo_option_value: false,
        }
    }

    /// Consumes one borrowed argument without allocating.
    pub fn push(&mut self, argument: &'argument [u8]) {
        if self.after_separator {
            return;
        }
        if self.expects_manifest_path {
            if argument.is_empty() {
                self.invalid_manifest_path = true;
            } else {
                self.manifest_path = Some(argument);
            }
            self.expects_manifest_path = false;
            return;
        }
        if self.expects_cargo_option_value {
            self.invalid_cargo_option_value |= argument.is_empty();
            self.expects_cargo_option_value = false;
            return;
        }
        if self.action.is_some() {
            return;
        }
        match argument {
            b"--" => self.after_separator = true,
            b"--help" | b"-h" => self.action = Some(CliAction::Help),
            b"--version" | b"-V" => self.action = Some(CliAction::Version),
            b"--exact" => self.exact = true,
            b"--manifest-path" => self.expects_manifest_path = true,
            b"--package" | b"-p" | b"--test" | b"--bin" | b"--example" | b"--bench"
            | b"--target" | b"--target-dir" | b"--profile" | b"--features" | b"--config"
            | b"--jobs" | b"-j" => self.expects_cargo_option_value = true,
            value if value.starts_with(b"--manifest-path=") => {
                let path = &value[b"--manifest-path=".len()..];
                if path.is_empty() {
                    self.invalid_manifest_path = true;
                } else {
                    self.manifest_path = Some(path);
                }
            }
            value if value.first().is_some_and(|byte| *byte != b'-') => {
                if self.filter.is_none() {
                    self.filter = Some(value);
                }
            }
            _ => {}
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

    /// Whether the test filter must match the full test name.
    #[must_use]
    pub const fn exact(&self) -> bool {
        self.exact
    }

    /// Whether the input ended while a manifest path was required or supplied it empty.
    #[must_use]
    pub const fn has_invalid_manifest_path(&self) -> bool {
        self.invalid_manifest_path || self.expects_manifest_path
    }

    /// Whether a Cargo option ended without its required value.
    #[must_use]
    pub const fn has_invalid_cargo_option_value(&self) -> bool {
        self.invalid_cargo_option_value || self.expects_cargo_option_value
    }
}
