use std::io::{ErrorKind, Read};
use std::path::Path;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Outcome {
    pub wall_ns: u128,
    pub first_output_ns: Option<u64>,
    pub first_result_ns: Option<u64>,
    pub timed_out: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub user_cpu_ns: u64,
    pub sys_cpu_ns: u64,
    pub max_rss_bytes: u64,
}

fn nonnegative<T: TryInto<u64>>(value: T) -> u64 {
    value.try_into().unwrap_or(0)
}

fn timeval_ns(value: libc::timeval) -> u64 {
    nonnegative(value.tv_sec) * 1_000_000_000 + nonnegative(value.tv_usec) * 1_000
}

pub type ResultMatcher = fn(&str) -> bool;

#[derive(Clone, Copy)]
pub struct Matchers {
    pub stdout: ResultMatcher,
    pub stderr: ResultMatcher,
}

const UNSET: u64 = u64::MAX;

struct Stamps {
    first_output: AtomicU64,
    first_result: AtomicU64,
}

impl Default for Stamps {
    fn default() -> Self {
        Stamps { first_output: AtomicU64::new(UNSET), first_result: AtomicU64::new(UNSET) }
    }
}

impl Stamps {
    fn first_output(&self) -> Option<u64> {
        Some(self.first_output.load(Ordering::SeqCst)).filter(|stamp| *stamp != UNSET)
    }

    fn first_result(&self) -> Option<u64> {
        Some(self.first_result.load(Ordering::SeqCst)).filter(|stamp| *stamp != UNSET)
    }

    fn result_pending(&self) -> bool {
        self.first_result.load(Ordering::SeqCst) == UNSET
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(UNSET - 1)
}

// both pipes race to stamp; fetch_min keeps the earliest across streams
fn scan_lines(bytes: &[u8], line_start: &mut usize, started: Instant, stamps: &Stamps, is_result: ResultMatcher) {
    while stamps.result_pending() {
        let Some(offset) = bytes[*line_start..].iter().position(|byte| *byte == b'\n') else {
            return;
        };
        let line = String::from_utf8_lossy(&bytes[*line_start..*line_start + offset]);
        if is_result(line.trim_end_matches('\r')) {
            stamps.first_result.fetch_min(elapsed_ns(started), Ordering::SeqCst);
        }
        *line_start += offset + 1;
    }
}

fn drain<Source: Read + Send + 'static>(source: Option<Source>, started: Instant, stamps: Arc<Stamps>, is_result: ResultMatcher) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut line_start = 0;
        if let Some(mut reader) = source {
            let mut chunk = [0_u8; 8192];
            loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                stamps.first_output.fetch_min(elapsed_ns(started), Ordering::SeqCst);
                bytes.extend_from_slice(&chunk[..read]);
                scan_lines(&bytes, &mut line_start, started, &stamps, is_result);
            }
            if line_start < bytes.len() {
                bytes.push(b'\n');
                scan_lines(&bytes, &mut line_start, started, &stamps, is_result);
                bytes.pop();
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}


fn exit_code_of(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -libc::WTERMSIG(status)
    }
}

fn wait_for(pid: libc::pid_t) -> Result<(i32, libc::rusage), String> {
    let mut status = 0;
    // zeroed rusage is a valid all-integer struct that wait4 fully overwrites
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let reaped = unsafe { libc::wait4(pid, &raw mut status, 0, &raw mut usage) };
        if reaped == pid {
            return Ok((status, usage));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(format!("wait4 on pid {pid} failed: {error}"));
        }
    }
}

#[cfg(target_os = "macos")]
fn rss_bytes(raw: u64) -> u64 {
    raw
}

#[cfg(not(target_os = "macos"))]
fn rss_bytes(raw: u64) -> u64 {
    raw * 1024
}

pub fn run(program: &str, args: &[String], cwd: &Path, envs: &[(String, String)], limit: Duration, matchers: Matchers) -> Result<Outcome, String> {
    let started = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .envs(envs.iter().map(|(key, value)| (key, value)))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn {program} in {}: {error}", cwd.display()))?;
    let pid = libc::pid_t::try_from(child.id()).map_err(|error| format!("pid out of range: {error}"))?;
    let stamps = Arc::new(Stamps::default());
    let stdout_reader = drain(child.stdout.take(), started, Arc::clone(&stamps), matchers.stdout);
    let stderr_reader = drain(child.stderr.take(), started, Arc::clone(&stamps), matchers.stderr);
    let (stop, watcher, expired) = watch(pid, limit);
    let waited = wait_for(pid);
    let _ = stop.send(());
    let _ = watcher.join();
    let (status, usage) = waited?;
    let wall_ns = started.elapsed().as_nanos();
    let stdout = stdout_reader.join().map_err(|_| "stdout reader panicked".to_string())?;
    let stderr = stderr_reader.join().map_err(|_| "stderr reader panicked".to_string())?;
    Ok(Outcome {
        wall_ns,
        first_output_ns: stamps.first_output(),
        first_result_ns: stamps.first_result(),
        timed_out: expired.load(Ordering::SeqCst),
        exit_code: exit_code_of(status),
        stdout,
        stderr,
        user_cpu_ns: timeval_ns(usage.ru_utime),
        sys_cpu_ns: timeval_ns(usage.ru_stime),
        max_rss_bytes: rss_bytes(nonnegative(usage.ru_maxrss)),
    })
}

pub fn load_average() -> [f64; 3] {
    let mut loads = [0.0_f64; 3];
    let filled = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    if filled < 3 { [f64::NAN; 3] } else { loads }
}

pub fn is_quiet(lock_present: bool, load_1m: f64, threshold: f64) -> bool {
    !lock_present && load_1m <= threshold
}

fn watch(pid: libc::pid_t, limit: Duration) -> (mpsc::Sender<()>, JoinHandle<()>, Arc<AtomicBool>) {
    let (stop, signal) = mpsc::channel::<()>();
    let expired = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&expired);
    let watcher = thread::spawn(move || {
        if signal.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            flag.store(true, Ordering::SeqCst);
            // negative pid targets the whole process group so test-binary grandchildren die too
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    });
    (stop, watcher, expired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    const SILENT: Matchers = Matchers { stdout: parse::never_result_line, stderr: parse::never_result_line };

    #[test]
    fn quiet_requires_no_lock_and_load_at_or_below_threshold() {
        assert!(is_quiet(false, 2.99, 3.0));
        assert!(is_quiet(false, 3.0, 3.0));
        assert!(!is_quiet(true, 0.5, 3.0));
        assert!(!is_quiet(false, 8.36, 3.0));
    }

    #[test]
    fn unreadable_load_is_not_quiet() {
        assert!(!is_quiet(false, f64::NAN, 3.0));
    }

    #[test]
    fn sleep_past_the_limit_is_killed_and_flagged_timed_out() {
        let started = Instant::now();
        let outcome = run("sleep", &["5".to_string()], Path::new("/"), &[], Duration::from_secs(1), SILENT).expect("sleep spawns");
        assert!(outcome.timed_out);
        assert!(outcome.exit_code < 0, "killed by signal, got {}", outcome.exit_code);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(outcome.wall_ns >= 1_000_000_000);
    }

    #[test]
    fn child_finishing_inside_the_limit_is_not_timed_out() {
        let outcome = run("true", &[], Path::new("/"), &[], Duration::from_secs(5), SILENT).expect("true spawns");
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, 0);
    }

    #[test]
    fn first_result_waits_for_the_result_line_not_the_process_start() {
        let matchers = Matchers { stdout: parse::cargo_test_result_line, stderr: parse::never_result_line };
        let script = "sleep 0.2; echo test x ... ok".to_string();
        let outcome = run("sh", &["-c".to_string(), script], Path::new("/"), &[], Duration::from_secs(5), matchers).expect("sh spawns");
        let first_result = outcome.first_result_ns.expect("result line was printed");
        let first_output = outcome.first_output_ns.expect("output was printed");
        assert!(first_result >= 200_000_000, "first_result_ns {first_result}");
        assert!(first_result < 1_000_000_000, "first_result_ns {first_result}");
        assert!(first_output <= first_result);
        assert!(outcome.wall_ns >= u128::from(first_result));
        assert_eq!(outcome.stdout, "test x ... ok\n");
    }

    #[test]
    fn result_line_on_the_unmatched_stream_is_not_a_first_result() {
        let matchers = Matchers { stdout: parse::cargo_test_result_line, stderr: parse::never_result_line };
        let script = "echo test x ... ok >&2".to_string();
        let outcome = run("sh", &["-c".to_string(), script], Path::new("/"), &[], Duration::from_secs(5), matchers).expect("sh spawns");
        assert!(outcome.first_output_ns.is_some());
        assert!(outcome.first_result_ns.is_none());
        assert_eq!(outcome.stderr, "test x ... ok\n");
    }

    #[test]
    fn result_line_without_trailing_newline_is_still_stamped_at_eof() {
        let matchers = Matchers { stdout: parse::cargo_test_result_line, stderr: parse::never_result_line };
        let outcome = run("printf", &["test y ... FAILED".to_string()], Path::new("/"), &[], Duration::from_secs(5), matchers).expect("printf spawns");
        assert!(outcome.first_result_ns.is_some());
        assert_eq!(outcome.stdout, "test y ... FAILED");
    }

    #[test]
    fn silent_child_has_no_first_output() {
        let outcome = run("true", &[], Path::new("/"), &[], Duration::from_secs(5), SILENT).expect("true spawns");
        assert!(outcome.first_output_ns.is_none());
    }
}
