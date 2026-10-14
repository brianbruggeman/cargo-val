use core::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
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

/// Opens `<target>/cargo-val/run/<pid>/data`; any failure warns once on stderr and yields a memory-only store, never an error.
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

/// Wraps an already-open file as a store, writing the magic; the seam a test uses to inject a file that fails.
///
/// # Errors
///
/// The write error of the magic.
pub fn from_file(file: File) -> io::Result<Store> {
    file.write_all_at(MAGIC, 0)?;
    Ok((Some(file), Cell::new(MAGIC.len() as u64), Cell::new(0), Cell::new(false)))
}

/// Seals `bytes` as the next page of the chain in `view` with `pwritev` (retried on EINTR and short writes), and returns the extended view.
///
/// Pipe-shaped: `(View, &[u8]) -> io::Result<View>`. The end offset only advances on a complete write,
/// so a failed or short write is overwritten by the next page rather than left in the chain.
///
/// # Errors
///
/// `NotConnected` for a memory-only store, the OS error of the write, or `WriteZero` when the file accepts no bytes.
pub fn append(store: &Store, (key, first, last, total): View, bytes: &[u8]) -> io::Result<View> {
    let file = store.0.as_ref().ok_or(io::ErrorKind::NotConnected)?;
    let end = store.1.get();
    let mut header = [0_u8; HEADER_BYTES];
    header[..8].copy_from_slice(&last.to_le_bytes());
    header[8..].copy_from_slice(&u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)?.to_le_bytes());
    write_page(file, &header, bytes, end)?;
    store.1.set(end + (HEADER_BYTES + bytes.len()) as u64);
    Ok((key, if first == 0 { end } else { first }, end, total + bytes.len() as u64))
}

fn write_page(file: &File, header: &[u8; HEADER_BYTES], bytes: &[u8], end: u64) -> io::Result<()> {
    let total = HEADER_BYTES + bytes.len();
    let mut done = 0;
    while done < total {
        let skipped_header = done.min(HEADER_BYTES);
        let skipped_bytes = done - skipped_header;
        let vectors = [
            libc::iovec { iov_base: header[skipped_header..].as_ptr().cast_mut().cast(), iov_len: HEADER_BYTES - skipped_header },
            libc::iovec { iov_base: bytes[skipped_bytes..].as_ptr().cast_mut().cast(), iov_len: bytes.len() - skipped_bytes },
        ];
        let offset = libc::off_t::try_from(end + done as u64).map_err(|_| io::ErrorKind::InvalidInput)?;
        let written = unsafe { libc::pwritev(file.as_raw_fd(), vectors.as_ptr(), 2, offset) };
        match usize::try_from(written) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => done += count,
            Err(_) => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

/// Reads the page at `offset` into the caller's `page` and returns `(previous page offset, bytes read)`.
///
/// # Errors
///
/// `NotConnected` for a memory-only store, `InvalidData` when the page is longer than `page`, or the OS read error.
pub fn read_page(store: &Store, offset: u64, page: &mut [u8]) -> io::Result<(u64, usize)> {
    let file = store.0.as_ref().ok_or(io::ErrorKind::NotConnected)?;
    let mut header = [0_u8; HEADER_BYTES];
    file.read_exact_at(&mut header, offset)?;
    let previous = u64::from_le_bytes(<[u8; 8]>::try_from(&header[..8]).map_err(|_| io::ErrorKind::InvalidData)?);
    let length = u32::from_le_bytes(<[u8; 4]>::try_from(&header[8..]).map_err(|_| io::ErrorKind::InvalidData)?) as usize;
    let window = page.get_mut(..length).ok_or(io::ErrorKind::InvalidData)?;
    file.read_exact_at(window, offset + HEADER_BYTES as u64)?;
    Ok((previous, length))
}

/// Counts `bytes` as lost and, the first time only, says so on stderr; the caller keeps draining.
pub fn lose(store: &Store, bytes: usize, error: &io::Error) {
    store.2.set(store.2.get() + bytes as u64);
    if !store.3.replace(true) {
        std::eprintln!("cargo-val: warn: run store write failed: {error}; output past the capture window is being dropped");
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;
    use std::vec;
    use std::vec::Vec;

    use super::{append, from_file, open, read_page, unavailable};
    use crate::test_alloc::measure_allocations;

    #[test]
    fn pages_chain_back_through_their_links_and_read_into_a_caller_page() {
        let directory = tempfile::tempdir().expect("temporary target");
        let store = open(Some(directory.path()));
        let mut view = (7, 0, 0, 0);

        for page in [b"first page".as_slice(), b"second", b"third and last"] {
            view = append(&store, view, page).expect("page seals");
        }
        let mut offsets = Vec::new();
        let mut cursor = view.2;
        let mut buffer = [0_u8; 64];
        let mut lengths = Vec::new();
        while cursor != 0 {
            let (previous, length) = read_page(&store, cursor, &mut buffer).expect("page reads");
            offsets.push(cursor);
            lengths.push(length);
            cursor = previous;
        }

        assert_eq!((view.0, view.3), (7, 10 + 6 + 14));
        assert_eq!(lengths, [14, 6, 10], "newest page first");
        assert_eq!(offsets.last().copied(), Some(view.1), "the oldest page is the first page");
    }

    #[test]
    fn a_page_longer_than_the_callers_page_is_refused() {
        let directory = tempfile::tempdir().expect("temporary target");
        let store = open(Some(directory.path()));
        let view = append(&store, (0, 0, 0, 0), &[1_u8; 32]).expect("page seals");

        let refused = read_page(&store, view.2, &mut [0_u8; 8]);

        assert_eq!(refused.map_err(|error| error.kind()), Err(std::io::ErrorKind::InvalidData));
    }


    fn chain(store: &super::Store, view: super::View) -> Vec<(u64, usize)> {
        let mut pages = Vec::new();
        let mut cursor = view.2;
        let mut buffer = [0_u8; 8192];
        while cursor != 0 {
            let (previous, length) = read_page(store, cursor, &mut buffer).expect("page reads back");
            pages.push((cursor, length));
            cursor = previous;
        }
        pages.reverse();
        pages
    }

    fn assert_contiguous(pages: &[(u64, usize)]) {
        assert_eq!(pages.first().map(|page| page.0), Some(8), "the first page follows the magic");
        for pair in pages.windows(2) {
            assert_eq!(pair[0].0 + 12 + pair[0].1 as u64, pair[1].0, "no gap or overlap between {pair:?}");
        }
    }

    fn set_file_size_limit(bytes: libc::rlim_t) {
        let limit = libc::rlimit { rlim_cur: bytes, rlim_max: libc::RLIM_INFINITY };
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit), 0, "file size limit applies");
        }
    }

    #[test]
    fn a_write_that_hits_the_size_limit_leaves_a_clean_chain_and_the_next_append_links_to_it() {
        let directory = tempfile::tempdir().expect("temporary target");
        let file = std::fs::File::options().read(true).write(true).create_new(true).open(directory.path().join("data")).expect("data file");
        let store = from_file(file).expect("store opens");
        let page = [0x5A_u8; 4096];
        let mut view = (3, 0, 0, 0);
        set_file_size_limit(10_000);

        let failure = loop {
            match append(&store, view, &page) {
                Ok(next) => view = next,
                Err(error) => break error,
            }
        };
        let end_after_failure = store.1.get();
        let good_pages = chain(&store, view);
        set_file_size_limit(libc::RLIM_INFINITY);
        let resumed = append(&store, view, b"after the limit lifts").expect("append works once the limit lifts");
        let resumed_pages = chain(&store, resumed);

        assert_ne!(failure.kind(), std::io::ErrorKind::Interrupted, "a genuine error surfaces: {failure}");
        assert_eq!(good_pages.len(), 2, "only whole pages under 10000 bytes were sealed");
        assert_contiguous(&good_pages);
        assert_eq!(end_after_failure, 8 + 2 * (12 + 4096), "the failed page did not advance the end");
        assert_eq!(resumed_pages.len(), 3);
        assert_contiguous(&resumed_pages);
        assert_eq!(resumed_pages[2], (end_after_failure, 21), "the page after the failure lands where the failed one began");
        assert_eq!(resumed.3, 2 * 4096 + 21);
    }

    #[test]
    fn a_leftover_data_file_from_a_dead_run_with_the_same_pid_is_replaced() {
        let directory = tempfile::tempdir().expect("temporary target");
        let place = directory.path().join("cargo-val").join("run").join(std::process::id().to_string());
        std::fs::create_dir_all(&place).expect("previous run directory");
        std::fs::write(place.join("data"), vec![0xEE_u8; 50_000]).expect("previous run data");

        let store = open(Some(directory.path()));
        let view = append(&store, (0, 0, 0, 0), b"this run").expect("the replaced store takes pages");

        assert!(store.0.is_some(), "the store is file-backed, not memory-only");
        assert_eq!(std::fs::metadata(place.join("data")).expect("data file").len(), 8 + 12 + 8, "nothing of the previous run remains");
        assert_eq!(chain(&store, view), [(8, 8)]);
    }
    #[test]
    fn a_memory_only_store_refuses_every_append() {
        let store = unavailable();

        let refused = append(&store, (0, 0, 0, 0), b"output");

        assert_eq!(refused.map_err(|error| error.kind()), Err(std::io::ErrorKind::NotConnected));
    }

    #[test]
    fn ten_thousand_appends_allocate_nothing() {
        let directory = tempfile::tempdir().expect("temporary target");
        let file = std::fs::File::options().read(true).write(true).create_new(true).open(directory.path().join("data")).expect("data file");
        let store = from_file(file).expect("store opens");
        let page = [0xA5_u8; 4096];
        let mut view = (0, 0, 0, 0);

        let (sealed, allocations) = measure_allocations(|| {
            for _ in 0..10_000 {
                view = append(&store, view, &page).expect("page seals");
            }
            view
        });

        assert_eq!(allocations, 0, "append allocates nothing per page");
        assert_eq!(sealed.3, 10_000 * 4096);
    }
}
