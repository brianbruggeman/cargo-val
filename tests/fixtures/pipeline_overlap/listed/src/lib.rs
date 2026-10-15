use std::fs::File;

/// The libtest protocol a `harness = false` binary must speak for cargo-val: a terse listing, then one result block per exact run.
/// The non-ignored listing parks on the fifo named by `CARGO_VAL_FIXTURE_LIST_FIFO` until the harness opens it, so a listing can be held in flight.
pub fn serve(binary: &str) {
    let arguments: Vec<String> = std::env::args().collect();
    let listing = arguments.iter().any(|argument| argument == "--list");
    if listing && arguments.iter().any(|argument| argument == "--ignored") {
        return;
    }
    if listing {
        if let Some(fifo) = std::env::var_os("CARGO_VAL_FIXTURE_LIST_FIFO") {
            File::open(fifo).expect("the harness-made fifo opens once its writer arrives");
        }
        println!("{binary}::lists_once: test\n\n1 test, 0 benchmarks");
        return;
    }
    println!("running 1 test\ntest {binary}::lists_once ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s");
}
