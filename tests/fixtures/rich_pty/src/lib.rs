#[cfg(test)]
mod tests {
    use std::{io::Write, path::{Path, PathBuf}, time::{Duration, Instant}};

    const DEADLINE: Duration = Duration::from_secs(90);

    #[test]
    fn settles_at_once() {}

    // stays in flight until the harness has seen a frame with a settled test, then lets the run finish
    #[test]
    fn held_until_released() {
        let Some(release) = std::env::var_os("CARGO_VAL_FIXTURE_RELEASE_FILE") else {
            return;
        };
        let release = PathBuf::from(release);
        let started = Instant::now();
        while !release.exists() {
            assert!(started.elapsed() < DEADLINE, "the release file never appeared");
            std::thread::yield_now();
        }
        held_outcome(&release);
    }

    #[test]
    fn fails_when_requested() {
        if std::env::var_os("CARGO_VAL_FIXTURE_FAIL").is_some() {
            panic!("requested fixture failure");
        }
    }

    // a held failure unwinds with its failed marker written; otherwise the held test floods and lets the run finish
    fn held_outcome(release: &Path) {
        if std::env::var_os("CARGO_VAL_FIXTURE_HELD_FAIL").is_some() {
            let _failed = FailedOnUnwind(release.with_extension("failed"));
            panic!("held fixture failure settled while the editor was open");
        }
        flood_after_release(release);
        std::fs::File::create(release.with_extension("done")).expect("the done marker is creatable");
    }

    struct FailedOnUnwind(PathBuf);

    impl Drop for FailedOnUnwind {
        fn drop(&mut self) {
            std::fs::File::create(&self.0).expect("the failed marker is creatable");
        }
    }

    // lets the editor go only after the held failure unwound and a flood of its own was read past it, so the failure was read while the editor was open
    #[test]
    fn done_once_the_held_failure_was_read_past() {
        let Some(release) = std::env::var_os("CARGO_VAL_FIXTURE_RELEASE_FILE").map(PathBuf::from) else {
            return;
        };
        if std::env::var_os("CARGO_VAL_FIXTURE_HELD_FAIL").is_none() {
            return;
        }
        let failed = release.with_extension("failed");
        let started = Instant::now();
        while !failed.exists() {
            assert!(started.elapsed() < DEADLINE, "the held failure never unwound");
            std::thread::yield_now();
        }
        flood_after_release(&release);
        std::fs::File::create(release.with_extension("done")).expect("the done marker is creatable");
    }

    // a pipe holds far less than this, so the write finishes only if the reader keeps draining
    fn flood_after_release(_release: &Path) {
        const FLOOD_BYTES: usize = 256 * 1024;
        if std::env::var_os("CARGO_VAL_FIXTURE_FLOOD").is_none() {
            return;
        }
        let line = [[b'x'; 1023].as_slice(), b"\n"].concat();
        let mut stderr = std::io::stderr().lock();
        (0..FLOOD_BYTES / line.len()).for_each(|_| stderr.write_all(&line).expect("the flood reaches the reader"));
    }
}
