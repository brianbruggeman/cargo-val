use std::process::Command;

#[cfg(unix)]
pub(crate) const EXIT_WITH_PARENT: &str = "CARGO_VAL_EXIT_WITH_PARENT";

// the one constructor for a cargo-val child: a SIGKILLed harness cannot clean up in-process, so the child exits with its parent
#[cfg(unix)]
pub(crate) fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-val"));
    command.env(EXIT_WITH_PARENT, "1");
    command
}

// windows has no parent watch, so the opt-in is refused there
#[cfg(not(unix))]
pub(crate) fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cargo-val"))
}
