use crate::binary::sha256_hex;
use std::fs;
use std::path::{Path, PathBuf};

const LOOP_LITERAL: &str = "1_000_000_u64";

pub fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut groups: Vec<&str> = Vec::new();
    let mut end = digits.len();
    while end > 0 {
        let start = end.saturating_sub(3);
        groups.push(&digits[start..end]);
        end = start;
    }
    groups.reverse();
    groups.join("_")
}

pub fn target_slug(dir: &Path) -> String {
    let name = dir.file_name().map_or_else(|| "fixture".to_string(), |name| name.to_string_lossy().into_owned());
    let digest = sha256_hex(dir.display().to_string().as_bytes());
    format!("{name}-{}", &digest[..12])
}

pub fn scale_source(source: &str, factor: u64) -> (String, Vec<usize>) {
    let replacement = format!("{}_u64", grouped(1_000_000 * factor));
    let mut hits = Vec::new();
    let mut rewritten = String::with_capacity(source.len());
    for (index, line) in source.split_inclusive('\n').enumerate() {
        if line.contains(LOOP_LITERAL) {
            hits.push(index + 1);
        }
        rewritten.push_str(&line.replace(LOOP_LITERAL, &replacement));
    }
    (rewritten, hits)
}

fn copy_tree(from: &Path, to: &Path, factor: u64, rewrites: &mut Vec<String>) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|error| format!("mkdir {}: {error}", to.display()))?;
    let entries = fs::read_dir(from).map_err(|error| format!("read_dir {}: {error}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read_dir entry in {}: {error}", from.display()))?;
        let name = entry.file_name();
        let source = entry.path();
        let target = to.join(&name);
        if source.is_dir() {
            if name != "target" {
                copy_tree(&source, &target, factor, rewrites)?;
            }
        } else {
            copy_file(&source, &target, factor, rewrites)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Path, target: &Path, factor: u64, rewrites: &mut Vec<String>) -> Result<(), String> {
    let is_rust = source.extension().is_some_and(|extension| extension == "rs");
    if !is_rust {
        fs::copy(source, target).map_err(|error| format!("copy {}: {error}", source.display()))?;
        return Ok(());
    }
    let text = fs::read_to_string(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    let (scaled, hits) = scale_source(&text, factor);
    rewrites.extend(hits.iter().map(|line| format!("{}:{line}", target.display())));
    fs::write(target, scaled).map_err(|error| format!("write {}: {error}", target.display()))
}

fn detach_from_parent_workspace(manifest: &Path) -> Result<(), String> {
    let text = fs::read_to_string(manifest).map_err(|error| format!("read {}: {error}", manifest.display()))?;
    if text.contains("[workspace]") {
        return Ok(());
    }
    fs::write(manifest, format!("{text}\n[workspace]\n")).map_err(|error| format!("write {}: {error}", manifest.display()))
}

pub fn materialize(fixture: &Path, factor: u64, base: &Path) -> Result<(PathBuf, Vec<String>), String> {
    let name = fixture.file_name().ok_or_else(|| format!("fixture {} has no directory name", fixture.display()))?;
    let destination = base.join("fixtures").join(format!("scaled-{factor}")).join(name);
    if destination.exists() {
        fs::remove_dir_all(&destination).map_err(|error| format!("clear {}: {error}", destination.display()))?;
    }
    let mut rewrites = Vec::new();
    copy_tree(fixture, &destination, factor, &mut rewrites)?;
    detach_from_parent_workspace(&destination.join("Cargo.toml"))?;
    Ok((destination, rewrites))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_inserts_underscores_every_three_digits() {
        assert_eq!(grouped(10_000_000), "10_000_000");
        assert_eq!(grouped(1_000_000), "1_000_000");
        assert_eq!(grouped(500), "500");
    }

    #[test]
    fn target_slug_differs_for_same_named_fixtures_at_different_paths() {
        let unscaled = target_slug(Path::new("/repo/tests/fixtures/workspace"));
        let scaled = target_slug(Path::new("/repo/tools/val-bench/fixtures/scaled-10/workspace"));
        assert!(unscaled.starts_with("workspace-") && scaled.starts_with("workspace-"));
        assert_ne!(unscaled, scaled);
    }

    #[test]
    fn target_slug_is_stable_for_one_path() {
        let dir = Path::new("/repo/tests/fixtures/workspace");
        assert_eq!(target_slug(dir), target_slug(dir));
    }

    #[test]
    fn scale_source_rewrites_the_loop_bound_from_arithmetic_fixture() {
        let source = "fn checksum(seed: u64) -> u64 {\n    (0..1_000_000_u64).fold(seed, |state, value| {\n";
        let (scaled, hits) = scale_source(source, 10);
        assert_eq!(hits, vec![2]);
        assert!(scaled.contains("(0..10_000_000_u64).fold"));
        assert!(!scaled.contains("1_000_000_u64"));
    }
}
