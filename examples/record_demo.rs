use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const WINDOW_SIZE: &str = "100x30";
const COLUMNS: &str = "100";
const ROWS: &str = "30";
const THEME: &str = "dracula";
const FONT_SIZE: &str = "16";
const MAX_GIF_BYTES: u64 = 2 * 1024 * 1024;

fn main() -> ExitCode {
    match record() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("record_demo: {message}");
            ExitCode::FAILURE
        }
    }
}

fn record() -> Result<(), String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cast = root.join("assets/demo.cast");
    let gif = root.join("assets/demo.gif");
    fs::create_dir_all(root.join("assets")).map_err(|error| format!("assets directory: {error}"))?;
    let target_directory = demo_target_directory(&root);
    prebuild_demo(&root, &target_directory)?;

    run(Command::new("asciinema").current_dir(&root).env("PATH", path_with_built_binary()?).envs(isolated_cargo(&target_directory)).args([
        "rec",
        "--overwrite",
        "--window-size",
        WINDOW_SIZE,
        "-c",
        "cargo val --manifest-path examples/demo/Cargo.toml --target-dir isolated --tests",
        "assets/demo.cast",
    ]))?;
    run(Command::new("agg").current_dir(&root).args([
        "--theme",
        THEME,
        "--font-size",
        FONT_SIZE,
        "--cols",
        COLUMNS,
        "--rows",
        ROWS,
        "assets/demo.cast",
        "assets/demo.gif",
    ]))?;
    report(&cast, &gif)
}

fn demo_target_directory(root: &Path) -> PathBuf {
    env::args_os().nth(1).map_or_else(|| root.join("examples/demo/target"), PathBuf::from)
}

fn isolated_cargo(target_directory: &Path) -> [(&'static str, PathBuf); 2] {
    // cargo's package cache lock is global to CARGO_HOME, so a private home keeps other cargo processes out of the cast
    [("CARGO_TARGET_DIR", target_directory.to_path_buf()), ("CARGO_HOME", target_directory.join("cargo-home"))]
}

fn prebuild_demo(root: &Path, target_directory: &Path) -> Result<(), String> {
    // a red demo run exits nonzero by design, so only a launch failure is fatal; the run warms the isolated build dir, and --tests skips the concurrent doc child whose cargo would contend for the package cache
    run(Command::new("cargo")
        .current_dir(root)
        .env("PATH", path_with_built_binary()?)
        .envs(isolated_cargo(target_directory))
        .args(["val", "--manifest-path", "examples/demo/Cargo.toml", "--target-dir", "isolated", "--tests"])
        .stdout(Stdio::null())
        .stderr(Stdio::null()))
}

fn path_with_built_binary() -> Result<OsString, String> {
    let executable = env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    let profile_directory = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("example executable has no profile directory")?;
    if !profile_directory.join(format!("cargo-val{}", env::consts::EXE_SUFFIX)).is_file() {
        return Err(format!("build the cargo-val binary first: {} has none", profile_directory.display()));
    }
    let mut entries = vec![profile_directory.to_path_buf()];
    entries.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(entries).map_err(|error| format!("PATH: {error}"))
}

fn run(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().display().to_string();
    let status = command.status().map_err(|error| format!("{program} did not launch: {error}"))?;
    // a red demo run exits nonzero by design, so only the recorder's own failure to launch is fatal
    if status.code().is_none() {
        return Err(format!("{program} was killed by a signal"));
    }
    Ok(())
}

fn report(cast: &Path, gif: &Path) -> Result<(), String> {
    let cast_bytes = fs::metadata(cast).map_err(|error| format!("{}: {error}", cast.display()))?.len();
    let gif_bytes = fs::metadata(gif).map_err(|error| format!("{}: {error}", gif.display()))?.len();
    let frames = gif_frames(&fs::read(gif).map_err(|error| format!("{}: {error}", gif.display()))?)?;
    println!("{}: {cast_bytes} bytes", cast.display());
    println!("{}: {gif_bytes} bytes, {frames} frames", gif.display());
    if gif_bytes > MAX_GIF_BYTES {
        return Err(format!("{} is {gif_bytes} bytes, over the {MAX_GIF_BYTES} byte limit", gif.display()));
    }
    Ok(())
}

fn gif_frames(bytes: &[u8]) -> Result<usize, String> {
    let malformed = || "malformed gif".to_string();
    let flags = *bytes.get(10).ok_or_else(malformed)?;
    let mut cursor = 13 + color_table_len(flags);
    let mut frames = 0;
    loop {
        match *bytes.get(cursor).ok_or_else(malformed)? {
            0x3B => return Ok(frames),
            0x21 => cursor = skip_sub_blocks(bytes, cursor + 2)?,
            0x2C => {
                frames += 1;
                let local_flags = *bytes.get(cursor + 9).ok_or_else(malformed)?;
                cursor = skip_sub_blocks(bytes, cursor + 10 + color_table_len(local_flags) + 1)?;
            }
            _ => return Err(malformed()),
        }
    }
}

fn color_table_len(flags: u8) -> usize {
    if flags & 0x80 == 0 { 0 } else { 3 << ((flags & 0x07) + 1) }
}

fn skip_sub_blocks(bytes: &[u8], mut cursor: usize) -> Result<usize, String> {
    loop {
        let length = usize::from(*bytes.get(cursor).ok_or("malformed gif")?);
        cursor += 1 + length;
        if length == 0 {
            return Ok(cursor);
        }
    }
}
