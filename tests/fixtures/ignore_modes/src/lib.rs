#[cfg(test)]
mod fixture {
    #[test]
    fn passes() {}

    #[test]
    fn fails() {
        panic!("fixture failure");
    }

    #[test]
    #[ignore = "depends on a host-local checkout outside this repo"]
    fn ignored_with_reason() {}
}
