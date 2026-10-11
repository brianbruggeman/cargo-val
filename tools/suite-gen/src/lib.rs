pub mod burn;
pub mod config;
pub mod emit;
pub mod error;
pub mod generated_lib;
pub mod manifest;
pub mod plan;
pub mod rng;

use burn::Calibration;
use config::Config;
use error::SuiteError;
use plan::Plan;

/// # Errors
/// Any planning or emit error.
pub fn generate(config: &Config, calibration: &Calibration) -> Result<Plan, SuiteError> {
    let plan = plan::build_plan(config, calibration.iters_per_ms)?;
    emit::emit_workspace(config, &plan)?;
    emit::write_manifest(config, &manifest::render(config, calibration, &plan))?;
    Ok(plan)
}

/// # Errors
/// Any planning or emit error.
pub fn run(config: &Config) -> Result<(Plan, Calibration), SuiteError> {
    let calibration = config.iters_per_ms.map_or_else(burn::calibrate, Calibration::fixed);
    let plan = generate(config, &calibration)?;
    Ok((plan, calibration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("suite-gen-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn snapshot(root: &Path, dir: &Path, into: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("read generated dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                snapshot(root, &path, into);
            } else {
                let relative = path.strip_prefix(root).expect("under root").display().to_string();
                into.insert(relative, fs::read(&path).expect("read generated file"));
            }
        }
    }

    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        snapshot(root, root, &mut files);
        files
    }

    fn small(out: PathBuf, seed: u64) -> Config {
        let mut config = Config::with_defaults(out);
        config.seed = seed;
        config.slow_fraction = 0.05;
        config.slow_ms = 50.0;
        config.target_serial_secs = Some(5.0);
        config
    }

    #[test]
    fn same_args_yield_byte_identical_trees() {
        let (left, right) = (scratch("det-left"), scratch("det-right"));
        let calibration = Calibration::fixed(500_000.0);
        generate(&small(left.clone(), 11), &calibration).expect("generate left");
        generate(&small(right.clone(), 11), &calibration).expect("generate right");
        let (left_tree, right_tree) = (tree(&left), tree(&right));
        assert!(left_tree.len() > 10, "files {}", left_tree.len());
        assert_eq!(left_tree, right_tree);
        let _ = fs::remove_dir_all(&left);
        let _ = fs::remove_dir_all(&right);
    }

    #[test]
    fn different_seed_changes_the_tree() {
        let (left, right) = (scratch("seed-left"), scratch("seed-right"));
        let calibration = Calibration::fixed(500_000.0);
        generate(&small(left.clone(), 1), &calibration).expect("generate left");
        generate(&small(right.clone(), 2), &calibration).expect("generate right");
        assert_ne!(tree(&left), tree(&right));
        let _ = fs::remove_dir_all(&left);
        let _ = fs::remove_dir_all(&right);
    }

    #[test]
    fn emits_expected_test_functions_and_manifest() {
        let out = scratch("shape");
        let calibration = Calibration::fixed(500_000.0);
        generate(&small(out.clone(), 3), &calibration).expect("generate");
        let source = fs::read_to_string(out.join("crates/suite_c00/tests/bin_00.rs")).expect("bin_00");
        assert_eq!(source.matches("#[test]").count(), 10);
        let manifest = fs::read_to_string(out.join("suite-manifest.json")).expect("manifest");
        assert!(manifest.contains("\"tests\": 120"));
        let _ = fs::remove_dir_all(&out);
    }
}
