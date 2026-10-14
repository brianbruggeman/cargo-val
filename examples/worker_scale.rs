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
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[path = "../tests/common/group.rs"]
mod group;
#[path = "../tests/common/output.rs"]
mod output;

const PLANTED: usize = 8;
const WORKER_DEADLINE: Duration = Duration::from_secs(3600);
const ROUNDS: usize = 3;
const CHUNK: usize = 64;
const END_TO_END: &str = "proxima worker pool (end to end)";
const FEED_ONLY: &str = "proxima worker pool (feed only, pool warm)";

const WORKER_MAIN: &str = r#"
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Phases {
    tests: usize,
    first_read: Duration,
    spawn_join: Duration,
    env_snapshot: Duration,
    env_restore: Duration,
    cwd: Duration,
    test_body: Duration,
    write_flush: Duration,
    read_name: Duration,
}

fn describe(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("non-string panic payload"));
    text.replace('\n', " | ")
}

fn restore_environment(environment: &[(OsString, OsString)]) {
    let drifted = std::env::vars_os().map(|(key, _)| key).filter(|key| !environment.iter().any(|(kept, _)| kept == key)).collect::<Vec<_>>();
    // the test thread has been joined, so no other thread can race the environment
    unsafe {
        drifted.iter().for_each(|key| std::env::remove_var(key));
        environment.iter().for_each(|(key, value)| std::env::set_var(key, value));
    }
}

fn run_case(case: fn(), phases: &mut Phases) -> Result<(), String> {
    let started = Instant::now();
    let environment = std::env::vars_os().collect::<Vec<_>>();
    phases.env_snapshot += started.elapsed();
    let started = Instant::now();
    let directory = std::env::current_dir().ok();
    phases.cwd += started.elapsed();
    let started = Instant::now();
    let joined = std::thread::spawn(move || {
        let body = Instant::now();
        let outcome = std::panic::catch_unwind(case);
        (body.elapsed(), outcome)
    })
    .join();
    let (body, outcome) = joined.unwrap_or_else(|payload| (Duration::ZERO, Err(payload)));
    phases.spawn_join += started.elapsed().saturating_sub(body);
    phases.test_body += body;
    let started = Instant::now();
    restore_environment(&environment);
    phases.env_restore += started.elapsed();
    let started = Instant::now();
    if let Some(path) = directory.as_deref() {
        let _ = std::env::set_current_dir(path);
    }
    phases.cwd += started.elapsed();
    match outcome {
        Ok(()) => Ok(()),
        Err(payload) => Err(describe(payload.as_ref())),
    }
}

fn main() {
    if std::env::args().any(|argument| argument == "--list") {
        CASES.iter().for_each(|(name, _)| println!("{name}: test"));
        return;
    }
    let worker_started = Instant::now();
    std::panic::set_hook(Box::new(|_| {}));
    let mut output = std::io::stdout().lock();
    let mut input = std::io::stdin().lock();
    let mut phases = Phases::default();
    let mut line = String::new();
    let mut awaiting_first = true;
    loop {
        line.clear();
        let started = Instant::now();
        let read = input.read_line(&mut line);
        if awaiting_first {
            phases.first_read = worker_started.elapsed();
            awaiting_first = false;
        } else {
            phases.read_name += started.elapsed();
        }
        let Ok(read) = read else { break };
        if read == 0 {
            break;
        }
        let name = line.trim();
        let Ok(position) = CASES.binary_search_by_key(&name, |(known, _)| *known) else {
            let _ = writeln!(output, "failed {name} unknown test");
            continue;
        };
        let outcome = run_case(CASES[position].1, &mut phases);
        phases.tests += 1;
        let started = Instant::now();
        let _ = match outcome {
            Ok(()) => writeln!(output, "ok {name}"),
            Err(message) => writeln!(output, "failed {name} {message}"),
        };
        let _ = output.flush();
        phases.write_flush += started.elapsed();
    }
    eprintln!(
        "worker_phases: tests={} first_read_us={} spawn_join_us={} env_snapshot_us={} env_restore_us={} cwd_us={} test_body_us={} write_flush_us={} read_name_us={}",
        phases.tests,
        phases.first_read.as_micros(),
        phases.spawn_join.as_micros(),
        phases.env_snapshot.as_micros(),
        phases.env_restore.as_micros(),
        phases.cwd.as_micros(),
        phases.test_body.as_micros(),
        phases.write_flush.as_micros(),
        phases.read_name.as_micros(),
    );
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
    stderr: JoinHandle<io::Result<String>>,
    cancel: Sender<()>,
    fired: JoinHandle<bool>,
    chunks: usize,
    round_trip: Duration,
}

const PHASE_PREFIX: &str = "worker_phases: ";
const PHASE_COUNT: usize = 8;
const PHASES: [&str; PHASE_COUNT] = ["first_read_us", "spawn_join_us", "env_snapshot_us", "env_restore_us", "cwd_us", "test_body_us", "write_flush_us", "read_name_us"];

#[derive(Clone, Copy, Default)]
struct WorkerPhases {
    tests: usize,
    micros: [u64; PHASE_COUNT],
    chunks: usize,
    round_trip_micros: u64,
}

struct Round {
    wall: Duration,
    outcomes: Outcomes,
    created: usize,
    phases: Vec<WorkerPhases>,
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
    let mut build = Command::new("cargo");
    build
        .args(["test", "--tests", "--no-run", "--target-dir"])
        .arg(&context.target)
        .current_dir(&context.fixture)
        .env_remove("CARGO_TARGET_DIR");
    let output = output::output_bounded(&mut build, WORKER_DEADLINE)?;
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
    let mut listing = Command::new(executable);
    listing.arg("--list");
    let output = output::output_bounded(&mut listing, group::SYNC_DEADLINE).map_err(|failure| format!("list {}: {failure}", executable.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().filter_map(|line| line.strip_suffix(": test")).map(str::to_string).collect())
}

fn spawn_worker(executable: &Path, directory: &Path) -> Result<Worker, String> {
    let mut command = Command::new(executable);
    group::lead_group(&mut command);
    let mut child = command
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn worker {}: {error}", executable.display()))?;
    let stdin = child.stdin.take().ok_or("worker stdin not piped")?;
    let stdout = child.stdout.take().ok_or("worker stdout not piped")?;
    let mut stderr = child.stderr.take().ok_or("worker stderr not piped")?;
    let (cancel, fired) = group::watchdog(child.id(), WORKER_DEADLINE);
    // stderr is drained as the worker writes it, so a full stderr pipe can never block the worker while this side waits on its stdout
    let stderr = thread::spawn(move || {
        let mut report = String::new();
        stderr.read_to_string(&mut report).map(|_| report)
    });
    Ok(Worker { child, stdin, reader: BufReader::new(stdout), stderr, cancel, fired, chunks: 0, round_trip: Duration::ZERO })
}

fn run_chunk(worker: &mut Worker, names: &[String]) -> Result<Vec<(String, bool)>, String> {
    let request = names.iter().fold(String::new(), |mut text, name| {
        text.push_str(name);
        text.push('\n');
        text
    });
    let started = Instant::now();
    worker.stdin.write_all(request.as_bytes()).and_then(|()| worker.stdin.flush()).map_err(|error| format!("write to worker: {error}"))?;
    let results = names
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
        .collect::<Result<Vec<_>, String>>()?;
    worker.round_trip += started.elapsed();
    worker.chunks += 1;
    Ok(results)
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

fn parse_phases(report: &str) -> Result<WorkerPhases, String> {
    let fields = report.lines().find_map(|line| line.strip_prefix(PHASE_PREFIX)).ok_or_else(|| format!("worker wrote no phase line: {report:?}"))?;
    let mut parsed = WorkerPhases::default();
    let mut seen = 0;
    for field in fields.split(' ') {
        let (key, value) = field.split_once('=').ok_or_else(|| format!("malformed phase field {field:?}"))?;
        if key == "tests" {
            parsed.tests = value.parse().map_err(|error| format!("phase tests={value:?}: {error}"))?;
        } else {
            let slot = PHASES.iter().position(|known| *known == key).ok_or_else(|| format!("unknown phase {key:?}"))?;
            parsed.micros[slot] = value.parse().map_err(|error| format!("phase {key}={value:?}: {error}"))?;
        }
        seen += 1;
    }
    if seen != PHASE_COUNT + 1 {
        return Err(format!("worker phase line has {seen} fields, expected {}: {fields:?}", PHASE_COUNT + 1));
    }
    Ok(parsed)
}

fn shut_down(workers: Vec<Worker>) -> Result<Vec<WorkerPhases>, String> {
    workers
        .into_iter()
        .map(|worker| {
            let Worker { mut child, stdin, reader, stderr, cancel, fired, chunks, round_trip } = worker;
            drop(stdin);
            drop(reader);
            let report = stderr.join().map_err(|_| String::from("worker stderr reader panicked"))?;
            let waited = child.wait();
            drop(cancel);
            if fired.join().unwrap_or(false) {
                return Err(format!("worker outlived {WORKER_DEADLINE:?} and its process group was killed"));
            }
            waited.map_err(|error| format!("wait for worker: {error}"))?;
            let report = report.map_err(|error| format!("read worker stderr: {error}"))?;
            let round_trip_micros = u64::try_from(round_trip.as_micros()).unwrap_or(u64::MAX);
            parse_phases(&report).map(|phases| WorkerPhases { chunks, round_trip_micros, ..phases })
        })
        .collect()
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
    let gathered = std::thread::scope(|scope| {
        let handles = pools.iter_mut().map(|(workers, names)| scope.spawn(move || run_binary(workers, names))).collect::<Vec<_>>();
        handles.into_iter().map(|handle| handle.join().map_err(|_| String::from("binary driver thread panicked"))?).collect::<Result<Vec<_>, String>>()
    })?;
    Ok(gathered.into_iter().flatten().collect())
}

fn end_to_end_round(context: &Context) -> Result<Round, String> {
    let start = Instant::now();
    let mut pools = start_pools(context)?;
    let outcomes = feed(&mut pools)?;
    let created = pools.iter().map(|(workers, _)| workers.len()).sum::<usize>();
    let mut phases = Vec::new();
    for (workers, _) in pools {
        phases.extend(shut_down(workers)?);
    }
    Ok(Round { wall: start.elapsed(), outcomes, created, phases })
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

fn check_phases(label: &str, round: &Round, expected: usize, problems: &mut Vec<String>) {
    let tests = round.phases.iter().map(|worker| worker.tests).sum::<usize>();
    let chunks = round.phases.iter().map(|worker| worker.chunks).sum::<usize>();
    if round.phases.len() != round.created || tests != expected || chunks == 0 {
        problems.push(format!("{label}: {} phase lines for {} workers, {tests} phase tests against {expected} expected, {chunks} chunks", round.phases.len(), round.created));
    }
}

fn seconds(micros: u64) -> f64 {
    Duration::from_micros(micros).as_secs_f64()
}

fn per_test_us(total_seconds: f64, tests: usize) -> f64 {
    total_seconds * 1e6 / as_float(tests)
}

fn print_phases(phases: &[WorkerPhases], timed_wall: Duration, created: usize) {
    let totals = phases.iter().fold([0_u64; PHASE_COUNT], |mut totals, worker| {
        totals.iter_mut().zip(worker.micros).for_each(|(total, micros)| *total += micros);
        totals
    });
    let tests = phases.iter().map(|worker| worker.tests).sum::<usize>();
    let available = timed_wall.as_secs_f64() * as_float(created);
    let attributed = totals.iter().map(|micros| seconds(*micros)).sum::<f64>();
    let unattributed = (available - attributed).max(0.0);
    println!("worker_phases summary: {} worker lines across {ROUNDS} timed rounds, {tests} tests, available worker-time {available:.3} s (end-to-end wall x workers)", phases.len());
    println!("{:<20} {:>14} {:>10} {:>14} {:>12}", "phase", "total us", "us/test", "% attributed", "% of round");
    for (name, micros) in PHASES.iter().zip(totals) {
        let total = seconds(micros);
        println!("{name:<20} {micros:>14} {:>10.2} {:>14.2} {:>12.2}", per_test_us(total, tests), total / attributed * 100.0, total / available * 100.0);
    }
    println!("{:<20} {:>14.0} {:>10.2} {:>14} {:>12.2}", "unattributed", unattributed * 1e6, per_test_us(unattributed, tests), "-", unattributed / available * 100.0);
    println!("{:<20} {:>14.0} {:>10.2} {:>14} {:>12.2}", "available", available * 1e6, per_test_us(available, tests), "-", 100.0);
    let chunks = phases.iter().map(|worker| worker.chunks).sum::<usize>();
    let trip = seconds(phases.iter().map(|worker| worker.round_trip_micros).sum::<u64>());
    println!(
        "driver chunk round trip: {chunks} chunks of up to {CHUNK} names, total {trip:.3} s (request write to last answer read), mean {:.1} us per chunk, {:.2} us per test",
        trip * 1e6 / as_float(chunks),
        per_test_us(trip, tests)
    );
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
    let warm = end_to_end_round(&context)?;
    check("warm end to end", &warm.outcomes, &expected, &mut problems);
    let created = warm.created;
    let mut warm_pool = start_pools(&context)?;
    check("warm feed", &feed(&mut warm_pool)?, &expected, &mut problems);
    let (mut end_to_end, mut feed_only) = (Vec::new(), Vec::new());
    let mut last = Outcomes::new();
    let (mut phases, mut timed_wall) = (Vec::new(), Duration::ZERO);
    for round in 1..=ROUNDS {
        let timed = end_to_end_round(&context)?;
        check(&format!("end to end round {round}"), &timed.outcomes, &expected, &mut problems);
        check_phases(&format!("end to end round {round}"), &timed, expected.len(), &mut problems);
        end_to_end.push(timed.wall.as_secs_f64());
        timed_wall += timed.wall;
        phases.extend(timed.phases);
        let (wall, outcomes) = feed_round(&mut warm_pool)?;
        check(&format!("feed only round {round}"), &outcomes, &expected, &mut problems);
        feed_only.push(wall.as_secs_f64());
        last = outcomes;
    }
    for (workers, _) in warm_pool {
        shut_down(workers)?;
    }
    print_table(&end_to_end, &feed_only, &last, created);
    for problem in &problems {
        println!("worker_scale: OUTCOME MISMATCH: {problem}");
    }
    print_phases(&phases, timed_wall, created);
    Ok(problems.is_empty())
}
