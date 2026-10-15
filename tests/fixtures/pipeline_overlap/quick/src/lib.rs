use std::fs::File;

/// Doubles a request count.
///
/// ```
/// assert_eq!(pipeline_quick::doubled(21), 42);
/// ```
pub fn doubled(count: u32) -> u32 {
    count * 2
}

/// Parks the calling test until the harness opens the fifo named by `CARGO_VAL_FIXTURE_TEST_FIFO`; with none named it returns at once.
pub fn held_until_released() {
    if let Some(fifo) = std::env::var_os("CARGO_VAL_FIXTURE_TEST_FIFO") {
        File::open(fifo).expect("the harness-made fifo opens once its writer arrives");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubling_after_release_is_exact() {
        super::held_until_released();
        assert_eq!(super::doubled(4), 8);
    }
}
