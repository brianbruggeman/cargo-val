use std::fmt::Write as _;
use std::path::PathBuf;

const CASE_COUNT: usize = 3000;

fn main() {
    let out_directory = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let mut source = String::new();
    for index in 0..CASE_COUNT {
        writeln!(
            source,
            "#[test]\nfn stdout_pressure_case_{index:04}() {{ record(\"stdout_pressure_case_{index:04}\"); }}"
        )
        .expect("writing to a string cannot fail");
    }
    std::fs::write(out_directory.join("cases.rs"), source).expect("generated cases are writable");
}
