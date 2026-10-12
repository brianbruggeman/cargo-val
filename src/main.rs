#[cfg(not(test))]
use cargo_val::cli::{CliAction, CliParser};
#[cfg(not(test))]
use std::ffi::{CString, OsString};
#[cfg(not(test))]
use std::io::{self, Write};
#[cfg(not(test))]
use std::process::ExitCode;

#[cfg(not(test))]
const HELP_BANNER: &[u8] = include_bytes!("../tests/fixtures/help-banner.txt");
#[cfg(not(test))]
const VERSION_BANNER: &[u8] = concat!("cargo-val ", env!("CARGO_PKG_VERSION"), "\n").as_bytes();
#[cfg(all(
    not(test),
    not(all(
        any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
        feature = "proxima-core",
        feature = "proxima-codec",
        feature = "proxima-primitives"
    ))
))]
const NOT_IMPLEMENTED: &[u8] = b"cargo-val is not implemented yet; use --help for usage\n";

#[cfg(not(test))]
fn main() -> ExitCode {
    let arguments: Result<Vec<CString>, _> = std::env::args_os()
        .skip(1)
        .map(|argument| CString::new(OsString::into_encoded_bytes(argument)))
        .collect();
    let Ok(arguments) = arguments else {
        let _ = writeln!(io::stderr(), "cargo-val: an argument contains a nul byte");
        return ExitCode::from(2);
    };
    match banner_exit(&arguments) {
        Some(code) => code,
        None => run_arguments(arguments),
    }
}

// cargo arguments are forwarded as C strings, so each one keeps its nul terminator in memory behind the slice
#[cfg(not(test))]
fn banner_exit(arguments: &[CString]) -> Option<ExitCode> {
    let mut parser = CliParser::new();
    for argument in arguments {
        parser.push(argument.to_bytes());
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

#[cfg(all(
    not(test),
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
fn run_arguments(arguments: Vec<CString>) -> ExitCode {
    let root = prime::os::core_shard::run_on_this_thread(
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

#[cfg(all(
    not(test),
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
fn exit_status(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(all(
    not(test),
    not(all(
        any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
        feature = "proxima-core",
        feature = "proxima-codec",
        feature = "proxima-primitives"
    ))
))]
fn run_arguments(_arguments: Vec<CString>) -> ExitCode {
    let _ = io::stderr().write_all(NOT_IMPLEMENTED);
    ExitCode::from(2)
}
