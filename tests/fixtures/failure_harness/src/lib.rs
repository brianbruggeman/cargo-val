#[cfg(test)]
mod failing {
    #[proxima::test]
    async fn body_never_completes() {
        std::future::pending::<()>().await;
    }

    #[proxima::test]
    async fn body_panics_after_await() {
        let retries: Vec<u32> = Vec::new();
        assert!(!retries.is_empty(), "no retries were scheduled");
    }
}
