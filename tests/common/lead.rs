use std::process::Command;

/// Makes `command` the leader of its own process group so a missed deadline can kill it together with everything it spawned.
pub fn lead_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0200)
    }
}
