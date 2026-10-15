#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::OnceLock;
use core::pin::pin;
use std::time::{Duration, Instant};

use proxima_core::time::timeout;

#[path = "common/fifo.rs"]
mod fifo;

#[path = "common/exit_wait.rs"]
mod exit_wait;

#[path = "common/binary.rs"]
mod binary;

#[path = "common/group.rs"]
mod group;
#[path = "common/drain.rs"]
mod drain;
#[path = "common/lead.rs"]
mod lead;
#[path = "common/pty.rs"]
mod pty;
#[path = "common/wait.rs"]
mod wait;

use cargo_val::edge_stream_unix::ChildStream;
use exit_wait::{ExitSource, until_exit};
use core::num::NonZeroI32;
use futures::future::join;
use pty::{Pty, open_pty};

const SIGINT_STATUS: i32 = 130;
const SIGTERM_STATUS: i32 = 143;
const IN_FLIGHT: usize = 2;
const DISCOVERED: usize = 3;
const SHOW_CURSOR: &str = "\x1b[?25h";

struct Fixture {
    child: Child,
    group: Option<libc::pid_t>,
    holder: libc::pid_t,
    held: File,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            if let Some(group) = self.group {
                libc::kill(-group, libc::SIGKILL);
            }
            if self.holder > 0 {
                libc::kill(self.holder, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn make_fifo(path: &Path) {
    let name = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no nul byte");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(
        made,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

fn open_nonblocking_reader(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

// read+write so the ready fifo parks until the child writes its pid instead of reading EOF before any writer exists
fn open_ready(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

fn start(label: &str, fixture: &str, arguments: &[&str], block_variable: &str) -> (Fixture, ChildStream) {
    start_as(label, fixture, arguments, block_variable, "rich", Stdio::null())
}

fn start_as(label: &str, fixture: &str, arguments: &[&str], block_variable: &str, output: &str, stdout: Stdio) -> (Fixture, ChildStream) {
    let directory = target_directory()
        .join("c74-rendezvous")
        .join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("rendezvous directory is creatable");
    for name in ["ready", "block", "held"] {
        make_fifo(&directory.join(name));
    }
    let ready = open_ready(&directory.join("ready"));
    let held = open_nonblocking_reader(&directory.join("held"));
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/{fixture}/Cargo.toml"));
    let mut command = binary::command();
    lead::lead_group(&mut command);
    let child = command
        .env("CARGO_VAL_OUTPUT", output)
        .arg("--manifest-path")
        .arg(manifest)
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_MAX_FAIL")
        .env_remove("CARGO_VAL_FAIL_FAST")
        .env_remove("CARGO_VAL_NO_TESTS")
        .env("NO_COLOR", "1")
        .env(block_variable, &directory)
        .env(
            "CARGO_TARGET_DIR",
            target_directory().join(format!("c74-fixture-{label}")),
        )
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .expect("cargo-val binary is spawnable");
    let group = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
    let fixture = Fixture {
        child,
        group: Some(group),
        holder: 0,
        held,
    };
    (fixture, ChildStream::new(OwnedFd::from(ready)))
}

fn alive(target: libc::pid_t) -> bool {
    unsafe {
        libc::kill(target, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}

// a macro rather than an async fn: each ChildStream owns a 20 KiB window, so awaiting a helper that holds one trips clippy::large_futures
macro_rules! interrupt {
    ($label:expr, $fixture:expr, $arguments:expr, $block_variable:expr, $signal:expr, $expected_status:expr) => {{
        let label: &str = $label;
        let (mut fixture, ready) = start(label, $fixture, $arguments, $block_variable);
        let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");
        let exit = ExitSource::new(NonZeroI32::new(cargo_val).expect("a spawned pid is nonzero"));
        let what = "the blocked process never reached the rendezvous";
        let line = until_exit(fifo::first_line(&ready, what), what, &exit, &[]).await;
        fixture.holder = line.trim().parse().expect("the blocked process wrote its pid");
        assert!(alive(fixture.holder), "the blocked process is alive before the signal");

        assert_eq!(unsafe { libc::kill(cargo_val, $signal) }, 0);
        let status = wait::wait_within(&fixture.child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not exit after the signal: {failure}"));
        let held = ChildStream::new(OwnedFd::from(fixture.held.try_clone().expect("the held fifo descriptor duplicates")));
        fifo::until_eof(&held, label).await;

        println!(
            "{{\"event\":\"c74_interrupt\",\"label\":\"{label}\",\"signal\":{},\"status\":{:?},\"held_fifo\":\"eof\"}}",
            $signal,
            status.code()
        );
        assert_eq!(status.code(), Some($expected_status), "{label}");
    }};
}

#[proxima::test(timeout = "310s")]
async fn c72_doc_only_run_forwards_sigint_to_the_doctest_group_and_exits_130() {
    interrupt!("doc-int", "doc_tests", &["--doc"], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGINT, SIGINT_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c72_doc_only_run_forwards_sigterm_to_the_doctest_group_and_exits_143() {
    interrupt!("doc-term", "doc_tests", &["--doc"], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGTERM, SIGTERM_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c72_default_run_forwards_sigint_to_the_doctest_group_and_exits_130() {
    interrupt!("all-int", "doc_tests", &[], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGINT, SIGINT_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c72_default_run_forwards_sigterm_to_the_doctest_group_and_exits_143() {
    interrupt!("all-term", "doc_tests", &[], "CARGO_VAL_FIXTURE_BLOCK_DIR", libc::SIGTERM, SIGTERM_STATUS);
}

#[proxima::test(timeout = "310s")]
async fn c74_running_test_receives_sigint_sent_to_cargo_val_alone_and_exits_130() {
    interrupt!(
        "test-int",
        "blocked_test",
        &["--lib", "unit_blocks_when_requested"],
        "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR",
        libc::SIGINT,
        SIGINT_STATUS
    );
}

#[proxima::test(timeout = "310s")]
async fn an_interrupted_agent_run_names_the_signal_and_the_blocked_test_and_closes_with_exit_130() {
    let (mut fixture, ready) = start_as(
        "agent-int",
        "blocked_test",
        &["--lib", "unit_blocks_when_requested"],
        "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR",
        "agent",
        Stdio::piped(),
    );
    let line = fifo::first_line(&ready, "the blocked test never reached the rendezvous").await;
    fixture.holder = line.trim().parse().expect("the blocked process wrote its pid");
    let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");

    assert_eq!(unsafe { libc::kill(cargo_val, libc::SIGINT) }, 0);
    let status = wait::wait_within(&fixture.child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not exit after the signal: {failure}"));
    let mut report = String::new();
    fixture.child.stdout.take().expect("stdout is piped").read_to_string(&mut report).expect("agent output is UTF-8");

    assert_eq!(status.code(), Some(SIGINT_STATUS), "{report}");
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 3, "INTERRUPTED, RUNNING, EXIT:\n{report}");
    assert_eq!(lines[0], "INTERRUPTED  signal=2  forwarded_groups=1", "{report}");
    assert_eq!(lines[1], "RUNNING  cargo_val_blocked_fixture tests::unit_blocks_when_requested", "{report}");
    assert_eq!(lines[2], "EXIT     130  interrupted", "{report}");
}

struct Observed {
    output: String,
    status: ExitStatus,
    groups: BTreeSet<libc::pid_t>,
    leader_group: libc::pid_t,
}

type Launch = (&'static str, &'static str, usize);

// shorter than the 310s the test attribute allows, so a stuck wait reports its stamps instead of the harness reporting only that the body timed out
const RUN_BUDGET: Duration = Duration::from_secs(280);

const TEST_PROCESSES: Launch = ("test", "--test-threads=2", IN_FLIGHT);
const ONE_SHARD_AT_A_TIME: Launch = ("shard", "--test-threads=1", 1);

fn spawned_arguments((process, threads, _): Launch) -> [&'static str; 7] {
    ["--lib", "--process", process, "--no-fail-fast", "--no-tests=pass", "--", threads]
}

async fn announced_holders(ready: &ChildStream, wanted: usize) -> Vec<libc::pid_t> {
    let mut seen = String::new();
    loop {
        let complete: Vec<&str> = seen.split_inclusive('\n').filter(|line| line.ends_with('\n')).collect();
        if complete.len() >= wanted {
            return complete.iter().map(|line| line.trim().parse().expect("a blocked test wrote its pid")).collect();
        }
        seen.push_str(&fifo::first_line(ready, "a blocked test never reached the rendezvous").await);
    }
}

// the groups are read before the signal because a killed holder has no group left to ask about
async fn interrupt_once_in_flight(run: (&str, &RefCell<Vec<String>>), fixture: &mut Fixture, ready: &ChildStream, (wanted, streams): (usize, &[(&str, &ChildStream)])) -> BTreeSet<libc::pid_t> {
    let cargo_val = libc::pid_t::try_from(fixture.child.id()).expect("pid fits pid_t");
    let exit = ExitSource::new(NonZeroI32::new(cargo_val).expect("a spawned pid is nonzero"));
    stamp(run, "ready_fifo_pids", "enter");
    let holders = until_exit(announced_holders(ready, wanted), "the blocked tests never all reached the rendezvous", &exit, streams).await;
    stamp(run, "ready_fifo_pids", "exit");
    let groups = holders.iter().map(|holder| unsafe { libc::getpgid(*holder) }).collect();
    fixture.holder = holders[0];
    assert_eq!(unsafe { libc::kill(cargo_val, libc::SIGINT) }, 0);
    stamp(run, "sigint_sent", "exit");
    groups
}

// one json line per wait boundary, kept in the caller's log: the test body runs on a worker thread whose stdout the harness does not capture, so only a panic carrying the log reaches the failure report
fn stamp((label, log): (&str, &RefCell<Vec<String>>), wait: &str, phase: &str) {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let elapsed_ms = ORIGIN.get_or_init(Instant::now).elapsed().as_millis();
    log.borrow_mut().push(format!("{{\"event\":\"c74_wait\",\"label\":\"{label}\",\"wait\":\"{wait}\",\"phase\":\"{phase}\",\"elapsed_ms\":{elapsed_ms}}}"));
}

async fn interrupted_plain_run(run: (&str, &RefCell<Vec<String>>), launch: Launch) -> Observed {
    let (mut fixture, ready) = start_as(run.0, "blocked_pair", &spawned_arguments(launch), "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", "plain", Stdio::piped());
    let ready = Box::new(ready);
    let groups = interrupt_once_in_flight(run, &mut fixture, &ready, (launch.2, &[])).await;
    stamp(run, "plain_exit", "enter");
    let status = wait::wait_within(&fixture.child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not exit after the signal: {failure}"));
    stamp(run, "plain_exit", "exit");
    let stdout = OwnedFd::from(fixture.child.stdout.take().expect("stdout is piped"));
    let nonblocking = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    assert_eq!(nonblocking, 0, "the plain stdout turns nonblocking: {}", std::io::Error::last_os_error());
    let stdout = Box::new(ChildStream::new(stdout));
    stamp(run, "plain_stdout_eof", "enter");
    let (bytes, fault) = drain::drain_until(&stdout, |_| false).await;
    stamp(run, "plain_stdout_eof", "exit");
    assert!(fault.is_none(), "the plain output read failed: {fault:?}");
    let output = String::from_utf8(bytes).expect("plain output is UTF-8");
    let leader_group = unsafe { libc::getpgid(0) };
    Observed { output, status, groups, leader_group }
}

async fn interrupted_rich_run(run: (&str, &RefCell<Vec<String>>), launch: Launch) -> Observed {
    let Pty { master, slave } = open_pty();
    let (mut fixture, ready) = start_as(run.0, "blocked_pair", &spawned_arguments(launch), "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR", "rich", Stdio::from(slave));
    let (ready, master) = (Box::new(ready), Box::new(ChildStream::new(OwnedFd::from(master))));
    let leader = fixture.child.id();
    let drained = async {
        stamp(run, "master_eof", "enter");
        let finished = drain::drain_until(&master, |_| false).await;
        stamp(run, "master_eof", "exit");
        finished
    };
    let streams = [("pty", &*master)];
    let work = join(interrupt_once_in_flight(run, &mut fixture, &ready, (launch.2, &streams)), drained);
    stamp(run, "rich_work", "enter");
    let (groups, (bytes, _)) = match group::bounded(leader, group::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE", None, work).await {
        Ok(finished) => finished,
        Err((report, _)) => panic!("{report}"),
    };
    stamp(run, "rich_work", "exit");
    stamp(run, "rich_exit", "enter");
    let status = wait::wait_within(&fixture.child, "SYNC_DEADLINE").await.unwrap_or_else(|failure| panic!("cargo-val did not exit after the signal: {failure}"));
    stamp(run, "rich_exit", "exit");
    let leader_group = unsafe { libc::getpgid(0) };
    Observed { output: String::from_utf8_lossy(&bytes).replace('\r', ""), status, groups, leader_group }
}

fn closing_block(output: &str) -> Vec<String> {
    let from = output.find("INTERRUPTED  ").unwrap_or_else(|| panic!("no INTERRUPTED line was printed:\n{output}"));
    let mut block = Vec::new();
    for line in output[from..].lines() {
        block.push(line.to_owned());
        if line.starts_with("SUMMARY") {
            break;
        }
    }
    block
}

fn summary_counts(line: &str) -> Vec<(String, usize)> {
    line.split_whitespace()
        .skip(1)
        .filter_map(|token| token.split_once('='))
        .filter_map(|(key, value)| value.parse().ok().map(|number| (key.to_owned(), number)))
        .collect()
}

fn count_of(counts: &[(String, usize)], key: &str) -> usize {
    counts.iter().find(|(name, _)| name == key).map_or_else(|| panic!("SUMMARY carries no {key}= key: {counts:?}"), |(_, value)| *value)
}

fn assert_spec_block(observed: &Observed) {
    let block = closing_block(&observed.output);
    let report = format!("{}\nstatus: {:?}", observed.output, observed.status);
    assert_eq!(observed.status.code(), Some(SIGINT_STATUS), "{report}");
    assert_eq!(block.len(), IN_FLIGHT + 2, "INTERRUPTED, one RUNNING per test in flight, SUMMARY and nothing between:\n{report}");
    assert_eq!(block[0], format!("INTERRUPTED  signal=2  forwarded_groups={IN_FLIGHT}"), "{report}");
    let reported: BTreeSet<libc::pid_t> = block[1..=IN_FLIGHT].iter().map(|line| running_group(line, &report)).collect();
    assert_eq!(reported, observed.groups, "each RUNNING line names the real process group of a blocked test:\n{report}");
    assert!(!observed.groups.contains(&observed.leader_group), "the blocked tests lead groups of their own, not the harness group");
    let counts = summary_counts(&block[IN_FLIGHT + 1]);
    let accounted = ["passed", "failed", "ignored", "not_run"].iter().map(|key| count_of(&counts, key)).sum::<usize>();
    assert_eq!(accounted, count_of(&counts, "discovered"), "passed + failed + ignored + not_run = discovered:\n{report}");
    assert_eq!(count_of(&counts, "discovered"), DISCOVERED, "{report}");
}

fn running_group(line: &str, report: &str) -> libc::pid_t {
    let fields: Vec<&str> = line.split("  ").collect();
    assert_eq!(fields.len(), 5, "RUNNING  <binary>  <test>  <age>  killed group=<pgid>: {line:?}\n{report}");
    assert_eq!(fields[0].trim_end(), "RUNNING", "{line:?}");
    assert_eq!(fields[1], "cargo_val_blocked_pair_fixture", "{line:?}");
    assert!(fields[2].starts_with("tests::unit_blocks_"), "{line:?}");
    assert!(fields[3].strip_suffix('s').is_some_and(|age| age.parse::<f64>().is_ok()), "the age is seconds with a unit: {line:?}");
    fields[4].strip_prefix("killed group=").and_then(|group| group.parse().ok()).unwrap_or_else(|| panic!("no killed group=<pgid> on {line:?}"))
}

fn without_ages_groups_and_wall(block: &[String]) -> String {
    let token = |token: &str| match token.rsplit_once('=') {
        Some(("group", _)) => "group=<pgid>".to_owned(),
        Some((key, seconds)) if seconds.strip_suffix('s').is_some_and(|age| age.parse::<f64>().is_ok()) => format!("{key}=<seconds>"),
        None if token.strip_suffix('s').is_some_and(|age| age.parse::<f64>().is_ok()) => "<seconds>".to_owned(),
        _ => token.to_owned(),
    };
    block.iter().map(|line| line.split(' ').map(token).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n")
}

#[proxima::test(timeout = "310s")]
async fn an_interrupted_plain_run_prints_the_specs_interrupted_block_with_each_tests_real_group_and_exits_130() {
    let observed = interrupted_plain_run(("block-plain", &RefCell::new(Vec::new())), TEST_PROCESSES).await;

    println!("{}", closing_block(&observed.output).join("\n"));
    assert_spec_block(&observed);
}

#[proxima::test(timeout = "310s")]
async fn an_interrupted_rich_run_prints_the_same_block_as_plain_after_the_frame_is_erased() {
    let stamps = RefCell::new(Vec::new());
    let both = async { (interrupted_plain_run(("block-rich-reference", &stamps), TEST_PROCESSES).await, interrupted_rich_run(("block-rich", &stamps), TEST_PROCESSES).await) };
    let mut both = pin!(both);
    let Ok((plain, rich)) = timeout(RUN_BUDGET, both.as_mut()).await else {
        panic!("the plain and rich runs outlived {RUN_BUDGET:?}; the last stamp without its exit names the wait:\n{}", stamps.borrow().join("\n"));
    };

    assert_spec_block(&rich);
    assert_eq!(without_ages_groups_and_wall(&closing_block(&rich.output)), without_ages_groups_and_wall(&closing_block(&plain.output)));
    assert_cursor_shown_last(&rich.output);
}

fn assert_cursor_shown_last(output: &str) {
    let summary = output.find("SUMMARY").expect("the closing block carries a SUMMARY line");
    let shown: Vec<usize> = output.match_indices(SHOW_CURSOR).map(|(at, _)| at).collect();
    assert_eq!(shown.len(), 1, "the cursor is shown exactly once:\n{output:?}");
    assert!(shown[0] > summary, "the cursor returns after the interrupted block and SUMMARY:\n{output:?}");
    assert!(output.ends_with(SHOW_CURSOR), "nothing is written after the cursor returns:\n{output:?}");
}

#[proxima::test(timeout = "310s")]
async fn an_interrupted_run_leaves_the_earlier_last_run_record_intact() {
    let record = target_directory().join("c74-fixture-record-int").join("cargo-val").join("last-run");
    let earlier = cargo_val::last_run::encode((258_230, 383, 0, 0, 0, 0));
    fs::create_dir_all(record.parent().expect("record has a directory")).expect("record directory is creatable");
    fs::write(&record, earlier).expect("earlier record is writable");

    interrupt!(
        "record-int",
        "blocked_test",
        &["--lib", "unit_blocks_when_requested"],
        "CARGO_VAL_FIXTURE_TEST_BLOCK_DIR",
        libc::SIGINT,
        SIGINT_STATUS
    );

    assert_eq!(fs::read(&record).expect("record still exists"), earlier);
}

#[proxima::test(timeout = "310s")]
async fn an_interrupted_shard_run_accounts_for_every_discovered_name() {
    let observed = interrupted_plain_run(("block-shard", &RefCell::new(Vec::new())), ONE_SHARD_AT_A_TIME).await;

    let block = closing_block(&observed.output);
    let report = format!("{}\nstatus: {:?}", observed.output, observed.status);
    assert_eq!(observed.status.code(), Some(SIGINT_STATUS), "{report}");
    assert_eq!(block.len(), DISCOVERED + 2, "INTERRUPTED, one RUNNING per test in the shard, SUMMARY:\n{report}");
    assert_eq!(block[0], "INTERRUPTED  signal=2  forwarded_groups=1", "one shard process is one group and one job:\n{report}");
    let groups: BTreeSet<libc::pid_t> = block[1..=DISCOVERED].iter().map(|line| running_group(line, &report)).collect();
    assert_eq!(groups, observed.groups, "every RUNNING line carries the shard process group:\n{report}");
    let counts = summary_counts(&block[DISCOVERED + 1]);
    let accounted = ["passed", "failed", "ignored", "not_run"].iter().map(|key| count_of(&counts, key)).sum::<usize>();
    assert_eq!(accounted, count_of(&counts, "discovered"), "{report}");
    assert_eq!(count_of(&counts, "discovered"), DISCOVERED, "{report}");
}
