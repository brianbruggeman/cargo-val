#![cfg(unix)]
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
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
    format!("------ {stream}: views_pair fixture::{test} ------\n")
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
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/views_pair/Cargo.toml"))
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
        .env("CARGO_VAL_JOBS", if process == "shard" { "1" } else { "4" })
        .env("CARGO_TARGET_DIR", directory(&format!("{label}-target")).join("target"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
    let expected = pattern(seed, total);
    let (stdout, stderr) = (fenced(output, &open("stdout", test), &close("stdout")), fenced(output, &open("stderr", test), &close("stderr")));
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
