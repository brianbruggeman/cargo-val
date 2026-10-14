//! Windows over a failure's output, read from the run store through one caller-owned buffer.
//!
//! The store holds every byte of a failed session. A reader never holds more than one window: `locate` finds the
//! window around the first panic, `pages` hands every page of a stream to a sink in order. Neither allocates.

use crate::edge_store::{self, HEADER_BYTES, Store, View};
use crate::edge_stream::CAPTURE_BYTES;
use memchr::memmem;

use crate::failure_class::{Exit, classify};

pub(crate) const PAGE_BYTES: usize = CAPTURE_BYTES as usize;
const CARRY_BYTES: usize = 8192;
const LOOKAHEAD_BYTES: usize = 4096;
pub(crate) const WINDOW_BYTES: usize = CARRY_BYTES + PAGE_BYTES;
/// The most of a stream one output event carries; a longer stream is read from the store, never held in the event.
pub(crate) const SYNC_WINDOW_BYTES: usize = PAGE_BYTES / 2;

/// Moves the window around the first complete panic report of a stream to the front of `buffer` and returns its length.
///
/// A stream with no panic report yields its first page. `buffer` holds a carried tail of the previous page ahead of each
/// new page, so a report that straddles a page boundary is read whole.
pub(crate) fn locate(store: &Store, (view, tail): (View, &[u8]), buffer: &mut [u8; WINDOW_BYTES]) -> usize {
    let (_, first, last, _) = view;
    let mut carried = 0;
    let mut position = (0, first);
    let mut walking = first != 0;
    while walking {
        let Ok((at, length)) = edge_store::next_page(store, position, &mut buffer[CARRY_BYTES..]) else {
            break;
        };
        position = (at, at + (HEADER_BYTES + length) as u64);
        let (start, stop) = (CARRY_BYTES - carried, CARRY_BYTES + length);
        if let Some(begin) = reports(&buffer[start..stop], (at == last && tail.is_empty(), at == first && carried == 0)) {
            buffer.copy_within(start + begin..stop, 0);
            return stop - start - begin;
        }
        carried = (stop - start).min(CARRY_BYTES);
        buffer.copy_within(stop - carried..stop, CARRY_BYTES - carried);
        walking = at != last;
    }
    if !tail.is_empty() {
        let (start, stop) = (CARRY_BYTES - carried, CARRY_BYTES + tail.len());
        buffer[CARRY_BYTES..stop].copy_from_slice(tail);
        if let Some(begin) = reports(&buffer[start..stop], (true, first == 0 && carried == 0)) {
            buffer.copy_within(start + begin..stop, 0);
            return stop - start - begin;
        }
    }
    head(store, (view, tail), buffer)
}

fn reports(window: &[u8], (last, whole): (bool, bool)) -> Option<usize> {
    let record = classify(window, Exit::Code(101), 0).ok()?;
    let message = record.message.as_ptr().addr().saturating_sub(window.as_ptr().addr());
    let end = message + record.message.len();
    let complete = last || end + LOOKAHEAD_BYTES <= window.len();
    let begin = if whole && window.len() <= SYNC_WINDOW_BYTES { 0 } else { section_start(&window[..message], end) };
    complete.then_some(begin)
}

// the report starts at libtest's `---- name stdout ----` line when one precedes it, so the projections still see the section they strip
fn section_start(before: &[u8], end: usize) -> usize {
    let thread = memmem::rfind(before, b"thread '").unwrap_or(0);
    let header = memmem::rfind(before, b"\n---- ").map(|at| at + 1);
    let preface = header.map(|at| if before[..at].ends_with(b"failures:\n\n") { at - b"failures:\n\n".len() } else { at });
    preface.filter(|begin| end - begin + LOOKAHEAD_BYTES <= SYNC_WINDOW_BYTES).unwrap_or(thread)
}

fn head(store: &Store, (view, tail): (View, &[u8]), buffer: &mut [u8; WINDOW_BYTES]) -> usize {
    if view.1 != 0
        && let Ok((_, length)) = edge_store::next_page(store, (0, view.1), &mut buffer[..PAGE_BYTES])
    {
        return length;
    }
    buffer[..tail.len()].copy_from_slice(tail);
    tail.len()
}


#[cfg(test)]
mod tests {
    use std::string::String;
    use std::vec::Vec;

    use super::{PAGE_BYTES, WINDOW_BYTES, locate};
    use crate::edge_store::{self, Store, View};

    const PANIC_REPORT: &[u8] = b"thread 'fixture::fails_when_requested' panicked at src/lib.rs:12:13:\nrequested fixture failure\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";

    fn noise(total: usize) -> Vec<u8> {
        (0..total).map(|index| if index % 64 == 63 { b'\n' } else { b'a' + u8::try_from(index % 26).unwrap_or(0) }).collect()
    }

    fn sealed(store: &Store, key: u32, bytes: &[u8]) -> View {
        bytes.chunks(PAGE_BYTES).fold((key, 0, 0, 0), |view, page| edge_store::append(store, view, page).expect("page seals"))
    }

    fn located(store: &Store, view: View, tail: &[u8]) -> Vec<u8> {
        let mut buffer = std::boxed::Box::new([0_u8; WINDOW_BYTES]);
        let length = locate(store, (view, tail), &mut buffer);
        buffer[..length].to_vec()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().expect("temporary target");
        let store = edge_store::open(Some(directory.path()));
        (directory, store)
    }

    #[test]
    fn a_panic_near_the_end_of_a_one_mebibyte_stream_is_found_with_its_neighbours() {
        let (_directory, store) = store();
        let mut output = noise(1_048_576);
        let at = output.len() - 40_000;
        output.splice(at..at, PANIC_REPORT.iter().copied());
        let view = sealed(&store, 0, &output);

        let window = located(&store, view, &[]);

        assert!(window.starts_with(b"thread 'fixture::fails_when_requested' panicked at src/lib.rs:12:13:\nrequested fixture failure\n"), "{}", String::from_utf8_lossy(&window[..80.min(window.len())]));
        assert!(window.len() > PANIC_REPORT.len() + 4096, "the neighbours after the report are loaded too: {}", window.len());
    }

    #[test]
    fn a_panic_straddling_a_page_boundary_is_read_whole() {
        let (_directory, store) = store();
        let mut output = noise(3 * PAGE_BYTES);
        let at = PAGE_BYTES - 30;
        output.splice(at..at, core::iter::once(b'\n').chain(PANIC_REPORT.iter().copied()));
        let view = sealed(&store, 0, &output);

        let window = located(&store, view, &[]);

        assert!(window.starts_with(b"thread 'fixture::fails_when_requested' panicked at"), "{} bytes starting {:?}", window.len(), String::from_utf8_lossy(&window[..60.min(window.len())]));
        assert!(window.windows(PANIC_REPORT.len()).any(|candidate| candidate == PANIC_REPORT), "the whole report is in one window");
    }

    #[test]
    fn a_stream_with_a_panic_in_its_unsealed_tail_reads_it_from_the_tail() {
        let (_directory, store) = store();
        let view = sealed(&store, 0, &noise(2 * PAGE_BYTES));

        let window = located(&store, view, PANIC_REPORT);

        assert!(window.starts_with(b"thread 'fixture::fails_when_requested' panicked at"));
    }

    #[test]
    fn a_stream_without_a_panic_yields_its_first_page_and_the_other_stream_is_stepped_over() {
        let (_directory, store) = store();
        let (first, second) = (noise(3 * PAGE_BYTES), noise(PAGE_BYTES).iter().map(u8::to_ascii_uppercase).collect::<Vec<u8>>());
        let mut stdout = (0, 0, 0, 0);
        let mut stderr = (1, 0, 0, 0);
        for (page, other) in first.chunks(PAGE_BYTES).zip(second.chunks(PAGE_BYTES).chain(core::iter::repeat(&b""[..]))) {
            stdout = edge_store::append(&store, stdout, page).expect("stdout page seals");
            if !other.is_empty() {
                stderr = edge_store::append(&store, stderr, other).expect("stderr page seals");
            }
        }

        let head = located(&store, stdout, &[]);
        let other_head = located(&store, stderr, &[]);

        assert_eq!(head, first[..PAGE_BYTES]);
        assert_eq!(other_head, second);
    }

    #[test]
    fn a_memory_only_store_yields_the_resident_tail() {
        let store = edge_store::unavailable();

        let window = located(&store, (0, 0, 0, 0), b"only the resident tail survives\n");

        assert_eq!(window, b"only the resident tail survives\n");
    }
}
