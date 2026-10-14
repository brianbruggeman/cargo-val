#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;
    use std::process;
    use std::thread;

    const RUNS_LOG: &str = "CARGO_VAL_FIXTURE_RUNS_LOG";
    const ONCE_STATE: &str = "CARGO_VAL_FIXTURE_ONCE_STATE";
    const HANG_STATE: &str = "CARGO_VAL_FIXTURE_HANG_STATE";
    const HANG_PID: &str = "CARGO_VAL_FIXTURE_HANG_PID";
    const ONLY_ONCE: &str = "CARGO_VAL_FIXTURE_ONLY_ONCE";
    const EXE_FILE: &str = "CARGO_VAL_FIXTURE_EXE_FILE";
    const CHANGING_STATE: &str = "CARGO_VAL_FIXTURE_CHANGING_STATE";
    const ONLY_CHANGING: &str = "CARGO_VAL_FIXTURE_ONLY_CHANGING";

    fn only_once_test_fails() -> bool {
        env::var_os(ONLY_ONCE).is_some()
    }

    fn only_changing_test_fails() -> bool {
        env::var_os(ONLY_CHANGING).is_some()
    }

    fn note_exe() {
        if let Some(file) = env::var_os(EXE_FILE) {
            let exe = env::current_exe().expect("the test binary knows its own path");
            fs::write(file, exe.to_string_lossy().as_bytes()).expect("the exe path file is writable");
        }
    }

    fn note_run(name: &str) {
        note_exe();
        let Some(log) = env::var_os(RUNS_LOG) else {
            return;
        };
        let mut file = OpenOptions::new().create(true).append(true).open(log).expect("the runs log opens for append");
        writeln!(file, "{name}").expect("the runs log takes one line per run");
    }

    fn first_run(state: &str) -> bool {
        let Some(marker) = env::var_os(state) else {
            return true;
        };
        let marker = Path::new(&marker);
        let first = !marker.exists();
        if first {
            fs::write(marker, b"failed once").expect("the state marker is writable");
        }
        first
    }

    #[test]
    fn fails_always_one() {
        note_run("fails_always_one");
        if only_once_test_fails() {
            return;
        }
        if env::var_os(CHANGING_STATE).is_some() {
            let attempt = if first_run(CHANGING_STATE) { "first attempt: expected three listeners" } else { "second attempt: expected five workers" };
            panic!("{attempt}");
        }
        let listeners = [8080, 8443];
        assert_eq!(listeners.len(), 3, "expected three listeners, found {listeners:?}");
    }

    #[test]
    fn fails_always_two() {
        note_run("fails_always_two");
        if only_once_test_fails() || only_changing_test_fails() {
            return;
        }
        let workers = [1, 2, 3];
        assert_eq!(workers.len(), 4, "expected four workers, found {workers:?}");
    }

    #[test]
    fn fails_once_then_passes() {
        note_run("fails_once_then_passes");
        if only_changing_test_fails() {
            return;
        }
        assert!(!first_run(ONCE_STATE), "the first run of this test fails on purpose");
    }

    #[test]
    fn fails_once_then_blocks() {
        note_run("fails_once_then_blocks");
        if only_once_test_fails() || only_changing_test_fails() {
            return;
        }
        assert!(!first_run(HANG_STATE), "the first run of this test fails on purpose");
        if let Some(pid_file) = env::var_os(HANG_PID) {
            fs::write(pid_file, process::id().to_string()).expect("the pid file is writable");
        }
        loop {
            thread::park();
        }
    }
}
