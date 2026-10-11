use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::error::SuiteError;
use crate::plan::{Plan, PlannedTest};

const LIB_SOURCE: &str = include_str!("generated_lib.rs");

#[must_use]
pub fn crate_name(index: u32) -> String {
    format!("suite_c{index:02}")
}

#[must_use]
pub fn binary_name(index: u32) -> String {
    format!("bin_{index:02}")
}

#[must_use]
pub fn test_name(index: u32) -> String {
    format!("case_{index:03}")
}

fn write(path: &Path, contents: &str) -> Result<(), SuiteError> {
    let io_error = |source| SuiteError::Io { path: path.to_path_buf(), source };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    fs::write(path, contents).map_err(io_error)
}

fn workspace_toml(config: &Config) -> String {
    let members: Vec<String> = (0..config.crates).map(|index| format!("    \"crates/{}\",", crate_name(index))).collect();
    format!(
        "[workspace]\nresolver = \"3\"\nmembers = [\n{}\n]\n\n[profile.dev]\nopt-level = 3\n\n[profile.test]\nopt-level = 3\n",
        members.join("\n")
    )
}

fn crate_toml(index: u32) -> String {
    format!("[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n", crate_name(index))
}

fn test_fn(test: &PlannedTest) -> String {
    format!(
        "#[test]\nfn {name}() {{\n    let digest = mix(black_box({seed}), black_box({iterations}));\n    \
         assert_ne!(digest, 0);\n    assert_eq!(clamp_add(black_box(250), 10, 255), 255);\n    \
         assert_ne!(checksum(&[digest, {seed}]), 0);\n}}\n",
        name = test_name(test.test_index),
        seed = test.mix_seed,
        iterations = test.iterations
    )
}

fn binary_source(crate_index: u32, tests: &[&PlannedTest]) -> String {
    let body: Vec<String> = tests.iter().map(|test| test_fn(test)).collect();
    format!(
        "use std::hint::black_box;\n\nuse {}::{{checksum, clamp_add, mix}};\n\n{}",
        crate_name(crate_index),
        body.join("\n")
    )
}

/// # Errors
/// `Io` when a generated file or directory cannot be written.
pub fn emit_workspace(config: &Config, plan: &Plan) -> Result<(), SuiteError> {
    write(&config.out.join("Cargo.toml"), &workspace_toml(config))?;
    write(&config.out.join(".gitignore"), "target/\n")?;
    for crate_index in 0..config.crates {
        let root = config.out.join("crates").join(crate_name(crate_index));
        write(&root.join("Cargo.toml"), &crate_toml(crate_index))?;
        write(&root.join("src/lib.rs"), LIB_SOURCE)?;
        for binary_index in 0..config.binaries_per_crate {
            let tests: Vec<&PlannedTest> = plan
                .tests
                .iter()
                .filter(|test| test.crate_index == crate_index && test.binary_index == binary_index)
                .collect();
            let path = root.join("tests").join(format!("{}.rs", binary_name(binary_index)));
            write(&path, &binary_source(crate_index, &tests))?;
        }
    }
    Ok(())
}

/// # Errors
/// `Io` when the manifest cannot be written.
pub fn write_manifest(config: &Config, contents: &str) -> Result<(), SuiteError> {
    write(&config.out.join("suite-manifest.json"), contents)
}
