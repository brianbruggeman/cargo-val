#[cfg(test)]
mod overlap {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const HOLD_OPEN: Duration = Duration::from_millis(400);

    fn nanoseconds() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is past the epoch").as_nanos()
    }

    fn log_path() -> Option<std::path::PathBuf> {
        std::env::var_os("CARGO_VAL_SHARD_LOG").map(Into::into)
    }

    fn append(kind: &str, name: &str) {
        let Some(path) = log_path() else {
            return;
        };
        let mut log = OpenOptions::new().create(true).append(true).open(path).expect("interval log opens for append");
        log.write_all(format!("{kind} {name} {}\n", nanoseconds()).as_bytes())
            .expect("interval log takes one record");
    }

    fn starts() -> usize {
        log_path()
            .and_then(|path| fs::read_to_string(path).ok())
            .map_or(0, |text| text.lines().filter(|line| line.starts_with("start ")).count())
    }

    // stays open until some other test starts, so a scheduler that allowed an overlap would record one
    fn interval(name: &str) {
        append("start", name);
        let started = starts();
        let deadline = Instant::now() + HOLD_OPEN;
        while starts() <= started && Instant::now() < deadline {
            std::hint::spin_loop();
        }
        append("end", name);
    }

    #[proxima::test(runtime = "tokio", serial)]
    async fn loads_whole_checkpoint() {
        interval("loads_whole_checkpoint");
    }

    #[proxima::test(runtime = "tokio", group = "real-checkpoint")]
    #[case::small(1u32)]
    #[case::large(4_000_000_000u32)]
    #[case::medium(70_000u32)]
    async fn decodes_each_size(#[case] value: u32) {
        interval(&format!("decodes_each_size_{value}"));
    }

    #[proxima::test(runtime = "tokio")]
    async fn declares_nothing() {
        interval("declares_nothing");
    }

    #[proxima::test(runtime = "tokio")]
    async fn declares_nothing_either() {
        interval("declares_nothing_either");
    }

    #[proxima::test(runtime = "tokio")]
    async fn declares_nothing_again() {
        interval("declares_nothing_again");
    }
}
