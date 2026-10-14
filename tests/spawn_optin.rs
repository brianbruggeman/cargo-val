#[cfg(unix)]
#[path = "common/binary.rs"]
mod binary;

use std::fs;
use std::path::{Path, PathBuf};

const BINARY_KEY: &str = "CARGO_BIN_EXE_cargo-val";
const CONSTRUCTOR: &str = "common/binary.rs";
const OPTED_IN_EXAMPLES: [&str; 2] = ["parity.rs", "scale.rs"];

fn sources(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).expect("the directory is readable") {
        let path = entry.expect("the entry is readable").path();
        if path.is_dir() && path.file_name().is_some_and(|name| name != "fixtures") {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found
}

fn manifest_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[test]
fn only_the_opt_in_constructor_resolves_the_cargo_val_binary_path() {
    let tests = manifest_path("tests");
    let scanned = sources(&tests);

    let raw: Vec<_> = scanned
        .iter()
        .filter(|path| !path.ends_with(CONSTRUCTOR) && !path.ends_with("spawn_optin.rs"))
        .filter(|path| fs::read_to_string(path).expect("the source is readable").contains(BINARY_KEY))
        .collect();

    assert!(scanned.len() > 10, "the scan saw {} test sources", scanned.len());
    assert!(raw.is_empty(), "these tests resolve the cargo-val binary without common/binary.rs: {raw:?}");
}

#[test]
fn examples_that_spawn_cargo_val_name_the_exit_with_parent_key() {
    for name in OPTED_IN_EXAMPLES {
        let source = fs::read_to_string(manifest_path("examples").join(name)).expect("the example is readable");
        assert!(source.contains("CARGO_VAL_EXIT_WITH_PARENT"), "examples/{name} spawns cargo-val without the opt-in");
    }
}

#[cfg(unix)]
#[test]
fn the_constructor_command_carries_the_exit_with_parent_opt_in() {
    let command = binary::command();

    let value = command.get_envs().find(|(key, _)| *key == binary::EXIT_WITH_PARENT).and_then(|(_, value)| value);

    assert_eq!(value, Some(std::ffi::OsStr::new("1")));
}
