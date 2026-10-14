#[cfg(test)]
mod tests {
    use std::{io::Write, path::PathBuf, time::{Duration, Instant}};

    const TESTS: usize = 29;
    const STORM_LINES: usize = 100;
    const DEADLINE: Duration = Duration::from_secs(90);

    fn release_file() -> Option<PathBuf> {
        std::env::var_os("CARGO_VAL_FIXTURE_RELEASE_FILE").map(PathBuf::from)
    }

    // the last test to finish writes the done marker the editor waits on, so the editor outlives the whole run
    fn finished(name: &str) {
        let Some(release) = release_file() else {
            return;
        };
        std::fs::File::create(release.with_file_name(format!("finished-{name}"))).expect("the finished marker is creatable");
        let directory = release.parent().expect("the release file has a directory");
        let count = std::fs::read_dir(directory).expect("the release directory is readable").flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with("finished-")).count();
        if count == TESTS {
            std::fs::File::create(release.with_extension("done")).expect("the done marker is creatable");
        }
    }

    fn storm(name: &str) {
        let mut stderr = std::io::stderr().lock();
        (0..STORM_LINES).for_each(|line| writeln!(stderr, "{name} diagnostic line {line:03}: connection to 10.0.0.{} reset by peer after 3 retries", line % 250).expect("the storm reaches the reader"));
        drop(stderr);
        finished(name);
        panic!("{name} failed after its diagnostics");
    }

    macro_rules! settles_at_once {
        ($($name:ident),+ $(,)?) => {
            $(
                #[test]
                fn $name() {
                    finished(stringify!($name));
                }
            )+
        };
    }

    macro_rules! storms {
        ($($name:ident),+ $(,)?) => {
            $(
                #[test]
                fn $name() {
                    storm(stringify!($name));
                }
            )+
        };
    }

    settles_at_once!(settles_at_once_a, settles_at_once_b, settles_at_once_c, settles_at_once_d);

    // stays in flight until the harness has seen the editor start, then settles
    #[test]
    fn held_until_released() {
        if let Some(release) = release_file() {
            let started = Instant::now();
            while !release.exists() {
                assert!(started.elapsed() < DEADLINE, "the release file never appeared");
                std::thread::yield_now();
            }
        }
        finished("held_until_released");
    }

    storms!(
        storm_01, storm_02, storm_03, storm_04, storm_05, storm_06, storm_07, storm_08,
        storm_09, storm_10, storm_11, storm_12, storm_13, storm_14, storm_15, storm_16,
        storm_17, storm_18, storm_19, storm_20, storm_21, storm_22, storm_23, storm_24,
    );
}
