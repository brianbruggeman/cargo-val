use std::fmt::Write as _;
use std::path::PathBuf;

const TESTS: usize = 4096;

fn main() {
    let mut source = String::new();
    for index in 0..TESTS {
        let _ = writeln!(source, "#[test]\nfn t{index:04}() {{}}");
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for build scripts"));
    std::fs::write(out.join("tests.rs"), source).expect("OUT_DIR is writable");
    println!("cargo:rerun-if-changed=build.rs");
}
