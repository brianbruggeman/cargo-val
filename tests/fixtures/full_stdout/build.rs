use std::fmt::Write as _;
use std::path::PathBuf;

const CASE_COUNT_VARIABLE: &str = "FULL_STDOUT_CASE_COUNT";

fn main() {
    println!("cargo:rerun-if-env-changed={CASE_COUNT_VARIABLE}");
    let out_directory = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let case_count: usize = std::env::var(CASE_COUNT_VARIABLE)
        .expect("the driving test sets the case count from the status queue capacity")
        .parse()
        .expect("the case count is a number");
    let mut source = String::new();
    for index in 0..case_count {
        writeln!(
            source,
            "#[test]\nfn stdout_pressure_case_{index:04}() {{ record(\"stdout_pressure_case_{index:04}\"); }}"
        )
        .expect("writing to a string cannot fail");
    }
    std::fs::write(out_directory.join("cases.rs"), source).expect("generated cases are writable");
}
