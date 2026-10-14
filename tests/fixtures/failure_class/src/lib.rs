#[cfg(test)]
mod failing {
    fn noise() {
        let lines: usize = std::env::var("CLASSIFY_NOISE_LINES").ok().and_then(|value| value.parse().ok()).unwrap_or(0);
        for index in 0..lines {
            println!("worker {index:04} flushed segment {index:04} to disk");
        }
    }

    #[test]
    fn assert_eq_mismatch() {
        noise();
        let expected = vec![1, 2, 3];
        let actual = vec![1, 2, 4];
        assert_eq!(expected, actual, "segment checksum differs");
    }

    #[test]
    fn plain_panic() {
        noise();
        let queue: Vec<u32> = Vec::new();
        let head = queue.first().copied();
        panic!("queue drained before head arrived: {head:?}");
    }

    #[test]
    fn registry_panic_under_user_call() {
        noise();
        let mut payload = bytes::Bytes::from_static(b"frame");
        let _ = payload.split_to(64);
    }

    #[test]
    fn segfault_crash() {
        noise();
        unsafe { core::ptr::null_mut::<u8>().write_volatile(1) };
    }

    #[test]
    fn abort_crash() {
        noise();
        std::process::abort();
    }
}
