use std::io::{self, Write};

const HELP_BANNER: &str = "cargo-val\nRun Rust tests as isolated processes with streaming discovery.\n\nUsage: cargo val [OPTIONS] [FILTER] [-- <TEST-ARGS>...]\n\nOptions:\n  -h, --help       Print help\n  -V, --version    Print version\n";

fn main() {
    let arguments = std::env::args_os().skip(1);
    let help_requested = arguments
        .into_iter()
        .any(|argument| argument == "--help" || argument == "-h");

    if help_requested {
        let mut stdout = io::stdout().lock();
        if stdout.write_all(HELP_BANNER.as_bytes()).is_err() {
            std::process::exit(1);
        }
        return;
    }

    eprintln!("cargo-val is not implemented yet; use --help for usage");
    std::process::exit(2);
}
