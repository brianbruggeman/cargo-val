//! Generates a workspace of thousands of trivial tests and times `cargo test`, `cargo nextest`, and `cargo val`
//! (its default process mode, `--process shard`, and `--process test`) over it.
//!
//! Build the binary first: `cargo build --release --bin cargo-val`, then `cargo run --release --example scale`.
//! `SCALE_TESTS` (default 13000) passing tests are split evenly over `SCALE_BINARIES` (default 4) library crates,
//! and 8 planted failing tests are spread across them. Exit status reflects outcome agreement, never timing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

#[path = "../tests/common/group.rs"]
mod group;
#[path = "../tests/common/output.rs"]
mod output;

const PLANTED: usize = 8;
const ARM_DEADLINE: Duration = Duration::from_secs(3600);
const ARMS: [Arm; 7] = [
    Arm::Test,
    Arm::Nextest,
    Arm::Val,
    Arm::ValTest,
    Arm::ValShardHalf,
    Arm::ValShardOne,
    Arm::ValShardTwo,
];
const ROUNDS: usize = 3;
const SHOWN_DIFFERENCES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Arm {
    Test,
    Nextest,
    Val,
    ValTest,
    ValShardHalf,
    ValShardOne,
    ValShardTwo,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Self::Test => "cargo test",
            Self::Nextest => "nextest",
            Self::Val => "cargo val",
            Self::ValTest => "val test, 1xCPU",
            Self::ValShardHalf => "val shard, 0.5xCPU",
            Self::ValShardOne => "val shard, 1xCPU",
            Self::ValShardTwo => "val shard, 2xCPU",
        }
    }
}

impl Arm {
    /// Live test processes a cargo-val arm asks for through `CARGO_VAL_JOBS`, as a multiple of the core count.
    fn children(self, cores: usize) -> Option<usize> {
        match self {
            Self::ValTest | Self::ValShardOne => Some(cores),
            Self::ValShardHalf => Some((cores / 2).max(1)),
            Self::ValShardTwo => Some((cores * 2).min(64)),
            Self::Test | Self::Nextest | Self::Val => None,
        }
    }
}

type Outcomes = BTreeMap<String, bool>;

struct Shape {
    tests: usize,
    binaries: usize,
}

struct Context {
    shape: Shape,
    fixture: PathBuf,
    target: PathBuf,
    cargo_val: PathBuf,
    jobs: usize,
}

struct Observation {
    wall: Duration,
    status: Option<i32>,
    outcomes: Outcomes,
}

fn main() -> ExitCode {
    match measure() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("scale: {message}");
            ExitCode::FAILURE
        }
    }
}

fn count_from_environment(key: &str, default: usize) -> Result<usize, String> {
    match std::env::var(key) {
        Ok(text) => text
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| format!("{key}={text} is not a positive integer")),
        Err(_) => Ok(default),
    }
}

fn context() -> Result<Context, String> {
    let executable = std::env::current_exe().map_err(|error| format!("current exe: {error}"))?;
    let profile = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("example executable has no profile directory")?;
    let cargo_val = profile.join(format!("cargo-val{}", std::env::consts::EXE_SUFFIX));
    if !cargo_val.is_file() {
        return Err(format!("{} is missing; run `cargo build --release --bin cargo-val`", cargo_val.display()));
    }
    let fixture = profile.join("scale-fixture");
    let shape = Shape {
        tests: count_from_environment("SCALE_TESTS", 13000)?,
        binaries: count_from_environment("SCALE_BINARIES", 4)?,
    };
    let jobs = std::thread::available_parallelism().map_err(|error| format!("parallelism: {error}"))?.get();
    Ok(Context { shape, target: fixture.join("target"), fixture, cargo_val, jobs })
}

fn passing_name(binary: usize, index: usize) -> String {
    format!("b{binary}_i{index:05}")
}

fn planted_name(binary: usize, ordinal: usize) -> String {
    format!("b{binary}_planted_{ordinal}")
}

fn planted_in(binary: usize, shape: &Shape) -> Vec<usize> {
    (0..PLANTED).filter(|ordinal| ordinal % shape.binaries == binary).collect()
}

fn passing_in(binary: usize, shape: &Shape) -> usize {
    shape.tests / shape.binaries + usize::from(binary < shape.tests % shape.binaries)
}

fn crate_source(binary: usize, shape: &Shape) -> String {
    let mut source = String::from("#[cfg(test)]\nmod cases {\n");
    for index in 0..passing_in(binary, shape) {
        let name = passing_name(binary, index);
        let _ = writeln!(source, "    #[test]\n    fn {name}() {{\n        assert_eq!({index}_usize % 7, {index}_usize % 7);\n    }}");
    }
    for ordinal in planted_in(binary, shape) {
        let name = planted_name(binary, ordinal);
        let _ = writeln!(source, "    #[test]\n    fn {name}() {{\n        assert_eq!(1_usize, 2_usize, \"planted failure {ordinal}\");\n    }}");
    }
    source.push_str("}\n");
    source
}

fn generate(context: &Context) -> Result<(), String> {
    let _ = fs::remove_dir_all(&context.fixture);
    let members = (0..context.shape.binaries).map(|binary| format!("\"scale_{binary:02}\"")).collect::<Vec<_>>();
    let manifest = format!("[workspace]\nresolver = \"3\"\nmembers = [{}]\n", members.join(", "));
    write_file(&context.fixture.join("Cargo.toml"), &manifest)?;
    for binary in 0..context.shape.binaries {
        let directory = context.fixture.join(format!("scale_{binary:02}"));
        let package = format!("[package]\nname = \"scale_{binary:02}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n");
        write_file(&directory.join("Cargo.toml"), &package)?;
        write_file(&directory.join("src").join("lib.rs"), &crate_source(binary, &context.shape))?;
    }
    Ok(())
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| format!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    fs::write(path, text).map_err(|error| format!("write {}: {error}", path.display()))
}

fn expected(shape: &Shape) -> Outcomes {
    let mut outcomes = Outcomes::new();
    for binary in 0..shape.binaries {
        for index in 0..passing_in(binary, shape) {
            outcomes.insert(format!("cases::{}", passing_name(binary, index)), true);
        }
        for ordinal in planted_in(binary, shape) {
            outcomes.insert(format!("cases::{}", planted_name(binary, ordinal)), false);
        }
    }
    outcomes
}

fn build(context: &Context) -> Result<(), String> {
    let mut build = Command::new("cargo");
    build
        .args(["test", "--lib", "--no-run", "--target-dir"])
        .arg(&context.target)
        .current_dir(&context.fixture)
        .env_remove("CARGO_TARGET_DIR");
    let output = output::output_bounded(&mut build, ARM_DEADLINE)?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!("fixture build failed: {}", String::from_utf8_lossy(&output.stderr)))
}

fn nextest_available() -> bool {
    let mut probe = Command::new("cargo");
    probe.args(["nextest", "--version"]);
    output::output_bounded(&mut probe, group::SYNC_DEADLINE).is_ok_and(|output| output.status.success())
}

fn build_command(arm: Arm, context: &Context) -> Command {
    let jobs = context.jobs.to_string();
    let mut command = match arm {
        Arm::Test | Arm::Nextest => Command::new("cargo"),
        Arm::Val | Arm::ValTest | Arm::ValShardHalf | Arm::ValShardOne | Arm::ValShardTwo => Command::new(&context.cargo_val),
    };
    #[cfg(unix)]
    if !matches!(arm, Arm::Test | Arm::Nextest) {
        command.env("CARGO_VAL_EXIT_WITH_PARENT", "1");
    }
    match arm {
        Arm::Test => command.args(["test", "--lib", "--no-fail-fast", "--color", "never", "--target-dir"]),
        Arm::Nextest => command.args(["nextest", "run", "--lib", "--no-fail-fast", "--color", "never", "--target-dir"]),
        Arm::Val => command.args(["val", "--lib", "--no-fail-fast", "--target-dir"]),
        Arm::ValTest => command.args(["val", "--process", "test", "--lib", "--no-fail-fast", "--target-dir"]),
        Arm::ValShardHalf | Arm::ValShardOne | Arm::ValShardTwo => {
            command.args(["val", "--process", "shard", "--lib", "--no-fail-fast", "--target-dir"])
        }
    };
    command.arg(&context.target);
    match arm {
        Arm::Test => command.args(["--", "--test-threads", &jobs]),
        Arm::Nextest => command.args(["-j", &jobs]),
        Arm::Val | Arm::ValTest | Arm::ValShardHalf | Arm::ValShardOne | Arm::ValShardTwo => command.args(["--jobs", &jobs]),
    };
    if let Some(children) = arm.children(context.jobs) {
        command.env("CARGO_VAL_JOBS", children.to_string());
    }
    if !matches!(arm, Arm::Test | Arm::Nextest) {
        command.env("CARGO_VAL_OUTPUT", "rich");
    }
    command.current_dir(&context.fixture).env_remove("CARGO_TARGET_DIR");
    command
}

fn run_arm(arm: Arm, context: &Context) -> Result<Observation, String> {
    let mut command = build_command(arm, context);
    let start = Instant::now();
    let output = output::output_bounded(&mut command, ARM_DEADLINE).map_err(|failure| format!("{}: {failure}", arm.label()))?;
    let wall = start.elapsed();
    let text = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    Ok(Observation { wall, status: output.status.code(), outcomes: parse(arm, &text) })
}

fn strip_escapes(line: &str) -> String {
    let mut plain = String::new();
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            characters.by_ref().find(char::is_ascii_alphabetic);
        } else {
            plain.push(character);
        }
    }
    plain
}

fn case_name(text: &str) -> Option<String> {
    let start = text.find("cases::")?;
    text[start..].split_whitespace().next().map(str::to_string)
}

fn classify(arm: Arm, line: &str) -> Option<(String, bool)> {
    let plain = strip_escapes(line);
    let text = plain.trim();
    let passed = match arm {
        Arm::Test => {
            let verdict = text.strip_prefix("test ")?.rsplit_once(" ... ")?.1;
            match verdict {
                "ok" => true,
                "FAILED" => false,
                _ => return None,
            }
        }
        Arm::Nextest if text.starts_with("PASS ") => true,
        Arm::Nextest if text.starts_with("FAIL ") => false,
        Arm::Val | Arm::ValTest | Arm::ValShardHalf | Arm::ValShardOne | Arm::ValShardTwo if text.ends_with(" PASS") => true,
        Arm::Val | Arm::ValTest | Arm::ValShardHalf | Arm::ValShardOne | Arm::ValShardTwo if text.ends_with(" FAIL") => false,
        _ => return None,
    };
    Some((case_name(text)?, passed))
}

fn parse(arm: Arm, text: &str) -> Outcomes {
    text.lines().filter_map(|line| classify(arm, line)).collect()
}

fn as_float(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        return sorted[middle];
    }
    f64::midpoint(sorted[middle - 1], sorted[middle])
}

fn coefficient_of_variation(values: &[f64]) -> f64 {
    let count = as_float(values.len());
    let mean = values.iter().sum::<f64>() / count;
    if values.len() < 2 || mean == 0.0 {
        return 0.0;
    }
    let variance = values.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / (count - 1.0);
    variance.sqrt() / mean * 100.0
}

fn check_against_expected(arm: Arm, label: &str, observation: &Observation, expected: &Outcomes, problems: &mut Vec<String>) {
    if observation.outcomes == *expected {
        return;
    }
    let missing = expected.iter().filter(|(name, passed)| observation.outcomes.get(*name) != Some(*passed)).count();
    let extra = observation.outcomes.keys().filter(|name| !expected.contains_key(*name)).count();
    problems.push(format!(
        "{} {label}: parsed {} outcomes, {missing} missing or flipped against the generated set, {extra} unexpected, exit {:?}",
        arm.label(),
        observation.outcomes.len(),
        observation.status
    ));
}

fn print_cross_arm_differences(last: &BTreeMap<Arm, Outcomes>, problems: &mut Vec<String>) {
    let names = last.values().flat_map(BTreeMap::keys).collect::<BTreeSet<_>>();
    let differing = names
        .into_iter()
        .filter(|name| last.values().map(|outcomes| outcomes.get(*name)).collect::<BTreeSet<_>>().len() > 1)
        .collect::<Vec<_>>();
    if differing.is_empty() {
        return;
    }
    problems.push(format!("{} tests have outcomes that differ between arms", differing.len()));
    for name in differing.into_iter().take(SHOWN_DIFFERENCES) {
        let cells = last
            .iter()
            .map(|(arm, outcomes)| format!("{}={}", arm.label(), outcomes.get(name).map_or("absent", |passed| if *passed { "pass" } else { "fail" })))
            .collect::<Vec<_>>();
        println!("  differs: {name}: {}", cells.join(", "));
    }
}

fn print_table(walls: &BTreeMap<Arm, Vec<f64>>, last: &BTreeMap<Arm, Outcomes>) {
    println!("{:<20} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7}", "arm", "round 1", "round 2", "round 3", "median", "cov %", "passed", "failed");
    for (arm, seconds) in walls {
        let outcomes = &last[arm];
        let passed = outcomes.values().filter(|passed| **passed).count();
        let cells = seconds.iter().map(|value| format!("{value:>9.3}")).collect::<Vec<_>>().join(" ");
        println!(
            "{:<20} {cells} {:>9.3} {:>7.2} {:>8} {:>7}",
            arm.label(),
            median(seconds),
            coefficient_of_variation(seconds),
            passed,
            outcomes.len() - passed
        );
    }
}

fn print_launch_cost(walls: &BTreeMap<Arm, Vec<f64>>, tests: usize) {
    let Some(test) = walls.get(&Arm::Test) else {
        println!("per-launch cost: unavailable without cargo test");
        return;
    };
    for arm in ARMS.into_iter().filter(|arm| !matches!(arm, Arm::Test | Arm::Nextest)) {
        let Some(val) = walls.get(&arm) else {
            continue;
        };
        let microseconds = (median(val) - median(test)) / as_float(tests) * 1e6;
        println!("per-launch cost: ({} median - cargo test median) / {tests} tests = {microseconds:.1} us", arm.label());
    }
}

fn measure() -> Result<bool, String> {
    let context = context()?;
    let expected = expected(&context.shape);
    println!(
        "scale: {} passing + {PLANTED} planted failing = {} tests over {} lib binaries, jobs {}, fixture {}",
        context.shape.tests,
        expected.len(),
        context.shape.binaries,
        context.jobs,
        context.fixture.display()
    );
    generate(&context)?;
    build(&context)?;
    let arms = ARMS.iter().copied().filter(|arm| *arm != Arm::Nextest || nextest_available()).collect::<Vec<_>>();
    if !arms.contains(&Arm::Nextest) {
        println!("scale: cargo-nextest not found, skipping the nextest arm");
    }
    let mut problems = Vec::new();
    let mut last = BTreeMap::new();
    for arm in &arms {
        let warm = run_arm(*arm, &context)?;
        check_against_expected(*arm, "warm run", &warm, &expected, &mut problems);
    }
    let mut walls = BTreeMap::<Arm, Vec<f64>>::new();
    for round in 0..ROUNDS {
        let mut order = ARMS;
        order.rotate_left(round * 3 % ARMS.len());
        for arm in order.iter().filter(|arm| arms.contains(arm)) {
            let observation = run_arm(*arm, &context)?;
            check_against_expected(*arm, &format!("round {}", round + 1), &observation, &expected, &mut problems);
            walls.entry(*arm).or_default().push(observation.wall.as_secs_f64());
            last.insert(*arm, observation.outcomes);
        }
    }
    print_table(&walls, &last);
    print_launch_cost(&walls, expected.len());
    print_cross_arm_differences(&last, &mut problems);
    for problem in &problems {
        println!("scale: OUTCOME MISMATCH: {problem}");
    }
    Ok(problems.is_empty())
}
