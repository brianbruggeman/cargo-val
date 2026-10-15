use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::time::Duration;

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[cfg(unix)]
use common::group;
#[cfg(unix)]
#[path = "common/lead.rs"]
mod lead;
#[cfg(unix)]
#[path = "common/wait.rs"]
mod wait;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;
#[cfg(unix)]
use common::{drain, drain_until};
#[cfg(unix)]
use futures::future::join3;
#[cfg(unix)]
use proxima_primitives::pipe::Pipe;
#[cfg(unix)]
#[path = "common/supervise.rs"]
mod supervise;
#[cfg(unix)]
use supervise::supervised;
#[cfg(unix)]
#[path = "common/lock_holder.rs"]
mod lock_holder;
#[cfg(unix)]
use lock_holder::hold_build_lock;

#[cfg(unix)]
const WAIT_LINE: &str = "cargo-val: waiting for ";

fn scratch(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(directory.join("holder/src")).expect("scratch directory is creatable");
    directory
}

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace/Cargo.toml")
}

// the package cache lock is global to CARGO_HOME and cannot be isolated, so the holder and cargo val each get a private CARGO_HOME: a held build lock is then the only contention under test
fn cargo_val(scratch: &Path, shared_target: &Path, arguments: &[&str]) -> Command {
    let mut command = binary::command();
    command
        .env("CARGO_VAL_OUTPUT", "plain")
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .args(arguments)
        .env_remove("RUST_TEST_THREADS")
        .env_remove("CARGO_VAL_JOBS")
        .env_remove("CARGO_VAL_TARGET_DIR")
        .env("NO_COLOR", "1")
        .env("CARGO_HOME", scratch.join("cargo-home"))
        .env("CARGO_TARGET_DIR", shared_target);
    command
}

// cargo 1.99 holds separate locks for the artifact and build directories; the package cache lock is global and is not under test
#[cfg(unix)]
fn directory_wait(line: &str) -> bool {
    line.starts_with(WAIT_LINE) && !line.contains("package cache")
}

fn passed(stdout: &str) -> usize {
    let total = stdout
        .lines()
        .find(|line| line.starts_with("total: "))
        .unwrap_or_else(|| panic!("a total line is printed:\n{stdout}"));
    let counted = total
        .split(", ")
        .find_map(|part| part.strip_prefix("passed "))
        .unwrap_or_else(|| panic!("the total line carries passed: {total}"));
    counted.parse().expect("passed is a count")
}

#[cfg(unix)]
fn wait_line_deadline() -> Duration {
    common::settings::bound("WAIT_LINE_DEADLINE")
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn a_cargo_val_run_completes_while_another_cargo_holds_the_shared_build_lock() {
    let scratch = scratch("isolated-lock");
    let shared = scratch.join("shared-target");
    let mut holder = hold_build_lock(&scratch, &shared);

    let output = fixture_output(&mut cargo_val(&scratch, &shared, &["--target-dir", "isolated"])).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(holder.still_building(), "the lock was held for the whole run");
    assert_eq!(output.status.code(), Some(0), "{stdout}{stderr}");
    assert!(passed(&stdout) > 0, "{stdout}");
    let directory_blocks: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("Blocking waiting for file lock on") && !line.contains("package cache"))
        .collect();
    assert!(directory_blocks.is_empty(), "no child blocked on a target directory lock: {stderr}");
    assert!(!stderr.lines().any(directory_wait), "no wait line named a target directory: {stderr}");
    assert!(shared.join("cargo-val").is_dir(), "the run built under the isolated subtree");
    holder.release().await;
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn shared_mode_waits_on_the_held_lock_and_says_so_once_per_child() {
    let scratch = scratch("shared-lock");
    let shared = scratch.join("shared-target");
    let holder = hold_build_lock(&scratch, &shared);

    let mut command = cargo_val(&scratch, &shared, &["--target-dir", "shared"]);
    let (status, stdout, lines) = supervised(&mut command, wait_line_deadline(), async |_, stdout, stderr, exit| {
        let watched_stderr = async {
            let (seen, fault) = drain_until(stderr, |bytes| String::from_utf8_lossy(bytes).lines().any(directory_wait)).await;
            assert!(fault.is_none(), "stderr failed to read before a wait line: {fault:?}");
            holder.release().await;
            let (rest, _) = drain(stderr).await;
            [seen, rest].concat()
        };
        let ((stdout, _), stderr_bytes, exit) = join3(drain(stdout), watched_stderr, exit.call(())).await;
        let status = exit.expect("cargo-val exit is observed once").expect("cargo-val is waitable");
        let lines = String::from_utf8_lossy(&stderr_bytes).lines().map(String::from).collect::<Vec<_>>();
        (status, String::from_utf8_lossy(&stdout).into_owned(), lines)
    })
    .await;

    let waits: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|candidate| directory_wait(candidate))
        .collect();
    assert_eq!(status.code(), Some(0), "{stdout}{lines:?}");
    assert!(passed(&stdout) > 0, "{stdout}");
    assert!((1..=2).contains(&waits.len()), "one line per waiting child: {waits:?}");
    assert!(
        waits.iter().all(|wait| wait.contains(shared.to_str().expect("utf-8 path"))),
        "{waits:?}"
    );
}

#[proxima::test(timeout = "310s")]
async fn a_named_target_dir_holds_a_test_and_a_doc_subdirectory() {
    let scratch = scratch("named-target");
    let shared = scratch.join("shared-target");
    let named = scratch.join("named-target");
    let named_text = named.to_str().expect("utf-8 path");

    let by_flag = fixture_output(&mut cargo_val(&scratch, &shared, &["--target-dir", named_text])).await;
    let by_setting = fixture_output(cargo_val(&scratch, &shared, &[]).env("CARGO_VAL_TARGET_DIR", named_text)).await;

    for output in [&by_flag, &by_setting] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(0), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
        assert!(passed(&stdout) > 0, "{stdout}");
    }
    assert!(named.join("test/debug").is_dir());
    assert!(named.join("doc/debug").is_dir());
    assert!(!shared.exists(), "nothing was built in the environment's target directory");
}

#[proxima::test(timeout = "310s")]
async fn an_empty_target_dir_setting_is_refused_before_any_build() {
    let scratch = scratch("empty-target");
    let shared = scratch.join("shared-target");

    let output = fixture_output(cargo_val(&scratch, &shared, &[]).env("CARGO_VAL_TARGET_DIR", "")).await;

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("CARGO_VAL_TARGET_DIR must be isolated, shared, or a non-empty path")
    );
    assert!(!shared.exists());
}


#[cfg(unix)]
async fn children_matching(parent: u32, pattern: &str) -> usize {
    let mut pgrep = Command::new("pgrep");
    pgrep.arg("-P").arg(parent.to_string()).arg("-f").arg(pattern);
    let found = common::output_within(&mut pgrep, common::settings::bound("SYNC_DEADLINE"), "SYNC_DEADLINE").await.expect("pgrep finishes within its deadline");
    String::from_utf8_lossy(&found.stdout).lines().count()
}

#[cfg(unix)]
fn package_cache_wait(line: &str) -> bool {
    line.contains("waiting for file lock on package cache") || line.starts_with("cargo-val: waiting for package cache")
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn the_doc_cargo_starts_only_after_the_test_build_has_output() {
    let scratch = scratch("doc-after-build");
    let shared = scratch.join("shared-target");
    let holder = hold_build_lock(&scratch, &shared);

    let mut command = cargo_val(&scratch, &shared, &["--target-dir", "shared"]);
    let (status, stdout, lines, building, documenting) =
        supervised(&mut command, wait_line_deadline(), async |leader, stdout, stderr, exit| {
            let watched_stderr = async {
                let (seen, fault) = drain_until(stderr, |bytes| String::from_utf8_lossy(bytes).lines().any(directory_wait)).await;
                assert!(fault.is_none(), "stderr failed to read before a wait line: {fault:?}");
                let counts = (children_matching(leader, "test --no-run").await, children_matching(leader, "test --doc").await);
                holder.release().await;
                let (rest, _) = drain(stderr).await;
                ([seen, rest].concat(), counts)
            };
            let ((stdout, _), (stderr_bytes, counts), exit) = join3(drain(stdout), watched_stderr, exit.call(())).await;
            let status = exit.expect("cargo-val exit is observed once").expect("cargo-val is waitable");
            let lines = String::from_utf8_lossy(&stderr_bytes).lines().map(String::from).collect::<Vec<_>>();
            (status, String::from_utf8_lossy(&stdout).into_owned(), lines, counts.0, counts.1)
        })
        .await;

    assert_eq!(building, 1, "the test build is the child that waits on the held lock: {lines:?}");
    assert_eq!(documenting, 0, "no doc cargo exists while the test build has produced nothing: {lines:?}");
    assert_eq!(status.code(), Some(0), "{stdout}{lines:?}");
    assert!(stdout.lines().any(|line| line.starts_with("doc-tests: ")), "the doc tests ran after the build: {stdout}");
    assert!(!lines.iter().any(|line| package_cache_wait(line)), "cargo val's own children never contended for the package cache: {lines:?}");
}

#[cfg(unix)]
#[proxima::test(timeout = "310s")]
async fn a_doc_cargo_started_beside_a_held_test_build_blocks_on_the_build_directory() {
    let scratch = scratch("doc-beside-build");
    let shared = scratch.join("shared-target");
    let holder = hold_build_lock(&scratch, &shared);

    let mut command = Command::new("cargo");
    command
        .arg("test")
        .arg("--doc")
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .env("CARGO_HOME", scratch.join("cargo-home"))
        .env("CARGO_TARGET_DIR", &shared)
        .env("CARGO_TERM_COLOR", "never");
    let (status, blocked, released) = supervised(&mut command, wait_line_deadline(), async |_, stdout, stderr, exit| {
        let watched_stderr = async {
            let (seen, fault) = drain_until(stderr, |bytes| String::from_utf8_lossy(bytes).contains("Blocking waiting for file lock on build directory")).await;
            assert!(fault.is_none(), "stderr failed to read before the lock line: {fault:?}");
            holder.release().await;
            let (rest, _) = drain(stderr).await;
            (String::from_utf8_lossy(&seen).into_owned(), String::from_utf8_lossy(&rest).into_owned())
        };
        let (_, (blocked, released), exit) = join3(drain(stdout), watched_stderr, exit.call(())).await;
        let status = exit.expect("the doc cargo exit is observed once").expect("the doc cargo is waitable");
        (status, blocked, released)
    })
    .await;

    assert!(blocked.contains("build directory"), "the overlapping doc cargo printed the lock line: {blocked}");
    assert!(status.success(), "the doc cargo finishes once released: {released}");
}
