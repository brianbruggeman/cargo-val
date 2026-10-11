use proc_macro::TokenStream;

#[proc_macro]
pub fn greeting_length(_input: TokenStream) -> TokenStream {
    "32usize".parse().expect("literal is a valid token stream")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn manifest_directory() -> PathBuf {
        PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR")
                .expect("cargo exports CARGO_MANIFEST_DIR to test binaries"),
        )
    }

    #[test]
    fn manifest_directory_holds_this_package_manifest() {
        let manifest = manifest_directory().join("Cargo.toml");

        let text = std::fs::read_to_string(&manifest).expect("manifest directory holds Cargo.toml");

        assert!(
            text.contains("name = \"dylib_env\""),
            "{}",
            manifest.display()
        );
    }

    #[test]
    fn package_variables_match_the_manifest() {
        assert_eq!(std::env::var("CARGO_PKG_NAME").as_deref(), Ok("dylib_env"));
        assert_eq!(std::env::var("CARGO_PKG_VERSION").as_deref(), Ok("0.1.0"));
        assert_eq!(std::env::var("CARGO_PKG_VERSION_MAJOR").as_deref(), Ok("0"));
        assert_eq!(std::env::var("CARGO_PKG_VERSION_MINOR").as_deref(), Ok("1"));
        assert_eq!(std::env::var("CARGO_PKG_VERSION_PATCH").as_deref(), Ok("0"));
        assert_eq!(std::env::var("CARGO_PKG_VERSION_PRE").as_deref(), Ok(""));
    }

    #[test]
    fn working_directory_is_the_package_directory() {
        let relative = std::fs::read_to_string(Path::new("data/greeting.txt"))
            .expect("data/greeting.txt resolves against the package directory");
        let absolute = std::fs::read_to_string(manifest_directory().join("data/greeting.txt"))
            .expect("data/greeting.txt exists under CARGO_MANIFEST_DIR");

        assert_eq!(relative, absolute);
        assert_eq!(relative.trim_end().len(), 32);
    }

    #[test]
    fn build_script_exports_reach_the_test_process() {
        let out_directory = PathBuf::from(
            std::env::var("OUT_DIR").expect("cargo exports OUT_DIR to test binaries"),
        );

        assert!(out_directory.is_dir(), "{}", out_directory.display());
        assert_eq!(
            std::env::var("DYLIB_ENV_FROM_BUILD_SCRIPT").as_deref(),
            Ok("exported by build.rs")
        );
    }
}
