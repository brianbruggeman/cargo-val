#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

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
    }

    #[test]
    fn fails_when_requested() {
        if std::env::var_os("CARGO_VAL_FIXTURE_FAIL").is_some() {
            panic!("requested fixture failure");
        }
    }
}
