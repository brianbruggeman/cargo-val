use std::fs::File;

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_VAL_FIXTURE_BUILD_FIFO");
    if let Some(fifo) = std::env::var_os("CARGO_VAL_FIXTURE_BUILD_FIFO") {
        File::open(fifo).expect("the harness-made fifo opens once its writer arrives");
    }
}
