use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum SuiteError {
    Usage(String),
    Infeasible(String),
    IterationCount { value: f64 },
    Io { path: PathBuf, source: std::io::Error },
}

impl fmt::Display for SuiteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "usage: {message}"),
            Self::Infeasible(message) => write!(formatter, "infeasible plan: {message}"),
            Self::IterationCount { value } => write!(formatter, "iteration count {value} is not a finite u64 value"),
            Self::Io { path, source } => write!(formatter, "io on {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for SuiteError {}
