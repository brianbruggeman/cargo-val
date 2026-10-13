#[cfg(test)]
mod suite {
    use std::io::Write;

    // raw writes bypass libtest's capture, so they land on the stream exactly as a stray child would print them
    #[test]
    fn stray_ok_then_passes() {
        let _ = std::io::stdout().write_all(b"all ok\n");
    }

    #[test]
    fn stray_ok_then_fails() {
        let _ = std::io::stdout().write_all(b"all ok\n");
        panic!("the real verdict is a failure");
    }

    #[test]
    fn nested_cargo_output_passes() {
        let _ = std::io::stdout().write_all(
            b"running 1 test\ntest inner ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n",
        );
    }

    #[test]
    fn plain_passes() {}
}
