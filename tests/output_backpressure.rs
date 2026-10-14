#![cfg(unix)]
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use memchr::memmem;

#[path = "common/binary.rs"]
mod binary;

const MEBIBYTE: usize = 1_048_576;
const STALL_BOUND: Duration = Duration::from_secs(120);
const SIGINT_STATUS: i32 = 130;

fn open(stream: &str, test: &str) -> String {
    open_in("views_pair", stream, test)
}

fn open_in(binary: &str, stream: &str, test: &str) -> String {
    format!("------ {stream}: {binary} fixture::{test} ------\n")
}

fn close(stream: &str) -> String {
    format!("------ end {stream} ------\n")
}

fn pattern(seed: usize, total: usize) -> Vec<u8> {
    (0..total).map(|index| if index % 64 == 63 { b'\n' } else { b'a' + u8::try_from((index + seed) % 26).unwrap_or(0) }).collect()
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn directory(label: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("output-backpressure-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("the scratch directory is creatable");
    path
}

fn spawn(label: &str, process: &str, environment: &[(&str, String)]) -> Child {
    spawn_in("views_pair", (label, process, if process == "shard" { "1" } else { "4" }), environment, Stdio::null())
}

fn spawn_in(fixture: &str, (label, process, jobs): (&str, &str, &str), environment: &[(&str, String)], stderr: Stdio) -> Child {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{fixture}/Cargo.toml")))
        .args(["--tests", "--no-fail-fast"])
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_VAL_OUTPUT", "plain")
        .env("CARGO_VAL_PROCESS", process)
        .env("CARGO_VAL_JOBS", jobs)
        .env("CARGO_TARGET_DIR", directory(&format!("{label}-target")).join("target"))
        .stdout(Stdio::piped())
        .stderr(stderr);
    command.envs(environment.iter().cloned());
    command.spawn().expect("cargo-val starts")
}

fn read_until(stdout: &mut impl Read, marker: &str) -> Vec<u8> {
    let (mut seen, mut byte) = (Vec::new(), [0_u8; 1]);
    while !seen.ends_with(marker.as_bytes()) {
        assert_eq!(stdout.read(&mut byte).expect("stdout is readable"), 1, "stdout ended before {marker:?}:\n{}", String::from_utf8_lossy(&seen));
        seen.push(byte[0]);
    }
    seen
}

fn fenced<'a>(output: &'a [u8], opening: &str, closing: &str) -> &'a [u8] {
    let start = memmem::find(output, opening.as_bytes()).unwrap_or_else(|| panic!("{opening:?} is printed")) + opening.len();
    let length = memmem::find(&output[start..], closing.as_bytes()).unwrap_or_else(|| panic!("{closing:?} follows {opening:?}"));
    &output[start..start + length]
}

fn at(output: &[u8], needle: &str) -> usize {
    memmem::find(output, needle.as_bytes()).unwrap_or_else(|| panic!("{needle:?} is printed"))
}

fn count(output: &[u8], needle: &str) -> usize {
    memmem::find_iter(output, needle.as_bytes()).count()
}

fn finish(mut child: Child, mut head: Vec<u8>) -> (Vec<u8>, Option<i32>) {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    stdout.read_to_end(&mut head).expect("the rest of stdout is readable");
    (head, child.wait().expect("cargo-val is reaped").code())
}

fn stream_digests(output: &[u8], test: &str, seed: usize, total: usize) -> (u64, u64) {
    digests_in("views_pair", output, test, (seed, total))
}

fn digests_in(binary: &str, output: &[u8], test: &str, (seed, total): (usize, usize)) -> (u64, u64) {
    let expected = pattern(seed, total);
    let (stdout, stderr) = (fenced(output, &open_in(binary, "stdout", test), &close("stdout")), fenced(output, &open_in(binary, "stderr", test), &close("stderr")));
    let begin = |block: &[u8]| memmem::find(block, &expected[..4096]).unwrap_or_else(|| panic!("the {test} pattern starts inside its fence"));
    let (from_stdout, from_stderr) = (begin(stdout), begin(stderr));
    (digest(&stdout[from_stdout..from_stdout + total]), digest(&stderr[from_stderr..from_stderr + total]))
}

#[test]
fn two_failures_settling_in_one_shard_item_each_print_complete_in_settle_order() {
    let child = spawn("pair", "shard", &[("VIEWS_PAIR_BYTES", MEBIBYTE.to_string())]);

    let (output, code) = finish(child, Vec::new());

    let expected = digest(&pattern(0, MEBIBYTE));
    let shifted = digest(&pattern(7, MEBIBYTE));
    assert_eq!(code, Some(1));
    assert_eq!(stream_digests(&output, "fails_a", 0, MEBIBYTE), (expected, expected), "the first block is whole and not repeated");
    assert_eq!(stream_digests(&output, "fails_b", 7, MEBIBYTE), (shifted, shifted), "the second block is whole and not repeated");
    assert_eq!((count(&output, "------ stdout: views_pair"), count(&output, &close("stdout"))), (2, 2), "stdout fences balance");
    assert_eq!((count(&output, "------ stderr: views_pair"), count(&output, &close("stderr"))), (2, 2), "stderr fences balance");
    assert!(at(&output, &open("stdout", "fails_a")) < at(&output, &open("stdout", "fails_b")), "blocks print in settle order");
    assert!(at(&output, &close("stderr")) < at(&output, &open("stdout", "fails_b")), "the first block closes before the second opens");
}

#[test]
fn the_summary_follows_every_block_and_no_block_opens_after_it() {
    let child = spawn("order", "shard", &[("VIEWS_PAIR_BYTES", MEBIBYTE.to_string())]);

    let (output, _) = finish(child, Vec::new());

    let summary = at(&output, "SUMMARY");
    let last_close = memmem::rfind(&output, close("stderr").as_bytes()).expect("a block closed");
    assert!(last_close < summary, "the closing block comes after the last failure block");
    assert_eq!(memmem::rfind(&output, b"------ stdout: views_pair").filter(|opened| *opened > summary), None, "no block opens after the summary");
}

fn fifo(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    assert!(Command::new("mkfifo").arg(&path).status().expect("mkfifo runs").success(), "the fifo {name} is created");
    path
}

#[test]
fn a_producer_finishes_four_mebibytes_while_a_sixteen_mebibyte_block_waits_on_an_unread_screen() {
    let scratch = directory("throttle");
    let (go, done) = (fifo(&scratch, "go"), fifo(&scratch, "done"));
    let mut child = spawn("throttle", "test", &[("VIEWS_PAIR_BIG_BYTES", (8 * MEBIBYTE).to_string()), ("VIEWS_PAIR_GO", go.display().to_string()), ("VIEWS_PAIR_DONE", done.display().to_string())]);
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let head = read_until(&mut stdout, &open("stdout", "fails_big"));

    File::options().write(true).open(&go).expect("the producer is waiting on the go fifo").write_all(b"g").expect("the producer is released");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut finished = [0_u8; 1];
        let outcome = File::open(&done).and_then(|mut fifo| fifo.read_exact(&mut finished));
        let _ = sender.send(outcome.is_ok());
    });
    let producer_finished = receiver.recv_timeout(STALL_BOUND);
    if producer_finished != Ok(true) {
        let _ = child.kill();
    }
    assert_eq!(producer_finished, Ok(true), "the producer completed its 4 MiB write while the block was still unread on the screen");

    child.stdout = Some(stdout);
    let (output, code) = finish(child, head);
    let expected = digest(&pattern(3, 8 * MEBIBYTE));
    assert_eq!(code, Some(1));
    assert_eq!(stream_digests(&output, "fails_big", 3, 8 * MEBIBYTE), (expected, expected), "the block still prints whole once the screen reads");
}

#[test]
fn an_interrupt_during_a_block_prints_it_whole_then_the_interrupted_block_then_the_summary() {
    let mut child = spawn("interrupt", "shard", &[("VIEWS_PAIR_BYTES", (2 * MEBIBYTE).to_string())]);
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let head = read_until(&mut stdout, &open("stdout", "fails_a"));

    assert_eq!(unsafe { libc::kill(child.id().cast_signed(), libc::SIGINT) }, 0, "the interrupt lands");
    child.stdout = Some(stdout);
    let (output, code) = finish(child, head);

    let text = String::from_utf8_lossy(&output).into_owned();
    let expected = digest(&pattern(0, 2 * MEBIBYTE));
    assert_eq!(stream_digests(&output, "fails_a", 0, 2 * MEBIBYTE), (expected, expected), "the pending block prints in full");
    assert_eq!(count(&output, "------ stdout: views_pair"), count(&output, &close("stdout")), "stdout fences balance:\n{}", &text[text.len().saturating_sub(600)..]);
    let (interrupted, summary) = (at(&output, "INTERRUPTED"), at(&output, "SUMMARY"));
    assert!(memmem::rfind(&output, close("stderr").as_bytes()).is_some_and(|closed| closed < interrupted), "every block closes before INTERRUPTED");
    assert!(interrupted < summary, "INTERRUPTED precedes SUMMARY");
    assert_eq!(code, Some(SIGINT_STATUS));
}

const SETTLERS: usize = 2000;
const BULK_FAILURES: usize = 40;
const BULK_BYTES: usize = 64 * 1024;
const PAUSE_NOTICE: &str = "tests pause until it catches up\n";

fn settled_in_order(output: &[u8]) -> Vec<usize> {
    String::from_utf8_lossy(output)
        .lines()
        .filter(|line| line.starts_with("PASS"))
        .filter_map(|line| line.split_once("fixture::settle_"))
        .filter_map(|(_, rest)| rest.get(..4)?.parse().ok())
        .collect()
}

fn target_of(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("output-backpressure-{label}-target-{}", std::process::id())).join("target")
}

fn store_bytes(label: &str) -> Vec<u8> {
    let runs = target_of(label).join("cargo-val/run");
    let run = std::fs::read_dir(&runs).expect("the run store directory exists").next().expect("one run directory").expect("the run directory reads");
    std::fs::read(run.path().join("data")).expect("the run store data file reads")
}

fn release_and_count(go: &Path, done: &Path, count: usize) -> (File, mpsc::Receiver<bool>) {
    let reader = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(done).expect("the done fifo opens for reading");
    let keeper = File::options().write(true).open(done).expect("the done fifo is held open so the reader sees no end of file between settlers");
    assert_eq!(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, 0) }, 0, "the done reader blocks again");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut finished = vec![0_u8; count];
        let _ = sender.send(reader.read_exact(&mut finished).is_ok());
        drop(keeper);
    });
    let mut release = File::options().write(true).open(go).expect("a settler is waiting on the go fifo");
    release.write_all(&vec![b'g'; count]).expect("every settler is released");
    (release, receiver)
}

#[test]
fn two_thousand_settlers_finish_behind_an_unread_eight_mebibyte_block_and_print_in_order_after_it_from_the_store() {
    let scratch = directory("flood");
    let (go, done) = (fifo(&scratch, "go"), fifo(&scratch, "done"));
    let environment = [("VIEWS_FLOOD_BIG_BYTES", (8 * MEBIBYTE).to_string()), ("VIEWS_FLOOD_GO", go.display().to_string()), ("VIEWS_FLOOD_DONE", done.display().to_string())];
    let mut child = spawn_in("views_flood", ("flood", "test", "1"), &environment, Stdio::null());
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let head = read_until(&mut stdout, &open_in("views_flood", "stdout", "fails_big"));

    let (_release, receiver) = release_and_count(&go, &done, SETTLERS);
    let settlers_finished = receiver.recv_timeout(STALL_BOUND);
    if settlers_finished != Ok(true) {
        let _ = child.kill();
    }
    assert_eq!(settlers_finished, Ok(true), "all {SETTLERS} settlers ran and settled while the 8 MiB block was still unread on the screen");

    child.stdout = Some(stdout);
    let (output, code) = finish(child, head);
    let expected = digest(&pattern(3, 8 * MEBIBYTE));
    assert_eq!(code, Some(1));
    assert_eq!(digests_in("views_flood", &output, "fails_big", (3, 8 * MEBIBYTE)), (expected, expected), "the block prints whole");
    assert_eq!(settled_in_order(&output), (0..SETTLERS).collect::<Vec<_>>(), "every settle line printed once, in the order the tests settled");
    assert!(at(&output, "fixture::settle_0000") > at(&output, &close("stderr")), "the settle lines print after the block they settled behind");
    let kept = store_bytes("flood");
    let in_store = memmem::find_iter(&kept, b"PASS     views_flood  fixture::settle_").count();
    println!("settle lines found in the run store scrollback: {in_store} of {SETTLERS}");
    assert!(in_store > 0, "the lines the ring had no room for were sealed into the run store");
    assert!(memmem::find(&kept, b"fixture::settle_1999").is_some(), "the last settle line is in the store");
}

#[test]
fn forty_failing_tests_with_sixty_four_kibibytes_each_all_print_in_full() {
    let child = spawn_in("views_flood", ("bulk", "test", "4"), &[("VIEWS_FLOOD_BULK_BYTES", BULK_BYTES.to_string())], Stdio::null());

    let (output, code) = finish(child, Vec::new());

    assert_eq!(code, Some(1));
    for index in 0..BULK_FAILURES {
        let expected = digest(&pattern(index, BULK_BYTES));
        assert_eq!(digests_in("views_flood", &output, &format!("bulk_{index:02}"), (index, BULK_BYTES)), (expected, expected), "block {index} is whole, not a bounded window");
    }
    assert_eq!((count(&output, "------ stdout: views_flood"), count(&output, &close("stdout"))), (BULK_FAILURES, BULK_FAILURES), "stdout fences balance");
    assert_eq!((count(&output, "------ stderr: views_flood"), count(&output, &close("stderr"))), (BULK_FAILURES, BULK_FAILURES), "stderr fences balance");
}

#[test]
fn a_read_only_target_pauses_with_one_notice_and_loses_no_line_or_order() {
    let target = directory("memory").join("target");
    std::fs::create_dir_all(target.join("cargo-val")).expect("the store directory is creatable");
    std::fs::set_permissions(target.join("cargo-val"), std::fs::Permissions::from_mode(0o555)).expect("the store directory turns read-only");
    let environment = [("VIEWS_FLOOD_BIG_BYTES", (2 * MEBIBYTE).to_string()), ("CARGO_TARGET_DIR", target.display().to_string())];
    let mut child = spawn_in("views_flood", ("memory", "test", "1"), &environment, Stdio::piped());
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let mut seen = read_until(&mut stderr, PAUSE_NOTICE);

    let (output, code) = finish(child, Vec::new());
    stderr.read_to_end(&mut seen).expect("the rest of stderr is readable");
    std::fs::set_permissions(target.join("cargo-val"), std::fs::Permissions::from_mode(0o755)).expect("the store directory is writable again");

    assert_eq!(code, Some(1));
    assert_eq!(count(&seen, PAUSE_NOTICE), 1, "the pause says so once");
    assert_eq!((count(&output, "------ stdout: views_flood"), count(&output, &close("stdout"))), (1, 1), "the failure block opens and closes, from the capture window the memory-only run kept");
    assert_eq!(settled_in_order(&output), (0..SETTLERS).collect::<Vec<_>>(), "nothing was lost and the order held while the run paused");
}
