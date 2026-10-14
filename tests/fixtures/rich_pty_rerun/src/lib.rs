#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process;
    use std::thread;
    use std::time::{Duration, Instant};

    const RUNS_LOG: &str = "CARGO_VAL_FIXTURE_RUNS_LOG";
    const ONCE_STATE: &str = "CARGO_VAL_FIXTURE_ONCE_STATE";
    const HANG_STATE: &str = "CARGO_VAL_FIXTURE_HANG_STATE";
    const HANG_PID: &str = "CARGO_VAL_FIXTURE_HANG_PID";
    const ONLY_ONCE: &str = "CARGO_VAL_FIXTURE_ONLY_ONCE";
    const EXE_FILE: &str = "CARGO_VAL_FIXTURE_EXE_FILE";
    const CHANGING_STATE: &str = "CARGO_VAL_FIXTURE_CHANGING_STATE";
    const ONLY_CHANGING: &str = "CARGO_VAL_FIXTURE_ONLY_CHANGING";
    const NONATOMIC: &str = "CARGO_VAL_FIXTURE_NONATOMIC";
    const WRITE_GAP_MS: &str = "CARGO_VAL_FIXTURE_WRITE_GAP_MS";

    // control knobs: NONATOMIC restores create-then-write, WRITE_GAP_MS widens the window between the two; busy-waits because the gap must not depend on a sleep
    fn write_gap() {
        let Some(millis) = env::var(WRITE_GAP_MS).ok().and_then(|text| text.parse::<u64>().ok()) else {
            return;
        };
        let gap = Duration::from_millis(millis);
        let started = Instant::now();
        while started.elapsed() < gap {
            thread::yield_now();
        }
    }

    // a reader that sees the path must see all of its bytes, so the content lands under a temp name and is renamed into place
    fn write_atomically(path: impl AsRef<Path>, bytes: &[u8]) {
        let path = path.as_ref();
        if env::var_os(NONATOMIC).is_some() {
            let mut file = fs::File::create(path).expect("the file is creatable");
            write_gap();
            file.write_all(bytes).expect("the file is writable");
            return;
        }
        let mut staged: PathBuf = path.to_path_buf();
        staged.set_extension(format!("tmp{}", process::id()));
        let mut file = fs::File::create(&staged).expect("the staging file is creatable");
        write_gap();
        file.write_all(bytes).expect("the staging file is writable");
        file.sync_all().expect("the staging file reaches disk before the rename");
        fs::rename(&staged, path).expect("the staged file renames into place");
    }

    fn only_once_test_fails() -> bool {
        env::var_os(ONLY_ONCE).is_some()
    }

    fn only_changing_test_fails() -> bool {
        env::var_os(ONLY_CHANGING).is_some()
    }

    fn note_exe() {
        if let Some(file) = env::var_os(EXE_FILE) {
            let exe = env::current_exe().expect("the test binary knows its own path");
            write_atomically(file, exe.to_string_lossy().as_bytes());
        }
    }

    // one write syscall per record so concurrent runs cannot interleave a name with another run's newline
    fn append_record(file: &mut fs::File, name: &str) {
        file.write_all(format!("{name}\n").as_bytes()).expect("the runs log takes one line per run");
    }

    fn note_run(name: &str) {
        note_exe();
        let Some(log) = env::var_os(RUNS_LOG) else {
            return;
        };
        let mut file = OpenOptions::new().create(true).append(true).open(log).expect("the runs log opens for append");
        append_record(&mut file, name);
    }

    fn first_run(state: &str) -> bool {
        let Some(marker) = env::var_os(state) else {
            return true;
        };
        let marker = Path::new(&marker);
        let first = !marker.exists();
        if first {
            write_atomically(marker, b"failed once");
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
            write_atomically(pid_file, process::id().to_string().as_bytes());
        }
        loop {
            thread::park();
        }
    }
}
