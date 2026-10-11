use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

pub struct Binary {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|directory| directory.join(name)).find(|candidate| candidate.is_file())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

pub fn locate(name: &str) -> Result<Option<Binary>, String> {
    let Some(path) = find_on_path(name) else {
        return Ok(None);
    };
    let bytes = fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(Some(Binary { path, sha256: sha256_hex(&bytes), size: u64::try_from(bytes.len()).unwrap_or(u64::MAX) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_the_fips_180_abc_vector() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
