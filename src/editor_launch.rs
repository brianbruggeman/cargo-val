//! The command line that opens an editor at a failure's `file:line:col`, and the foreground run of it.
//!
//! The editor identity is the program's basename, the same reading [`crate::hyperlink::editor_template`] gives `VISUAL` and `EDITOR`.

use std::format;
use std::string::ToString;
use std::vec::Vec;

use crate::line::Line;

const DEFAULT_EDITOR: &[u8] = b"vi";

#[cfg(unix)]
const MISSING_PROGRAM: i32 = libc::ENOENT;
#[cfg(not(unix))]
const MISSING_PROGRAM: i32 = -2;
const NOT_SUPPORTED: i32 = -1;

/// `VISUAL`, else `EDITOR`, else `vi`; an empty or blank value counts as unset.
#[must_use]
pub fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Vec<u8> {
    [visual, editor]
        .into_iter()
        .flatten()
        .find(|command| !command.trim().is_empty())
        .map_or_else(|| DEFAULT_EDITOR.to_vec(), |command| command.trim().as_bytes().to_vec())
}

/// The full argv for opening `file` at `line` and `col` with `command`: the program, any words the command already carried, then the editor's own location syntax.
///
/// `code`, `cursor`, `windsurf` take `-g file:line:col`; `zed`, `hx`, `subl` take `file:line:col`; `vim`, `vi`, `nvim`, `view` take `+line file`;
/// `nano` takes `+line,col file`; `emacs` and `emacsclient` take `+line:col file`; an unknown editor gets the plain path.
#[must_use]
pub fn editor_argv(command: &[u8], file: &[u8], line: u32, col: u32) -> Vec<Vec<u8>> {
    let mut words = command.split(u8::is_ascii_whitespace).filter(|word| !word.is_empty());
    let program = words.next().unwrap_or(DEFAULT_EDITOR);
    let name = program.rsplit(|byte| *byte == b'/').next().unwrap_or(program);
    let mut argv: Vec<Vec<u8>> = core::iter::once(program).chain(words).map(<[u8]>::to_vec).collect();
    argv.extend(location_arguments(name, file, line, col));
    argv
}

fn location_arguments(name: &[u8], file: &[u8], line: u32, col: u32) -> Vec<Vec<u8>> {
    let site = [file, b":", line.to_string().as_bytes(), b":", col.to_string().as_bytes()].concat();
    match name {
        b"code" | b"cursor" | b"windsurf" => std::vec![b"-g".to_vec(), site],
        b"zed" | b"hx" | b"subl" => std::vec![site],
        b"idea" => std::vec![b"--line".to_vec(), line.to_string().into_bytes(), file.to_vec()],
        b"vim" | b"vi" | b"nvim" | b"view" => std::vec![format!("+{line}").into_bytes(), file.to_vec()],
        b"nano" => std::vec![format!("+{line},{col}").into_bytes(), file.to_vec()],
        b"emacs" | b"emacsclient" => std::vec![format!("+{line}:{col}").into_bytes(), file.to_vec()],
        _ => std::vec![file.to_vec()],
    }
}

/// Spawns `argv` as the terminal's foreground process group and returns its process group and the source that reports its exit.
///
/// Composes the crate's spawn edge, [`crate::edge_spawn::spawn_foreground`], and the exit source every test child already arms: the editor leads its own process group and owns the controlling terminal while it runs,
/// so `Ctrl-C` reaches it and not cargo-val, and the pump learns of its exit as one more lane instead of blocking on it.
///
/// # Errors
///
/// Returns the OS error code when the editor cannot be started or watched.
#[cfg(unix)]
pub fn spawn(argv: &[Vec<u8>], environment: &[&[u8]], directory: &[u8]) -> Result<(u32, crate::ExitSource), i32> {
    let (program, rest) = argv.split_first().ok_or(libc::EINVAL)?;
    let arguments: Vec<&[u8]> = rest.iter().map(Vec::as_slice).collect();
    let group = crate::edge_spawn::spawn_foreground(program, &arguments, environment, Some(directory), libc::STDIN_FILENO).map_err(|error| error.os_code)?;
    let process = core::num::NonZeroI32::new(group.cast_signed()).ok_or(libc::EINVAL)?;
    Ok((group, crate::ExitSource::new(process)))
}

/// The platform cannot hand the terminal to an editor; the code is `ENOTSUP`-shaped so the caller words it as unsupported.
///
/// # Errors
///
/// Always returns `-1`.
#[cfg(not(unix))]
pub fn spawn(_argv: &[Vec<u8>], _environment: &[&[u8]], _directory: &[u8]) -> Result<(u32, crate::ExitSource), i32> {
    Err(NOT_SUPPORTED)
}


/// How long a probe child may take before the editor counts as not running.
pub const PROBE_DEADLINE_MS: u64 = 300;

fn program_and_name(command: &[u8]) -> (&[u8], &[u8]) {
    let program = command.split(u8::is_ascii_whitespace).find(|word| !word.is_empty()).unwrap_or(DEFAULT_EDITOR);
    (program, program.rsplit(|byte| *byte == b'/').next().unwrap_or(program))
}

fn variable<'value>(environment: &[&'value [u8]], name: &[u8]) -> Option<&'value [u8]> {
    environment
        .iter()
        .find_map(|entry| entry.strip_prefix(name).and_then(|rest| rest.strip_prefix(b"=")))
        .filter(|value| !value.is_empty())
}

fn words(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|part| part.to_vec()).collect()
}

/// The sockets nvim leaves in `$XDG_RUNTIME_DIR` as `nvim.*`, sorted; a directory that cannot be read has none.
#[must_use]
#[cfg(unix)]
pub fn nvim_sockets(environment: &[&[u8]]) -> Vec<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;

    let Some(directory) = variable(environment, b"XDG_RUNTIME_DIR") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(std::ffi::OsStr::from_bytes(directory)) else {
        return Vec::new();
    };
    let mut sockets: Vec<Vec<u8>> = entries
        .flatten()
        .filter(|entry| entry.file_name().as_bytes().starts_with(b"nvim.") && entry.file_type().is_ok_and(|kind| kind.is_socket()))
        .map(|entry| entry.path().as_os_str().as_bytes().to_vec())
        .collect();
    sockets.sort();
    sockets
}

/// There is no socket directory to read off Unix.
#[must_use]
#[cfg(not(unix))]
pub fn nvim_sockets(_environment: &[&[u8]]) -> Vec<Vec<u8>> {
    Vec::new()
}

fn nvim_target<'value>(environment: &[&'value [u8]], sockets: &'value [Vec<u8>]) -> Option<&'value [u8]> {
    variable(environment, b"NVIM_LISTEN_ADDRESS").or_else(|| sockets.first().map(Vec::as_slice))
}

/// The child whose exit says whether a running editor can take the file, or `None` when the answer needs no child.
///
/// nvim inside nvim (`$NVIM`) is already known; otherwise it asks the first socket (`$NVIM_LISTEN_ADDRESS`, else `$XDG_RUNTIME_DIR/nvim.*`) to evaluate `1`;
/// emacs asks `emacsclient -e t`; vim lists its servers on stdout.
#[must_use]
pub fn probe_argv(command: &[u8], environment: &[&[u8]], sockets: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let (program, name) = program_and_name(command);
    match name {
        b"nvim" if variable(environment, b"NVIM").is_some() => None,
        b"nvim" => nvim_target(environment, sockets).map(|socket| words(&[program, b"--server", socket, b"--remote-expr", b"1"])),
        b"emacs" | b"emacsclient" => Some(words(&[b"emacsclient", b"-e", b"t"])),
        b"vim" => Some(words(&[program, b"--serverlist"])),
        _ => None,
    }
}

/// The decision for one `e` press: whether to run detached, and the commands in order.
///
/// `answer` is the probe's stdout when the probe exited 0, and `None` when it failed, timed out, or none was needed.
/// A detached list runs one command after another without taking the terminal; an attached list is one command that is handed the terminal.
#[must_use]
pub fn commands(command: &[u8], file: &[u8], (line, col): (u32, u32), (environment, sockets): (&[&[u8]], &[Vec<u8>]), answer: Option<&[u8]>) -> (bool, Vec<Vec<Vec<u8>>>) {
    let (program, name) = program_and_name(command);
    let attached = || (false, std::vec![editor_argv(command, file, line, col)]);
    let site = format!("+{line}:{col}").into_bytes();
    match name {
        b"code" | b"cursor" | b"windsurf" | b"zed" | b"subl" | b"idea" => (true, std::vec![editor_argv(command, file, line, col)]),
        b"nvim" => match (variable(environment, b"NVIM"), answer) {
            (Some(server), _) => (true, nvim_remote(program, server, file, (line, col))),
            (None, Some(_)) => nvim_target(environment, sockets).map_or_else(attached, |server| (true, nvim_remote(program, server, file, (line, col)))),
            (None, None) => attached(),
        },
        b"emacs" | b"emacsclient" => match answer {
            Some(_) => (true, std::vec![words(&[b"emacsclient", b"-n", &site, file])]),
            None => (false, std::vec![words(&[if name == b"emacs" { program } else { b"emacs" }, b"-nw", &site, file])]),
        },
        b"vim" => match answer.and_then(|listing| listing.split(|byte| *byte == b'\n').next()).map(<[u8]>::trim_ascii).filter(|server| !server.is_empty()) {
            Some(server) => (true, std::vec![words(&[program, b"--servername", server, b"--remote-silent", format!("+{line}").as_bytes(), file])]),
            None => attached(),
        },
        _ => attached(),
    }
}

fn nvim_remote(program: &[u8], server: &[u8], file: &[u8], (line, col): (u32, u32)) -> Vec<Vec<Vec<u8>>> {
    let cursor = format!(":call cursor({line},{col})<CR>").into_bytes();
    std::vec![words(&[program, b"--server", server, b"--remote", file]), words(&[program, b"--server", server, b"--remote-send", &cursor])]
}

/// Starts `argv` as its own process group with both streams piped and standard input closed, so it never touches the terminal.
///
/// # Errors
///
/// Returns the OS error code when the program cannot start.
pub fn spawn_detached(argv: &[Vec<u8>], environment: &[&[u8]], directory: &[u8]) -> Result<(u32, crate::edge_spawn::Child), i32> {
    let (program, rest) = argv.split_first().ok_or(MISSING_PROGRAM)?;
    let arguments: Vec<&[u8]> = rest.iter().map(Vec::as_slice).collect();
    let child = crate::edge_spawn::spawn_in_group(program, &arguments, environment, &[], Some(directory)).map_err(|error| error.os_code)?;
    Ok((child.process_id(), child))
}

/// Kills a probe that missed its deadline, with everything it started.
pub fn terminate(group: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-group.cast_signed(), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// The exit code of an ended editor: its status, or `128 + signal` when a signal ended it.
#[must_use]
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.code().or_else(|| status.signal().map(|signal| 128 + signal)).unwrap_or(1)
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

/// Takes the terminal back from the editor's group.
pub fn reclaim_terminal() {
    #[cfg(unix)]
    crate::edge_spawn::reclaim_terminal(libc::STDIN_FILENO);
}

/// Ends a running editor because cargo-val is going away: hangs up its group, then takes the terminal back.
pub fn abandon(group: u32) {
    #[cfg(unix)]
    crate::edge_spawn::hang_up(group);
    #[cfg(not(unix))]
    let _ = group;
    reclaim_terminal();
}

/// Continues the editor if it stopped since the last call; the pump calls it on each tick because an exit source cannot see a stop.
pub fn keep_running(group: u32) {
    #[cfg(unix)]
    crate::edge_spawn::continue_stopped(group);
    #[cfg(not(unix))]
    let _ = group;
}

/// Words the review footer shows for how the editor ended: nothing for a clean exit, else the status, the missing program, or the platform limit.
pub fn write_notice<const CAPACITY: usize>(out: &mut Line<CAPACITY>, outcome: Result<i32, i32>, raw: bool, program: &[u8]) {
    if !raw {
        out.text(b"terminal did not return to key mode");
        return;
    }
    match outcome {
        Ok(0) => {}
        Ok(code) => {
            out.text(b"editor exited with status ");
            out.number(usize::try_from(code).unwrap_or(0));
            out.text(b": ");
            out.text(program);
        }
        Err(code) if code == NOT_SUPPORTED => out.text(b"editor not supported on this platform"),
        Err(code) if code == MISSING_PROGRAM => {
            out.text(b"editor not found: ");
            out.text(program);
        }
        Err(code) => {
            out.text(b"editor failed to start: ");
            out.text(program);
            out.text(b" (os error ");
            out.number(usize::try_from(code).unwrap_or(0));
            out.text(b")");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::string::String;
    use std::vec::Vec;

    use super::{commands, editor_argv, editor_command, probe_argv};

    fn argv_text(command: &str, file: &str, line: u32, col: u32) -> Vec<String> {
        editor_argv(command.as_bytes(), file.as_bytes(), line, col).into_iter().map(|word| String::from_utf8(word).expect("argv is text")).collect()
    }

    #[proxima::test]
    #[case::code_goes_to_line_and_column("code", &["code", "-g", "/w/src/lib.rs:28:13"])]
    #[case::cursor_like_code("cursor", &["cursor", "-g", "/w/src/lib.rs:28:13"])]
    #[case::zed_takes_the_site("zed", &["zed", "/w/src/lib.rs:28:13"])]
    #[case::helix_takes_the_site("hx", &["hx", "/w/src/lib.rs:28:13"])]
    #[case::vim_plus_line_then_file("vim", &["vim", "+28", "/w/src/lib.rs"])]
    #[case::vi_plus_line_then_file("vi", &["vi", "+28", "/w/src/lib.rs"])]
    #[case::nvim_plus_line_then_file("nvim", &["nvim", "+28", "/w/src/lib.rs"])]
    #[case::idea_line_flag_then_file("idea", &["idea", "--line", "28", "/w/src/lib.rs"])]
    #[case::nano_line_comma_column("nano", &["nano", "+28,13", "/w/src/lib.rs"])]
    #[case::emacs_line_colon_column("emacs", &["emacs", "+28:13", "/w/src/lib.rs"])]
    #[case::unknown_editor_gets_the_plain_path("kate", &["kate", "/w/src/lib.rs"])]
    #[case::absolute_program_path_is_named_by_its_basename("/opt/bin/code", &["/opt/bin/code", "-g", "/w/src/lib.rs:28:13"])]
    #[case::words_the_command_carried_stay_before_the_location("code -w", &["code", "-w", "-g", "/w/src/lib.rs:28:13"])]
    async fn the_argv_follows_the_editors_own_location_syntax(#[case] command: &str, #[case] expected: &[&str]) {
        assert_eq!(argv_text(command, "/w/src/lib.rs", 28, 13), expected);
    }

    #[proxima::test]
    #[case::visual_wins(Some("code"), Some("vim"), "code")]
    #[case::editor_when_visual_unset(None, Some("vim"), "vim")]
    #[case::editor_when_visual_blank(Some("  "), Some("nano"), "nano")]
    #[case::vi_when_neither(None, None, "vi")]
    #[case::vi_when_both_blank(Some(""), Some(" "), "vi")]
    async fn the_editor_is_visual_then_editor_then_vi(#[case] visual: Option<&str>, #[case] editor: Option<&str>, #[case] expected: &str) {
        assert_eq!(editor_command(visual, editor), expected.as_bytes());
    }

    fn decided(command: &str, environment: &[&str], sockets: &[&str], answer: Option<&str>) -> (bool, Vec<Vec<String>>) {
        let environment: Vec<&[u8]> = environment.iter().map(|entry| entry.as_bytes()).collect();
        let sockets: Vec<Vec<u8>> = sockets.iter().map(|socket| socket.as_bytes().to_vec()).collect();
        let (detached, commands) = commands(command.as_bytes(), b"/w/src/lib.rs", (28, 13), (&environment, &sockets), answer.map(str::as_bytes));
        let text = |argv: Vec<Vec<u8>>| argv.into_iter().map(|word| String::from_utf8(word).expect("argv is text")).collect();
        (detached, commands.into_iter().map(text).collect())
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| String::from(*word)).collect()
    }

    #[proxima::test]
    #[case::code("code", &["code", "-g", "/w/src/lib.rs:28:13"])]
    #[case::cursor("cursor", &["cursor", "-g", "/w/src/lib.rs:28:13"])]
    #[case::windsurf("windsurf", &["windsurf", "-g", "/w/src/lib.rs:28:13"])]
    #[case::zed("zed", &["zed", "/w/src/lib.rs:28:13"])]
    #[case::subl("subl", &["subl", "/w/src/lib.rs:28:13"])]
    #[case::idea("idea", &["idea", "--line", "28", "/w/src/lib.rs"])]
    async fn a_gui_cli_always_runs_detached_with_its_own_location_syntax(#[case] command: &str, #[case] expected: &[&str]) {
        assert_eq!(decided(command, &[], &[], None), (true, std::vec![argv(expected)]));
    }

    #[proxima::test]
    async fn nvim_inside_nvim_sends_the_file_then_the_cursor_to_the_parent_server() {
        let decision = decided("nvim", &["NVIM=/run/user/501/nvim.4242.0"], &["/run/user/501/nvim.9.0"], None);

        let remote = argv(&["nvim", "--server", "/run/user/501/nvim.4242.0", "--remote", "/w/src/lib.rs"]);
        let cursor = argv(&["nvim", "--server", "/run/user/501/nvim.4242.0", "--remote-send", ":call cursor(28,13)<CR>"]);
        assert_eq!(decision, (true, std::vec![remote, cursor]));
    }

    #[proxima::test]
    #[case::listen_address_wins_over_runtime_sockets(&["NVIM_LISTEN_ADDRESS=/tmp/nvim-a"], &["/run/user/501/nvim.9.0"], "/tmp/nvim-a")]
    #[case::first_runtime_socket_when_no_listen_address(&[], &["/run/user/501/nvim.9.0", "/run/user/501/nvim.12.0"], "/run/user/501/nvim.9.0")]
    async fn nvim_with_a_live_socket_uses_that_socket(#[case] environment: &[&str], #[case] sockets: &[&str], #[case] server: &str) {
        let (detached, commands) = decided("nvim", environment, sockets, Some("1"));

        assert!(detached);
        assert_eq!(commands[0], argv(&["nvim", "--server", server, "--remote", "/w/src/lib.rs"]));
        assert_eq!(commands.len(), 2);
    }

    #[proxima::test]
    #[case::no_socket_at_all(&[], &[], None)]
    #[case::socket_that_did_not_answer(&[], &["/run/user/501/nvim.9.0"], None)]
    async fn nvim_with_no_live_socket_is_launched_with_the_terminal(#[case] environment: &[&str], #[case] sockets: &[&str], #[case] answer: Option<&str>) {
        assert_eq!(decided("nvim", environment, sockets, answer), (false, std::vec![argv(&["nvim", "+28", "/w/src/lib.rs"])]));
    }

    #[proxima::test]
    async fn emacs_with_a_running_daemon_uses_emacsclient_without_waiting() {
        assert_eq!(decided("emacs", &[], &[], Some("t")), (true, std::vec![argv(&["emacsclient", "-n", "+28:13", "/w/src/lib.rs"])]));
    }

    #[proxima::test]
    #[case::emacs("emacs", "emacs")]
    #[case::emacsclient_falls_back_to_emacs("emacsclient", "emacs")]
    async fn emacs_with_no_daemon_launches_a_terminal_emacs_and_never_starts_one(#[case] command: &str, #[case] program: &str) {
        assert_eq!(decided(command, &[], &[], None), (false, std::vec![argv(&[program, "-nw", "+28:13", "/w/src/lib.rs"])]));
    }

    #[proxima::test]
    async fn vim_with_a_server_sends_the_file_to_the_first_one_listed() {
        let decision = decided("vim", &[], &[], Some("GVIM\nOTHER\n"));

        assert_eq!(decision, (true, std::vec![argv(&["vim", "--servername", "GVIM", "--remote-silent", "+28", "/w/src/lib.rs"])]));
    }

    #[proxima::test]
    #[case::probe_failed(None)]
    #[case::empty_serverlist(Some(""))]
    #[case::blank_serverlist(Some("\n"))]
    async fn vim_with_no_server_is_launched_with_the_terminal(#[case] answer: Option<&str>) {
        assert_eq!(decided("vim", &[], &[], answer), (false, std::vec![argv(&["vim", "+28", "/w/src/lib.rs"])]));
    }

    #[proxima::test]
    #[case::nano("nano", &["nano", "+28,13", "/w/src/lib.rs"])]
    #[case::vi("vi", &["vi", "+28", "/w/src/lib.rs"])]
    #[case::unknown("kate", &["kate", "/w/src/lib.rs"])]
    async fn any_other_editor_is_launched_with_the_terminal(#[case] command: &str, #[case] expected: &[&str]) {
        assert_eq!(decided(command, &["NVIM=/x"], &["/s"], Some("t")), (false, std::vec![argv(expected)]));
    }

    #[proxima::test]
    #[case::nvim_in_nvim_needs_no_probe("nvim", &["NVIM=/x"], &[], None)]
    #[case::nvim_without_a_socket_needs_no_probe("nvim", &[], &[], None)]
    #[case::nvim_probes_its_socket("nvim", &[], &["/run/user/501/nvim.9.0"], Some(&["nvim", "--server", "/run/user/501/nvim.9.0", "--remote-expr", "1"]))]
    #[case::emacs_asks_emacsclient("emacs", &[], &[], Some(&["emacsclient", "-e", "t"]))]
    #[case::vim_lists_its_servers("vim", &[], &[], Some(&["vim", "--serverlist"]))]
    #[case::code_needs_no_probe("code", &[], &[], None)]
    #[case::nano_needs_no_probe("nano", &[], &[], None)]
    async fn only_the_editors_that_must_be_running_are_probed(#[case] command: &str, #[case] environment: &[&str], #[case] sockets: &[&str], #[case] expected: Option<&[&str]>) {
        let environment: Vec<&[u8]> = environment.iter().map(|entry| entry.as_bytes()).collect();
        let sockets: Vec<Vec<u8>> = sockets.iter().map(|socket| socket.as_bytes().to_vec()).collect();

        let probe = probe_argv(command.as_bytes(), &environment, &sockets);

        let text = probe.map(|argv| argv.into_iter().map(|word| String::from_utf8(word).expect("argv is text")).collect::<Vec<_>>());
        assert_eq!(text, expected.map(argv));
    }
}
