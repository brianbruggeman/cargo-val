//! The last-run record: 14 bytes under the workspace's target directory, read once at the start of a run and replaced once at its end.
//!
//! Layout: `version u8 (1) | wall_ms u64 LE | tests u32 LE | exit u8`. Reading and writing both happen at the run's
//! edges, never inside the event loop; the record reaches the render state as data through `Event::Previous`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const VERSION: u8 = 1;
const RECORD_BYTES: usize = 14;
const DIRECTORY: &str = "cargo-val";
const FILE: &str = "last-run";

/// What a finished run leaves behind: its wall time, the tests it discovered, and its exit code.
pub type Record = (u64, u32, u8);

#[must_use]
pub fn encode((wall_ms, tests, exit): Record) -> [u8; RECORD_BYTES] {
    let mut bytes = [0_u8; RECORD_BYTES];
    bytes[0] = VERSION;
    bytes[1..9].copy_from_slice(&wall_ms.to_le_bytes());
    bytes[9..13].copy_from_slice(&tests.to_le_bytes());
    bytes[13] = exit;
    bytes
}

/// # Errors
///
/// The reason the bytes are not a record: a wrong length or a wrong version.
pub fn decode(bytes: &[u8]) -> Result<Record, &'static str> {
    let Ok::<[u8; RECORD_BYTES], _>(record) = bytes.try_into() else {
        return Err("wrong length");
    };
    if record[0] != VERSION {
        return Err("wrong version");
    }
    let wall = <[u8; 8]>::try_from(&record[1..9]).map_err(|_| "wrong length")?;
    let tests = <[u8; 4]>::try_from(&record[9..13]).map_err(|_| "wrong length")?;
    Ok((u64::from_le_bytes(wall), u32::from_le_bytes(tests), record[13]))
}

/// The record's place: `<target>/cargo-val/last-run`.
#[must_use]
pub fn path(target: &Path) -> PathBuf {
    target.join(DIRECTORY).join(FILE)
}

/// The target directory to keep the record in, from `[explicit, environment, metadata]` in that order: an explicit `--target-dir` root first, then `CARGO_TARGET_DIR`, then the directory `cargo metadata` reports.
#[must_use]
pub fn target_directory(sources: [Option<&[u8]>; 3]) -> Option<PathBuf> {
    sources
        .into_iter()
        .flatten()
        .next()
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .filter(|chosen| !chosen.is_empty())
        .map(PathBuf::from)
}

/// Reads the previous run's record; any absence or damage is `None` with a debug event naming why, never an error.
#[must_use]
pub fn read(target: &Path) -> Option<Record> {
    let place = path(target);
    match load(&place) {
        Ok(record) => Some(record),
        Err(reason) => {
            proxima_telemetry::debug!(reason = reason, path = %place.display(), "no previous run record");
            None
        }
    }
}

fn load(place: &Path) -> Result<Record, &'static str> {
    let file = File::open(place).map_err(|error| if error.kind() == io::ErrorKind::NotFound { "missing" } else { "unreadable" })?;
    let mut bytes = [0_u8; RECORD_BYTES + 1];
    let mut filled = 0;
    let mut source = file.take(bytes.len() as u64);
    while filled < bytes.len() {
        match source.read(&mut bytes[filled..]).map_err(|_| "unreadable")? {
            0 => break,
            count => filled += count,
        }
    }
    decode(&bytes[..filled])
}

/// Replaces the record atomically: the bytes land in a temporary file beside it, then a rename puts them in place.
///
/// # Errors
///
/// The I/O error of creating the directory, writing the temporary file or renaming it; a failed rename removes the temporary file.
pub fn write(target: &Path, record: Record) -> io::Result<()> {
    let place = path(target);
    let temporary = stage(&place, &encode(record))?;
    commit(&temporary, &place)
}

fn stage(place: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let directory = place.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    fs::create_dir_all(directory)?;
    let temporary = directory.join(std::format!("{FILE}.{}.tmp", std::process::id()));
    let mut file = File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(temporary)
}

fn commit(temporary: &Path, place: &Path) -> io::Result<()> {
    fs::rename(temporary, place).inspect_err(|_| {
        let _ = fs::remove_file(temporary);
    })
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::{commit, decode, encode, path, read, stage, target_directory, write};
    use crate::test_dir::tempdir;

    const RUN: (u64, u32, u8) = (258_230, 383, 1);

    #[test]
    fn a_record_survives_the_round_trip_in_fixed_little_endian_fields() {
        let bytes = encode(RUN);
        assert_eq!(bytes, [1, 0xB6, 0xF0, 0x03, 0, 0, 0, 0, 0, 0x7F, 0x01, 0, 0, 1]);
        assert_eq!(decode(&bytes), Ok(RUN));
    }

    #[test]
    fn a_wrong_length_or_version_is_not_a_record() {
        let good = encode(RUN);
        let mut other_version = good;
        other_version[0] = 2;
        let mut longer = good.to_vec();
        longer.push(0);
        assert_eq!(decode(&good[..13]), Err("wrong length"));
        assert_eq!(decode(&longer), Err("wrong length"));
        assert_eq!(decode(&[]), Err("wrong length"));
        assert_eq!(decode(&other_version), Err("wrong version"));
    }

    #[test]
    fn the_second_read_returns_what_the_first_run_wrote() {
        let target = tempdir().expect("tempdir is creatable");
        assert_eq!(read(target.path()), None);
        write(target.path(), RUN).expect("first run writes its record");
        assert_eq!(read(target.path()), Some(RUN));
        write(target.path(), (1_000, 12, 0)).expect("second run replaces it");
        assert_eq!(read(target.path()), Some((1_000, 12, 0)));
    }

    #[test]
    fn a_corrupt_or_unreadable_record_reads_as_no_previous_run() {
        let target = tempdir().expect("tempdir is creatable");
        let place = path(target.path());
        std::fs::create_dir_all(place.parent().expect("record has a directory")).expect("directory is creatable");
        std::fs::write(&place, [1_u8, 2, 3]).expect("short record is writable");
        assert_eq!(read(target.path()), None);
        let mut bad_version = encode(RUN);
        bad_version[0] = 9;
        std::fs::write(&place, bad_version).expect("bad version is writable");
        assert_eq!(read(target.path()), None);
        std::fs::remove_file(&place).expect("record is removable");
        std::fs::create_dir(&place).expect("a directory can stand where the record should be");
        assert_eq!(read(target.path()), None);
    }

    #[test]
    fn the_bytes_are_staged_beside_the_record_and_only_the_rename_publishes_them() {
        let target = tempdir().expect("tempdir is creatable");
        write(target.path(), RUN).expect("earlier run writes its record");
        let place = path(target.path());
        let temporary = stage(&place, &encode((5, 6, 0))).expect("staging succeeds");
        assert_eq!(temporary.parent(), place.parent());
        assert_eq!(read(target.path()), Some(RUN));
        assert_eq!(std::fs::read(&temporary).expect("staged file is readable"), encode((5, 6, 0)));
        commit(&temporary, &place).expect("rename succeeds");
        assert_eq!(read(target.path()), Some((5, 6, 0)));
        assert!(!temporary.exists());
    }

    #[test]
    fn a_failed_rename_leaves_the_place_untouched_and_the_temporary_removed() {
        let target = tempdir().expect("tempdir is creatable");
        let place = path(target.path());
        std::fs::create_dir_all(&place).expect("a directory can stand where the record should be");
        std::fs::write(place.join("occupant"), b"x").expect("occupant is writable");
        assert!(write(target.path(), RUN).is_err());
        let names: Vec<_> = std::fs::read_dir(place.parent().expect("record has a directory"))
            .expect("directory is listable")
            .map(|entry| entry.expect("entry is readable").file_name())
            .collect();
        assert_eq!(names, ["last-run"]);
        assert!(place.join("occupant").exists());
    }

    #[test]
    fn the_target_directory_prefers_the_explicit_root_then_the_environment_then_metadata() {
        let metadata = Some(b"/work/target".as_slice());
        assert_eq!(target_directory([Some(b"/a"), Some(b"/b"), metadata]), Some("/a".into()));
        assert_eq!(target_directory([None, Some(b"/b"), metadata]), Some("/b".into()));
        assert_eq!(target_directory([None, None, metadata]), Some("/work/target".into()));
        assert_eq!(target_directory([None, Some(b""), None]), None);
        assert_eq!(target_directory([None, None, Some(b"\xff".as_slice())]), None);
    }
}
