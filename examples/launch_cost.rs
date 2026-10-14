//! Times one test-binary launch per spawn path on the fixture that the `scale` example generated.
//!
//! Run `cargo run --release --example scale` first. `LAUNCH_COUNT` (default 500) sets the launches per arm.
//! Each arm launches, waits for exit, and closes, sequentially, interleaved arm by arm so drift hits all arms alike.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/block_on.rs"]
mod block_on;

#[cfg(windows)]
use cargo_val::edge_spawn_windows::{Child, HeldLaunchStorage, held_launch_storage, resume_ready, spawn_exact, spawn_exact_ready};
#[cfg(not(windows))]
use cargo_val::edge_spawn_unix::{Child, HeldLaunchStorage, held_launch_storage, resume_ready, spawn_exact, spawn_exact_ready};

const FIRST_BINARY: &str = "scale_00";
const PASSING_TEST: &str = "cases::b0_i00000";
const ABSENT_TEST: &str = "cases::__no_such_test__";
const WARMUP: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpawnPath {
    CargoValExact,
    StdExact,
    StdAbsent,
    CargoValAbsent,
    CargoValHiddenConsole,
}

const PATHS: [SpawnPath; 5] = [
    SpawnPath::CargoValExact,
    SpawnPath::StdExact,
    SpawnPath::StdAbsent,
    SpawnPath::CargoValAbsent,
    SpawnPath::CargoValHiddenConsole,
];

impl SpawnPath {
    fn label(self) -> &'static str {
        match self {
            Self::CargoValExact => "(a) cargo-val spawn_exact, passing test",
            Self::StdExact => "(b) std Command output, passing test",
            Self::StdAbsent => "(c) std Command output, absent test",
            Self::CargoValAbsent => "(d) cargo-val spawn_exact, absent test",
            Self::CargoValHiddenConsole => "(e) cargo-val spawn_exact, passing test, CREATE_NO_WINDOW",
        }
    }
}

struct Launcher {
    executable: PathBuf,
    directory: PathBuf,
    environment: Vec<Vec<u8>>,
    storage: RefCell<HeldLaunchStorage>,
    hidden_console_storage: RefCell<HeldLaunchStorage>,
}

#[cfg(windows)]
fn hidden_console_storage() -> HeldLaunchStorage {
    held_launch_storage().with_hidden_console()
}

#[cfg(not(windows))]
fn hidden_console_storage() -> HeldLaunchStorage {
    held_launch_storage()
}

fn main() -> ExitCode {
    match measure() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("launch_cost: {message}");
            ExitCode::FAILURE
        }
    }
}

fn fixture_directory() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    let fixture = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("example executable has no profile directory")?
        .join("scale-fixture");
    if fixture.join("Cargo.toml").is_file() {
        return Ok(fixture);
    }
    Err(format!("{} is missing; run `cargo run --release --example scale` first", fixture.display()))
}

fn first_test_binary(fixture: &Path) -> Result<PathBuf, String> {
    let mut build = Command::new("cargo");
    build
        .args(["test", "--lib", "--no-run", "--message-format=json", "--target-dir"])
        .arg(fixture.join("target"))
        .current_dir(fixture)
        .env_remove("CARGO_TARGET_DIR");
    let output = block_on::output_blocking(build, common::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE")?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["target"]["name"] == FIRST_BINARY)
        .find_map(|message| message["executable"].as_str().map(PathBuf::from))
        .ok_or_else(|| format!("no executable for {FIRST_BINARY} in cargo output"))
}

fn inherited_environment() -> Vec<Vec<u8>> {
    std::env::vars_os()
        .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()).into_bytes())
        .collect()
}

#[cfg(not(windows))]
fn wait_for_exit(process_id: u32) -> Result<(), String> {
    let pid = libc::pid_t::try_from(process_id).map_err(|error| format!("pid {process_id}: {error}"))?;
    let mut status = 0;
    // safety: waitpid takes a pid we own and a valid status pointer, and reports failure through its return value.
    let reaped = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    if reaped == pid {
        return Ok(());
    }
    Err(format!("waitpid {pid}: {}", std::io::Error::last_os_error()))
}

#[cfg(windows)]
fn wait_for_exit(process_id: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

    // safety: OpenProcess takes plain integers; a null return is checked before the handle is used.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
    if handle.is_null() {
        return Err(format!("OpenProcess {process_id}: {}", std::io::Error::last_os_error()));
    }
    // safety: the handle is the valid process handle opened above and is closed exactly once.
    let waited = unsafe { WaitForSingleObject(handle, INFINITE) };
    unsafe { CloseHandle(handle) };
    if waited == 0 {
        return Ok(());
    }
    Err(format!("WaitForSingleObject {process_id}: {waited}"))
}

impl Launcher {
    fn cargo_val_launch(&self, storage: &RefCell<HeldLaunchStorage>, test: &str) -> Result<(), String> {
        let environment = self.environment.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let directory = self.directory.to_string_lossy().into_owned().into_bytes();
        let executable = self.executable.to_string_lossy().into_owned().into_bytes();
        let child = spawn_exact(
            &mut storage.borrow_mut(),
            &executable,
            test.as_bytes(),
            &[],
            &environment,
            &[],
            &directory,
        )
        .map_err(|error| format!("spawn_exact: {error:?}"))?;
        wait_for_exit(child.process_id())?;
        drop(child);
        Ok(())
    }

    fn std_launch(&self, test: &str) -> Result<(), String> {
        let mut launch = Command::new(&self.executable);
        launch.args(["--exact", test]).current_dir(&self.directory);
        let output = block_on::output_blocking(launch, common::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE")?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!("{test} exited {:?}", output.status.code()))
    }

    fn launch(&self, path: SpawnPath) -> Result<(), String> {
        match path {
            SpawnPath::CargoValExact => self.cargo_val_launch(&self.storage, PASSING_TEST),
            SpawnPath::CargoValHiddenConsole => self.cargo_val_launch(&self.hidden_console_storage, PASSING_TEST),
            SpawnPath::StdExact => self.std_launch(PASSING_TEST),
            SpawnPath::StdAbsent => self.std_launch(ABSENT_TEST),
            SpawnPath::CargoValAbsent => self.cargo_val_launch(&self.storage, ABSENT_TEST),
        }
    }
}

fn percentile(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn median(sorted: &[f64]) -> f64 {
    percentile(sorted, 50)
}

fn measure() -> Result<(), String> {
    let count = match std::env::var("LAUNCH_COUNT") {
        Ok(text) => text.parse::<usize>().ok().filter(|count| *count > 0).ok_or_else(|| format!("LAUNCH_COUNT={text} is not a positive integer"))?,
        Err(_) => 500,
    };
    let fixture = fixture_directory()?;
    let launcher = Launcher {
        executable: first_test_binary(&fixture)?,
        directory: fixture,
        environment: inherited_environment(),
        storage: RefCell::new(held_launch_storage()),
        hidden_console_storage: RefCell::new(hidden_console_storage()),
    };
    println!("launch_cost: {} launches per arm of {}", count, launcher.executable.display());
    for _ in 0..WARMUP {
        PATHS.iter().try_for_each(|path| launcher.launch(*path))?;
        SPLIT_ARMS.iter().try_for_each(|arm| launcher.split_launch(*arm).map(drop))?;
    }
    let mut samples = vec![Vec::with_capacity(count); PATHS.len()];
    let mut split_samples = vec![Vec::with_capacity(count); SPLIT_ARMS.len()];
    for _ in 0..count {
        for (slot, path) in PATHS.iter().enumerate() {
            let start = Instant::now();
            launcher.launch(*path)?;
            samples[slot].push(start.elapsed().as_secs_f64() * 1e6);
        }
        for (slot, arm) in SPLIT_ARMS.iter().enumerate() {
            split_samples[slot].push(launcher.split_launch(*arm)?);
        }
    }
    report(&mut samples);
    report_split(&mut split_samples);
    Ok(())
}

fn report(samples: &mut [Vec<f64>]) {
    println!("{:<44} {:>12} {:>12}", "arm", "median us", "p99 us");
    for (path, arm) in PATHS.iter().zip(samples.iter_mut()) {
        arm.sort_by(f64::total_cmp);
        println!("{:<44} {:>12.1} {:>12.1}", path.label(), median(arm), percentile(arm, 99));
    }
    println!("cargo-val construction overhead, passing test (a) - (b): {:.1} us median", median(&samples[0]) - median(&samples[1]));
    println!("cargo-val construction overhead, absent test (d) - (c): {:.1} us median", median(&samples[3]) - median(&samples[2]));
    println!("console-flag cost, passing test (e) - (a): {:.1} us median (positive: CREATE_NO_WINDOW is slower)", median(&samples[4]) - median(&samples[0]));
    println!("image-load plus libtest startup floor (c): {:.1} us median", median(&samples[2]));
    println!("arm (a) and (d) include a pid-based wait (waitpid, or OpenProcess + WaitForSingleObject + CloseHandle on Windows)");
}

#[derive(Clone, Copy)]
enum SplitArm {
    CreateSuspended,
    ChildAbsent,
    ChildPassing,
}

const SPLIT_ARMS: [SplitArm; 3] = [SplitArm::CreateSuspended, SplitArm::ChildAbsent, SplitArm::ChildPassing];

impl SplitArm {
    fn label(self) -> &'static str {
        match self {
            Self::CreateSuspended => "(f) cargo-val spawn_exact_ready, create only",
            Self::ChildAbsent => "(g) resume -> exit, absent test",
            Self::ChildPassing => "(h) resume -> exit, passing test",
        }
    }
}

impl Launcher {
    fn ready_child(&self, test: &str) -> Result<Child, String> {
        let environment = self.environment.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let directory = self.directory.to_string_lossy().into_owned().into_bytes();
        let executable = self.executable.to_string_lossy().into_owned().into_bytes();
        spawn_exact_ready(&mut self.storage.borrow_mut(), &executable, test.as_bytes(), &[], &environment, &[], &directory)
            .map_err(|error| format!("spawn_exact_ready: {error:?}"))
    }

    fn timed_create(&self) -> Result<f64, String> {
        let start = Instant::now();
        let child = self.ready_child(PASSING_TEST)?;
        let created = start.elapsed().as_secs_f64() * 1e6;
        resume_ready(&child);
        wait_for_exit(child.process_id())?;
        Ok(created)
    }

    fn timed_resume_to_exit(&self, test: &str) -> Result<f64, String> {
        let child = self.ready_child(test)?;
        let start = Instant::now();
        resume_ready(&child);
        wait_for_exit(child.process_id())?;
        Ok(start.elapsed().as_secs_f64() * 1e6)
    }

    fn split_launch(&self, arm: SplitArm) -> Result<f64, String> {
        match arm {
            SplitArm::CreateSuspended => self.timed_create(),
            SplitArm::ChildAbsent => self.timed_resume_to_exit(ABSENT_TEST),
            SplitArm::ChildPassing => self.timed_resume_to_exit(PASSING_TEST),
        }
    }
}

fn report_split(samples: &mut [Vec<f64>]) {
    println!("{:<44} {:>12} {:>12}", "split arm", "median us", "p99 us");
    for (arm, values) in SPLIT_ARMS.iter().zip(samples.iter_mut()) {
        values.sort_by(f64::total_cmp);
        println!("{:<44} {:>12.1} {:>12.1}", arm.label(), median(values), percentile(values, 99));
    }
    println!("parent create (suspended) (f): {:.1} us median", median(&samples[0]));
    println!("child init + libtest start (g): {:.1} us median", median(&samples[1]));
    println!("test body share (h) - (g): {:.1} us median", median(&samples[2]) - median(&samples[1]));
    println!("{}", split_note());
}

#[cfg(windows)]
const fn split_note() -> &'static str {
    "(f) times only the CreateProcessW CREATE_SUSPENDED call; (g)/(h) time ResumeThread to process exit (pid-based wait included)"
}

#[cfg(not(windows))]
const fn split_note() -> &'static str {
    "unix mapping: no suspended state, so (f) times the spawn call alone and (g)/(h) time from spawn return to exit; resume_ready is a no-op"
}
