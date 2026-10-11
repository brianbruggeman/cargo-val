mod args;
mod arm;
mod binary;
mod exec;
mod fixture;
mod json;
mod parse;
mod stats;

use arm::Arm;
use binary::Binary;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const GPU_LOCK: &str = "/Users/brianbruggeman/repos/slot-0/.long_ctx_backups/gpu.lock";
const PREBUILD_LIMIT: Duration = Duration::from_hours(24);
const NO_RESULTS: exec::Matchers = exec::Matchers { stdout: parse::never_result_line, stderr: parse::never_result_line };

struct Run {
    arm: Arm,
    phase: &'static str,
    iteration: u32,
    command: String,
    outcome: exec::Outcome,
    executed: u64,
    loads: [f64; 3],
    quiet_box: bool,
}

struct Context<'config> {
    config: &'config args::Config,
    cargo_val: Option<Binary>,
    cargo_nextest: Option<Binary>,
    sink: File,
}

fn main() -> ExitCode {
    let default_target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("bench-fixtures");
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--help" || argument == "-h") {
        println!("{}", args::help());
        return ExitCode::SUCCESS;
    }
    let outcome = args::parse(arguments.into_iter(), default_target).and_then(|config| execute(&config));
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("val-bench: {message}");
            ExitCode::from(2)
        }
    }
}

fn execute(config: &args::Config) -> Result<bool, String> {
    let sink = File::create(&config.output).map_err(|error| format!("create {}: {error}", config.output.display()))?;
    let mut context = Context {
        config,
        cargo_val: binary::locate("cargo-val")?,
        cargo_nextest: binary::locate("cargo-nextest")?,
        sink,
    };
    let arms = resolve_arms(&mut context)?;
    let mut healthy = true;
    for fixture in &config.fixtures {
        healthy &= bench_fixture(&mut context, fixture, &arms)?;
    }
    Ok(healthy)
}

fn resolve_arms(context: &mut Context) -> Result<Vec<Arm>, String> {
    let mut available = Vec::new();
    for arm in context.config.arms.clone() {
        let missing = match arm {
            Arm::Nextest => context.cargo_nextest.is_none(),
            Arm::CargoVal => context.cargo_val.is_none(),
            Arm::CargoTest => false,
        };
        if missing {
            let reason = format!("{} not found on PATH", arm.subcommand_binary().unwrap_or("binary"));
            emit_skip(context, arm, &reason)?;
        } else {
            available.push(arm);
        }
    }
    if available.is_empty() {
        return Err("no runnable arms".to_string());
    }
    Ok(available)
}

fn emit_skip(context: &mut Context, arm: Arm, reason: &str) -> Result<(), String> {
    let mut object = json::Object::default();
    object.text("record", "arm_skipped").text("arm", arm.name()).text("reason", reason);
    eprintln!("val-bench: skipping {}: {reason}", arm.name());
    write_line(context, &object.render())
}

fn write_line(context: &mut Context, line: &str) -> Result<(), String> {
    writeln!(context.sink, "{line}")
        .and_then(|()| context.sink.flush())
        .map_err(|error| format!("write {}: {error}", context.config.output.display()))
}

fn target_dir_for(config: &args::Config, dir: &Path) -> PathBuf {
    config.target_dir.join(fixture::target_slug(dir))
}

fn environment(config: &args::Config, dir: &Path) -> Vec<(String, String)> {
    vec![
        ("CARGO_TARGET_DIR".into(), target_dir_for(config, dir).display().to_string()),
        ("CARGO_VAL_JOBS".into(), config.val_jobs.to_string()),
        ("RUST_TEST_THREADS".into(), config.jobs.to_string()),
        ("NO_COLOR".into(), "1".into()),
    ]
}

fn prebuild(context: &Context, arms: &[Arm], dir: &Path, manifest: &str) -> Result<(), String> {
    let jobs = context.config.jobs.to_string();
    let mut build = vec![
        "test".to_string(),
        "--manifest-path".into(),
        manifest.into(),
        "--tests".into(),
        "--jobs".into(),
        jobs.clone(),
        "--no-run".into(),
    ];
    build.extend(context.config.cargo_args.iter().cloned());
    let mut builds = vec![build];
    if arms.contains(&Arm::Nextest) {
        let mut list = vec!["nextest".into(), "list".into(), "--manifest-path".into(), manifest.into(), "--build-jobs".into(), jobs];
        list.extend(context.config.cargo_args.iter().cloned());
        builds.push(list);
    }
    for build in builds {
        let outcome = exec::run("cargo", &build, dir, &environment(context.config, dir), PREBUILD_LIMIT, NO_RESULTS)?;
        if outcome.exit_code != 0 {
            return Err(format!("prebuild `cargo {}` exited {}: {}", build.join(" "), outcome.exit_code, outcome.stderr));
        }
    }
    Ok(())
}

fn bench_fixture(context: &mut Context, fixture: &Path, arms: &[Arm]) -> Result<bool, String> {
    let fixture = fixture.canonicalize().map_err(|error| format!("fixture {}: {error}", fixture.display()))?;
    let dir = match context.config.loop_scale {
        Some(factor) => scaled_dir(&fixture, factor)?,
        None => fixture,
    };
    let manifest = dir.join("Cargo.toml").display().to_string();
    prebuild(context, arms, &dir, &manifest)?;
    let mut measured: BTreeMap<&'static str, Vec<Run>> = BTreeMap::new();
    let mut healthy = true;
    let total = context.config.warmup + context.config.iterations;
    for round in 0..total {
        let runs = run_round(context, arms, &dir, &manifest, round)?;
        healthy &= publish_round(context, &dir, runs, &mut measured)?;
    }
    print_summary(&dir, context.config.filter.as_deref(), &measured);
    Ok(healthy)
}

fn scaled_dir(fixture: &Path, factor: u64) -> Result<PathBuf, String> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (dir, rewrites) = fixture::materialize(fixture, factor, &base)?;
    for rewrite in rewrites {
        eprintln!("val-bench: rewrote loop bound at {rewrite}");
    }
    Ok(dir)
}

fn run_round(context: &Context, arms: &[Arm], dir: &Path, manifest: &str, round: u32) -> Result<Vec<Run>, String> {
    let phase = if round < context.config.warmup { "warmup" } else { "measured" };
    let iteration = round.saturating_sub(context.config.warmup);
    let rotation = usize::try_from(round).unwrap_or(0) % arms.len();
    let mut runs = Vec::new();
    for offset in 0..arms.len() {
        let arm = arms[(rotation + offset) % arms.len()];
        let args = arm.args(manifest, context.config.filter.as_deref(), context.config.jobs, &context.config.cargo_args);
        let loads = exec::load_average();
        let quiet_box = exec::is_quiet(Path::new(GPU_LOCK).exists(), loads[0], context.config.quiet_load);
        let outcome = exec::run("cargo", &args, dir, &environment(context.config, dir), Duration::from_secs(context.config.timeout_secs), arm.matchers())?;
        let executed = arm.tests_run(&outcome.stdout, &outcome.stderr);
        runs.push(Run { arm, phase, iteration, command: format!("cargo {}", args.join(" ")), outcome, executed, loads, quiet_box });
    }
    Ok(runs)
}

fn publish_round(context: &mut Context, dir: &Path, runs: Vec<Run>, measured: &mut BTreeMap<&'static str, Vec<Run>>) -> Result<bool, String> {
    let counts: Vec<u64> = runs.iter().filter(|run| !run.outcome.timed_out).map(|run| run.executed).collect();
    let mismatch = counts.windows(2).any(|pair| pair[0] != pair[1]);
    let mut healthy = !mismatch;
    for run in runs {
        healthy &= run.outcome.exit_code == 0;
        let line = render_run(context, dir, &run, mismatch);
        write_line(context, &line)?;
        if run.phase == "measured" && !run.outcome.timed_out {
            measured.entry(run.arm.name()).or_default().push(run);
        }
    }
    if mismatch {
        eprintln!("val-bench: tests_run mismatch across arms {counts:?} in {}", dir.display());
    }
    Ok(healthy)
}

fn render_run(context: &Context, dir: &Path, run: &Run, mismatch: bool) -> String {
    let config = context.config;
    let mut object = json::Object::default();
    object
        .text("record", "run")
        .text("arm", run.arm.name())
        .text("fixture", &dir.display().to_string())
        .optional_text("filter", config.filter.as_deref())
        .text("phase", run.phase)
        .number("iteration", run.iteration)
        .number("wall_ns", run.outcome.wall_ns)
        .optional_number("first_output_ns", run.outcome.first_output_ns)
        .optional_number("first_result_ns", run.outcome.first_result_ns)
        .number("exit_code", run.outcome.exit_code)
        .number("tests_run", run.executed)
        .flag("tests_run_mismatch", mismatch)
        .flag("timed_out", run.outcome.timed_out)
        .flag("failed", mismatch || run.outcome.timed_out || run.outcome.exit_code != 0)
        .number("user_cpu_ns", run.outcome.user_cpu_ns)
        .number("sys_cpu_ns", run.outcome.sys_cpu_ns)
        .number("max_rss_bytes", run.outcome.max_rss_bytes)
        .number("load_1m", run.loads[0])
        .number("load_5m", run.loads[1])
        .number("load_15m", run.loads[2])
        .flag("quiet_box", run.quiet_box)
        .number("quiet_load_threshold", config.quiet_load)
        .text("target_dir", &target_dir_for(config, dir).display().to_string())
        .number("jobs", config.jobs)
        .number("val_jobs", config.val_jobs)
        .text("cargo_args", &config.cargo_args.join(" "))
        .text("command", &run.command);
    append_binary(&mut object, ["cargo_val_path", "cargo_val_sha256", "cargo_val_size"], context.cargo_val.as_ref());
    append_binary(&mut object, ["cargo_nextest_path", "cargo_nextest_sha256", "cargo_nextest_size"], context.cargo_nextest.as_ref());
    object.text("stdout", &run.outcome.stdout).text("stderr", &run.outcome.stderr);
    object.render()
}

fn append_binary(object: &mut json::Object, keys: [&'static str; 3], binary: Option<&Binary>) {
    let path = binary.map(|found| found.path.display().to_string());
    object.optional_text(keys[0], path.as_deref());
    object.optional_text(keys[1], binary.map(|found| found.sha256.as_str()));
    match binary {
        Some(found) => object.number(keys[2], found.size),
        None => object.optional_text(keys[2], None),
    };
}

fn print_summary(dir: &Path, filter: Option<&str>, measured: &BTreeMap<&'static str, Vec<Run>>) {
    println!("fixture {} filter {}", dir.display(), filter.unwrap_or("(none)"));
    println!("{:<11} {:>3} {:>6} {:>10} {:>10} {:>10} {:>10} {:>7} {:>10}", "arm", "n", "tests", "mean_ms", "median_ms", "min_ms", "max_ms", "cov", "first_ms");
    for (name, runs) in measured {
        let walls: Vec<f64> = runs.iter().map(|run| wall_ms(run.outcome.wall_ns)).collect();
        let tests = runs.last().map_or_else(|| "-".to_string(), |run| run.executed.to_string());
        if let Some(summary) = stats::summarize(&walls) {
            println!(
                "{:<11} {:>3} {:>6} {:>10.3} {:>10.3} {:>10.3} {:>10.3} {:>7.4} {:>10}",
                name, summary.count, tests, summary.mean, summary.median, summary.min, summary.max, summary.cov, first_result_median(runs)
            );
        }
    }
}

fn first_result_median(runs: &[Run]) -> String {
    let mut stamps: Vec<f64> = runs.iter().filter_map(|run| run.outcome.first_result_ns).map(|nanos| wall_ms(u128::from(nanos))).collect();
    stamps.sort_by(f64::total_cmp);
    if stamps.is_empty() {
        return "-".to_string();
    }
    format!("{:.3}", stats::median(&stamps))
}

fn wall_ms(wall_ns: u128) -> f64 {
    f64::from(u32::try_from(wall_ns / 1_000).unwrap_or(u32::MAX)) / 1_000.0
}
