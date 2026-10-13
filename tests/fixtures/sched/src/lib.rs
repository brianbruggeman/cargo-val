#[cfg(test)]
mod declared {
    use std::io::Write;

    fn record_process() {
        let Some(path) = std::env::var_os("CARGO_VAL_SHARD_LOG") else {
            return;
        };
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("shard log opens for append");
        log.write_all(format!("{}\n", std::process::id()).as_bytes())
            .expect("shard log takes the pid");
    }

    #[proxima::test(runtime = "tokio", serial)]
    async fn loads_whole_checkpoint() {
        record_process();
        assert_eq!(2 + 2, 4);
    }

    #[proxima::test(runtime = "tokio", group = "real-checkpoint")]
    #[case::small(1u32)]
    #[case::large(4_000_000_000u32)]
    async fn decodes_each_size(#[case] value: u32) {
        record_process();
        assert!(value > 0);
    }

    #[proxima::test(runtime = "tokio", process)]
    async fn shares_the_binary_process() {
        record_process();
        assert_eq!(1 + 2, 3);
    }

    #[proxima::test(runtime = "tokio", group = "gpu", process)]
    async fn shares_the_gpu_group_process() {
        record_process();
        assert_eq!(3 * 3, 9);
    }

    #[proxima::test(runtime = "tokio", group = "gpu", process)]
    async fn also_shares_the_gpu_group_process() {
        record_process();
        assert_eq!(4 * 4, 16);
    }

    #[proxima::test(runtime = "tokio", serial, process)]
    async fn runs_alone_in_the_shared_process() {
        record_process();
        assert_eq!(4 - 1, 3);
    }

    #[proxima::test(runtime = "tokio")]
    async fn declares_nothing() {
        record_process();
        assert_eq!(u8::MAX, 255);
    }
}
