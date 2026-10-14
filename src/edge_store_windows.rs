use core::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::FileExt;
use std::path::Path;
use std::process;
use std::string::ToString;

const MAGIC: &[u8; 8] = b"CVRUN001";
const HEADER_BYTES: usize = 12;
const DIRECTORY: &str = "cargo-val";
const RUN_DIRECTORY: &str = "run";
const DATA_FILE: &str = "data";

/// One run's append-only store: `(file, end offset, lost bytes, noticed)`. A missing file is a memory-only run.
///
/// Pages follow an 8-byte magic: `prev_offset u64 LE | len u32 LE | bytes`, `prev_offset` being the
/// previous page of the same stream (0 for none, since no page starts before offset 8).
pub type Store = (Option<File>, Cell<u64>, Cell<u64>, Cell<bool>);

/// One stream's chain through the store: `(stream key, first page, last page, bytes sealed)`; pages are 0 when none is sealed.
pub type View = (u32, u64, u64, u64);

/// A store with no file: every append fails, nothing is noticed, bytes are counted lost.
#[must_use]
pub fn unavailable() -> Store {
    (None, Cell::new(0), Cell::new(0), Cell::new(true))
}

/// Opens `<target>\cargo-val\run\<pid>\data`; any failure warns once on stderr and yields a memory-only store, never an error.
///
/// With no target directory the run is memory-only silently: there was nowhere to ask for a store.
#[must_use]
pub fn open(target: Option<&Path>) -> Store {
    let Some(target) = target else {
        return unavailable();
    };
    let place = target.join(DIRECTORY).join(RUN_DIRECTORY).join(process::id().to_string());
    match create(&place).and_then(from_file) {
        Ok(store) => store,
        Err(error) => {
            std::eprintln!("cargo-val: warn: run store unavailable at {}: {error}; output past the capture window is not kept", place.display());
            unavailable()
        }
    }
}

fn create(place: &Path) -> io::Result<File> {
    fs::create_dir_all(place)?;
    OpenOptions::new().read(true).write(true).create(true).truncate(true).open(place.join(DATA_FILE))
}

fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = file.seek_write(bytes, offset)?;
        if count == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[count..];
        offset += count as u64;
    }
    Ok(())
}

fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = file.seek_read(bytes, offset)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = &mut bytes[count..];
        offset += count as u64;
    }
    Ok(())
}

/// Wraps an already-open file as a store, writing the magic; the seam a test uses to inject a file that fails.
///
/// # Errors
///
/// The write error of the magic.
pub fn from_file(file: File) -> io::Result<Store> {
    write_all_at(&file, MAGIC, 0)?;
    Ok((Some(file), Cell::new(MAGIC.len() as u64), Cell::new(0), Cell::new(false)))
}

/// Seals `bytes` as the next page of the chain in `view` and returns the extended view.
///
/// Pipe-shaped: `(View, &[u8]) -> io::Result<View>`. The end offset only advances on a complete write,
/// so a failed or short write is overwritten by the next page rather than left in the chain.
///
/// # Errors
///
/// `NotConnected` for a memory-only store, or the OS error of the write.
pub fn append(store: &Store, (key, first, last, total): View, bytes: &[u8]) -> io::Result<View> {
    let file = store.0.as_ref().ok_or(io::ErrorKind::NotConnected)?;
    let end = store.1.get();
    let mut header = [0_u8; HEADER_BYTES];
    header[..8].copy_from_slice(&last.to_le_bytes());
    header[8..].copy_from_slice(&u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)?.to_le_bytes());
    write_all_at(file, &header, end)?;
    write_all_at(file, bytes, end + HEADER_BYTES as u64)?;
    store.1.set(end + (HEADER_BYTES + bytes.len()) as u64);
    Ok((key, if first == 0 { end } else { first }, end, total + bytes.len() as u64))
}

/// Reads the page at `offset` into the caller's `page` and returns `(previous page offset, bytes read)`.
///
/// # Errors
///
/// `NotConnected` for a memory-only store, `InvalidData` when the page is longer than `page`, or the OS read error.
pub fn read_page(store: &Store, offset: u64, page: &mut [u8]) -> io::Result<(u64, usize)> {
    let file = store.0.as_ref().ok_or(io::ErrorKind::NotConnected)?;
    let mut header = [0_u8; HEADER_BYTES];
    read_exact_at(file, &mut header, offset)?;
    let previous = u64::from_le_bytes(<[u8; 8]>::try_from(&header[..8]).map_err(|_| io::ErrorKind::InvalidData)?);
    let length = u32::from_le_bytes(<[u8; 4]>::try_from(&header[8..]).map_err(|_| io::ErrorKind::InvalidData)?) as usize;
    let window = page.get_mut(..length).ok_or(io::ErrorKind::InvalidData)?;
    read_exact_at(file, window, offset + HEADER_BYTES as u64)?;
    Ok((previous, length))
}

/// Counts `bytes` as lost and, the first time only, says so on stderr; the caller keeps draining.
pub fn lose(store: &Store, bytes: usize, error: &io::Error) {
    store.2.set(store.2.get() + bytes as u64);
    if !store.3.replace(true) {
        std::eprintln!("cargo-val: warn: run store write failed: {error}; output past the capture window is being dropped");
    }
}
