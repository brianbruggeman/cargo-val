#[cfg(test)]
mod suite {
    const ABORT_VARIABLE: &str = "CARGO_VAL_QUIRK_ABORT";

    fn aborts_here(name: &str) -> bool {
        std::env::var(ABORT_VARIABLE).is_ok_and(|requested| requested == name)
    }

    #[test]
    #[should_panic]
    fn panics_plain() {
        panic!("plain panic that should_panic expects");
    }

    #[test]
    #[should_panic(expected = "the expected message")]
    fn panics_with_expected_message() {
        panic!("the expected message");
    }

    #[test]
    #[should_panic]
    fn panics_then_aborts() {
        if aborts_here("suite::panics_then_aborts") {
            std::process::abort();
        }
        panic!("should panic before the abort is requested");
    }

    #[test]
    fn child_process_writes_to_stdout() {
        let status = std::process::Command::new("sh")
            .args(["-c", "echo raw-line-from-child"])
            .status()
            .expect("sh runs");
        assert!(status.success());
    }

    #[test]
    fn passes_after_the_stray_output() {}
}
