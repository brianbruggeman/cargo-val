use std::process::{Command, Output};

mod common;
#[path = "common/binary.rs"]
mod binary;

use common::fixture_output;


fn echo_command() -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.args(["/C", "echo", "captured"]);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("echo");
        command.arg("captured");
        command
    }
}

fn version_command() -> Command {
    let mut command = binary::command();
    command.arg("--version");
    command
}

fn help_command() -> Command {
    let mut command = binary::command();
    command.arg("--help");
    command
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}, stdout {} bytes {:?}, stderr {} bytes {:?}",
        output.status.code(),
        output.stdout.len(),
        String::from_utf8_lossy(&output.stdout),
        output.stderr.len(),
        String::from_utf8_lossy(&output.stderr),
    )
}

async fn capture_equals_std(mut command: Command) {
    let reference = command.output().expect("std runs the reference child");
    let captured = fixture_output(&mut command).await;

    assert_eq!(captured.stdout, reference.stdout, "stdout differs\ncaptured: {}\nreference: {}", describe(&captured), describe(&reference));
    assert_eq!(captured.stderr, reference.stderr, "stderr differs\ncaptured: {}\nreference: {}", describe(&captured), describe(&reference));
    assert_eq!(captured.status.code(), reference.status.code(), "exit code differs\ncaptured: {}\nreference: {}", describe(&captured), describe(&reference));
    assert!(!reference.stdout.is_empty(), "the reference child printed nothing, so the comparison proves nothing: {}", describe(&reference));
}

#[proxima::test]
async fn a_shell_echo_is_captured_byte_for_byte_with_its_exit_code() {
    capture_equals_std(echo_command()).await;
}

#[proxima::test]
async fn cargo_val_version_is_captured_byte_for_byte_with_its_exit_code() {
    capture_equals_std(version_command()).await;
}

#[proxima::test]
async fn cargo_val_help_is_captured_byte_for_byte_with_its_exit_code() {
    capture_equals_std(help_command()).await;
}

// windows keeps eight process-group jobs live at once, so nine runs expose a harness that never releases a finished child's group
#[proxima::test]
async fn children_past_the_group_table_capacity_are_each_captured_in_full() {
    for _round in 0..12 {
        capture_equals_std(echo_command()).await;
    }
}
