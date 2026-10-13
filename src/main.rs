#[cfg(not(test))]
use cargo_val::cli::{CliAction, CliParser};
#[cfg(not(test))]
use prime::os::core_shard::run_on_this_thread_as;
#[cfg(not(test))]
use prime::os::reactor::Hangup;
#[cfg(not(test))]
use std::ffi::OsString;
#[cfg(not(test))]
use std::io::{self, Write};
#[cfg(not(test))]
use std::process::ExitCode;

#[cfg(not(test))]
const HELP_BANNER: &[u8] = include_bytes!("../tests/fixtures/help-banner.txt");
#[cfg(not(test))]
const VERSION_BANNER: &[u8] = concat!("cargo-val ", env!("CARGO_PKG_VERSION"), "\n").as_bytes();

#[cfg(not(test))]
fn main() -> ExitCode {
    let arguments: Vec<Vec<u8>> = std::env::args_os()
        .skip(1)
        .map(OsString::into_encoded_bytes)
        .collect();
    if arguments.iter().any(|argument| argument.contains(&0)) {
        let _ = writeln!(io::stderr(), "cargo-val: an argument contains a nul byte");
        return ExitCode::from(2);
    }
    match banner_exit(&arguments) {
        Some(code) => code,
        None => run_arguments(arguments),
    }
}

#[cfg(not(test))]
fn banner_exit(arguments: &[Vec<u8>]) -> Option<ExitCode> {
    let mut parser = CliParser::new();
    for argument in arguments {
        parser.push(argument);
        match parser.action() {
            CliAction::Run => {}
            CliAction::Help => return Some(banner_status(HELP_BANNER)),
            CliAction::Version => return Some(banner_status(VERSION_BANNER)),
        }
    }
    None
}

#[cfg(not(test))]
fn banner_status(banner: &[u8]) -> ExitCode {
    ExitCode::from(u8::from(io::stdout().write_all(banner).is_err()))
}

#[cfg(not(test))]
fn run_arguments(arguments: Vec<Vec<u8>>) -> ExitCode {
    let root = run_on_this_thread_as::<Hangup, _>(
        proxima_runtime::CoreId(0),
        cargo_val::run::drive(arguments),
    );
    match root {
        Ok(Ok(summary)) => exit_status(summary.exit_code()),
        Ok(Err(error)) => {
            let _ = io::stderr().write_all(error.render().bytes());
            exit_status(error.exit_code())
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "cargo-val: prime root failed: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(test))]
fn exit_status(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
