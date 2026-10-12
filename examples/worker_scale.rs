//! Measures a worker pool: test binaries with `harness = false` whose main is a reusable worker.
//!
//! Each fixture binary holds a static table of tests, reads newline-delimited names from stdin, runs each on a fresh
//! thread under `catch_unwind` with env and cwd restored afterwards, and never exits between tests.
//! Arm one times a whole round end to end: start `jobs` workers per binary, list, feed every name, shut down.
//! Arm two times only the feed against a pool kept warm across rounds.
//! `SCALE_TESTS` (default 13000) passing tests are split over `SCALE_BINARIES` (default 4) binaries with 8 planted failures.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const PLANTED: usize = 8;
const ROUNDS: usize = 3;
const CHUNK: usize = 64;
const END_TO_END: &str = "proxima worker pool (end to end)";
const FEED_ONLY: &str = "proxima worker pool (feed only, pool warm)";

const WORKER_MAIN: &str = r#"
use std::ffi::OsString;
use std::io::{BufRead, Write};

fn describe(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("non-string panic payload"));
    text.replace('\n', " | ")
}

fn restore(environment: &[(OsString, OsString)], directory: Option<&std::path::Path>) {
    let drifted = std::env::vars_os().map(|(key, _)| key).filter(|key| !environment.iter().any(|(kept, _)| kept == key)).collect::<Vec<_>>();
    // the test thread has been joined, so no other thread can race the environment
    unsafe {
        drifted.iter().for_each(|key| std::env::remove_var(key));
        environment.iter().for_each(|(key, value)| std::env::set_var(key, value));
    }
    if let Some(path) = directory {
        let _ = std::env::set_current_dir(path);
    }
}

fn run_case(case: fn()) -> Result<(), String> {
    let environment = std::env::vars_os().collect::<Vec<_>>();
    let directory = std::env::current_dir().ok();
    let outcome = std::thread::spawn(move || std::panic::catch_unwind(case)).join();
    restore(&environment, directory.as_deref());
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(payload)) | Err(payload) => Err(describe(payload.as_ref())),
    }
}

fn main() {
    if std::env::args().any(|argument| argument == "--list") {
        CASES.iter().for_each(|(name, _)| println!("{name}: test"));
        return;
    }
    std::panic::set_hook(Box::new(|_| {}));
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let name = line.trim();
        let Ok(position) = CASES.binary_search_by_key(&name, |(known, _)| *known) else {
            let _ = writeln!(output, "failed {name} unknown test");
            continue;
        };
        let _ = match run_case(CASES[position].1) {
            Ok(()) => writeln!(output, "ok {name}"),
            Err(message) => writeln!(output, "failed {name} {message}"),
        };
        let _ = output.flush();
    }
}
"#;

type Outcomes = BTreeMap<String, bool>;

#[derive(Clone, Copy)]
struct Shape {
    tests: usize,
    binaries: usize,
}

struct Context {
    shape: Shape,
    fixture: PathBuf,
    target: PathBuf,
    jobs: usize,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

fn main() -> ExitCode {
    match measure() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("worker_scale: {message}");
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
    let fixture = profile.join("worker-fixture");
    let shape = Shape {
        tests: count_from_environment("SCALE_TESTS", 13000)?,
        binaries: count_from_environment("SCALE_BINARIES", 4)?,
    };
    let jobs = std::thread::available_parallelism().map_err(|error| format!("parallelism: {error}"))?.get();
    Ok(Context { shape, target: fixture.join("target"), fixture, jobs })
}

fn passing_name(binary: usize, index: usize) -> String {
    format!("b{binary}_i{index:05}")
}

fn planted_name(binary: usize, ordinal: usize) -> String {
    format!("b{binary}_planted_{ordinal}")
}

fn planted_in(binary: usize, shape: Shape) -> Vec<usize> {
    (0..PLANTED).filter(|ordinal| ordinal % shape.binaries == binary).collect()
}

fn passing_in(binary: usize, shape: Shape) -> usize {
    shape.tests / shape.binaries + usize::from(binary < shape.tests % shape.binaries)
}

fn test_source(binary: usize, shape: Shape) -> String {
    let mut functions = String::new();
    let mut table = String::from("static CASES: &[(&str, fn())] = &[\n");
    for index in 0..passing_in(binary, shape) {
        let name = passing_name(binary, index);
        let _ = writeln!(functions, "fn {name}() {{\n    assert_eq!({index}_usize % 7, {index}_usize % 7);\n}}");
        let _ = writeln!(table, "    (\"cases::{name}\", {name}),");
    }
    for ordinal in planted_in(binary, shape) {
        let name = planted_name(binary, ordinal);
        let _ = writeln!(functions, "fn {name}() {{\n    assert_eq!(1_usize, 2_usize, \"planted failure {ordinal}\");\n}}");
        let _ = writeln!(table, "    (\"cases::{name}\", {name}),");
    }
    table.push_str("];\n");
    format!("{functions}{table}{WORKER_MAIN}")
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| format!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    fs::write(path, text).map_err(|error| format!("write {}: {error}", path.display()))
}

fn generate(context: &Context) -> Result<(), String> {
    let _ = fs::remove_dir_all(&context.fixture);
    let members = (0..context.shape.binaries).map(|binary| format!("\"worker_{binary:02}\"")).collect::<Vec<_>>();
    write_file(&context.fixture.join("Cargo.toml"), &format!("[workspace]\nresolver = \"3\"\nmembers = [{}]\n", members.join(", ")))?;
    for binary in 0..context.shape.binaries {
        let directory = context.fixture.join(format!("worker_{binary:02}"));
        let package = format!(
            "[package]\nname = \"worker_{binary:02}\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n[[test]]\nname = \"t{binary:02}\"\npath = \"tests/cases.rs\"\nharness = false\n"
        );
        write_file(&directory.join("Cargo.toml"), &package)?;
        write_file(&directory.join("tests").join("cases.rs"), &test_source(binary, context.shape))?;
    }
    Ok(())
}

fn expected(shape: Shape) -> Outcomes {
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
    let output = Command::new("cargo")
        .args(["test", "--tests", "--no-run", "--target-dir"])
        .arg(&context.target)
        .current_dir(&context.fixture)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .map_err(|error| format!("spawn cargo test --no-run: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!("fixture build failed: {}", String::from_utf8_lossy(&output.stderr)))
}

fn find_binary(context: &Context, binary: usize) -> Result<PathBuf, String> {
    let deps = context.target.join("debug").join("deps");
    let prefix = format!("t{binary:02}-");
    let entries = fs::read_dir(&deps).map_err(|error| format!("read {}: {error}", deps.display()))?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            let executable = if std::env::consts::EXE_SUFFIX.is_empty() { !name.contains('.') } else { name.ends_with(std::env::consts::EXE_SUFFIX) };
            name.starts_with(&prefix) && executable
        })
        .ok_or_else(|| format!("no test executable {prefix}* in {}", deps.display()))
}

fn list_names(executable: &Path) -> Result<Vec<String>, String> {
    let output = Command::new(executable).arg("--list").output().map_err(|error| format!("list {}: {error}", executable.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().filter_map(|line| line.strip_suffix(": test")).map(str::to_string).collect())
}

fn spawn_worker(executable: &Path, directory: &Path) -> Result<Worker, String> {
    let mut child = Command::new(executable)
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn worker {}: {error}", executable.display()))?;
    let stdin = child.stdin.take().ok_or("worker stdin not piped")?;
    let stdout = child.stdout.take().ok_or("worker stdout not piped")?;
    Ok(Worker { child, stdin, reader: BufReader::new(stdout) })
}

fn run_chunk(worker: &mut Worker, names: &[String]) -> Result<Vec<(String, bool)>, String> {
    let request = names.iter().fold(String::new(), |mut text, name| {
        text.push_str(name);
        text.push('\n');
        text
    });
    worker.stdin.write_all(request.as_bytes()).and_then(|()| worker.stdin.flush()).map_err(|error| format!("write to worker: {error}"))?;
    names
        .iter()
        .map(|name| {
            let mut line = String::new();
            let read = worker.reader.read_line(&mut line).map_err(|error| format!("read from worker: {error}"))?;
            let mut fields = line.trim_end().splitn(3, ' ');
            let (verdict, reported) = (fields.next(), fields.next());
            if read == 0 || reported != Some(name.as_str()) {
                return Err(format!("worker answered {line:?} for {name}"));
            }
            Ok((name.clone(), verdict == Some("ok")))
        })
        .collect()
}

fn drive(worker: &mut Worker, names: &[String], cursor: &AtomicUsize) -> Result<Vec<(String, bool)>, String> {
    let mut results = Vec::new();
    loop {
        let start = cursor.fetch_add(CHUNK, Ordering::Relaxed);
        if start >= names.len() {
            return Ok(results);
        }
        results.extend(run_chunk(worker, &names[start..(start + CHUNK).min(names.len())])?);
    }
}

fn run_binary(workers: &mut [Worker], names: &[String]) -> Result<Vec<(String, bool)>, String> {
    let cursor = AtomicUsize::new(0);
    let gathered = std::thread::scope(|scope| {
        let handles = workers.iter_mut().map(|worker| scope.spawn(|| drive(worker, names, &cursor))).collect::<Vec<_>>();
        handles.into_iter().map(|handle| handle.join().map_err(|_| String::from("driver thread panicked"))?).collect::<Result<Vec<_>, String>>()
    })?;
    Ok(gathered.into_iter().flatten().collect())
}

fn shut_down(workers: Vec<Worker>) {
    for worker in workers {
        let Worker { mut child, stdin, reader } = worker;
        drop(stdin);
        drop(reader);
        let _ = child.wait();
    }
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

fn as_float(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
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

type Pools = Vec<(Vec<Worker>, Vec<String>)>;

fn start_pools(context: &Context) -> Result<Pools, String> {
    (0..context.shape.binaries)
        .map(|binary| {
            let executable = find_binary(context, binary)?;
            let names = list_names(&executable)?;
            let workers = (0..context.jobs).map(|_| spawn_worker(&executable, &context.fixture)).collect::<Result<Vec<_>, _>>()?;
            Ok((workers, names))
        })
        .collect()
}

fn feed(pools: &mut Pools) -> Result<Outcomes, String> {
    let mut outcomes = Outcomes::new();
    for (workers, names) in pools.iter_mut() {
        outcomes.extend(run_binary(workers, names)?);
    }
    Ok(outcomes)
}

fn end_to_end_round(context: &Context) -> Result<(Duration, Outcomes, usize), String> {
    let start = Instant::now();
    let mut pools = start_pools(context)?;
    let outcomes = feed(&mut pools)?;
    let created = pools.iter().map(|(workers, _)| workers.len()).sum::<usize>();
    for (workers, _) in pools {
        shut_down(workers);
    }
    Ok((start.elapsed(), outcomes, created))
}

fn feed_round(pools: &mut Pools) -> Result<(Duration, Outcomes), String> {
    let start = Instant::now();
    let outcomes = feed(pools)?;
    Ok((start.elapsed(), outcomes))
}

fn print_row(label: &str, walls: &[f64], outcomes: &Outcomes) {
    let passed = outcomes.values().filter(|passed| **passed).count();
    let cells = walls.iter().map(|value| format!("{value:>9.3}")).collect::<Vec<_>>().join(" ");
    println!("{label:<44} {cells} {:>9.3} {:>7.2} {:>8} {:>7}", median(walls), coefficient_of_variation(walls), passed, outcomes.len() - passed);
}

fn print_table(end_to_end: &[f64], feed_only: &[f64], outcomes: &Outcomes, created: usize) {
    println!("{:<44} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7}", "arm", "round 1", "round 2", "round 3", "median", "cov %", "passed", "failed");
    print_row(END_TO_END, end_to_end, outcomes);
    print_row(FEED_ONLY, feed_only, outcomes);
    println!("worker processes created: {created} per end-to-end round ({} across {ROUNDS} timed rounds, warm run excluded), plus {created} for the warm pool", created * ROUNDS);
    println!("per test, end to end: {:.1} us; feed only: {:.1} us (median round / {} tests)", median(end_to_end) / as_float(outcomes.len()) * 1e6, median(feed_only) / as_float(outcomes.len()) * 1e6, outcomes.len());
}

fn check(label: &str, outcomes: &Outcomes, expected: &Outcomes, problems: &mut Vec<String>) {
    if outcomes != expected {
        problems.push(format!("{label}: {} outcomes against {} expected", outcomes.len(), expected.len()));
    }
}

fn measure() -> Result<bool, String> {
    let context = context()?;
    let expected = expected(context.shape);
    println!(
        "worker_scale: {} passing + {PLANTED} planted failing = {} tests over {} harness=false binaries, jobs {}, fixture {}",
        context.shape.tests,
        expected.len(),
        context.shape.binaries,
        context.jobs,
        context.fixture.display()
    );
    generate(&context)?;
    build(&context)?;
    let mut problems = Vec::new();
    let (_, warm_outcomes, created) = end_to_end_round(&context)?;
    check("warm end to end", &warm_outcomes, &expected, &mut problems);
    let mut warm_pool = start_pools(&context)?;
    check("warm feed", &feed(&mut warm_pool)?, &expected, &mut problems);
    let (mut end_to_end, mut feed_only) = (Vec::new(), Vec::new());
    let mut last = Outcomes::new();
    for round in 1..=ROUNDS {
        let (wall, outcomes, _) = end_to_end_round(&context)?;
        check(&format!("end to end round {round}"), &outcomes, &expected, &mut problems);
        end_to_end.push(wall.as_secs_f64());
        let (wall, outcomes) = feed_round(&mut warm_pool)?;
        check(&format!("feed only round {round}"), &outcomes, &expected, &mut problems);
        feed_only.push(wall.as_secs_f64());
        last = outcomes;
    }
    for (workers, _) in warm_pool {
        shut_down(workers);
    }
    print_table(&end_to_end, &feed_only, &last, created);
    for problem in &problems {
        println!("worker_scale: OUTCOME MISMATCH: {problem}");
    }
    Ok(problems.is_empty())
}
