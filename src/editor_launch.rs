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

    use super::{editor_argv, editor_command};

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
}
