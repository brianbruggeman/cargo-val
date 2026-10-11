fn main() {
    println!("cargo:rustc-env=DYLIB_ENV_FROM_BUILD_SCRIPT=exported by build.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
