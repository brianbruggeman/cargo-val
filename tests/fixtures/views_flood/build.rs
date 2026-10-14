use std::fmt::Write as _;
use std::path::PathBuf;

const SETTLERS: usize = 2000;
const BULK: usize = 40;

fn main() {
    let mut source = String::new();
    for index in 0..SETTLERS {
        let _ = writeln!(source, "#[test]\nfn settle_{index:04}() {{\n    settle();\n}}");
    }
    for index in 0..BULK {
        let _ = writeln!(source, "#[test]\nfn bulk_{index:02}() {{\n    fail_with_output(\"VIEWS_FLOOD_BULK_BYTES\", {index});\n}}");
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for build scripts"));
    std::fs::write(out.join("tests.rs"), source).expect("OUT_DIR is writable");
    println!("cargo:rerun-if-changed=build.rs");
}
