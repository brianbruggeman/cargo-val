//! Reads the scheduling section of a test binary from disk: the only I/O the sans-IO locator needs.

use std::fs::File;
use std::io;

use crate::sched_binary::{Absent, Fault, READ_CAPACITY, Request, SectionLocator, validate};

#[cfg(unix)]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_at(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

fn read_full(file: &File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match read_at(file, offset + filled as u64, &mut buffer[filled..])? {
            0 => break,
            count => filled += count,
        }
    }
    Ok(filled)
}

/// What a test binary declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loaded {
    /// This many records are in the caller's storage, in the file's byte order.
    Records { count: usize, little_endian: bool },
    /// The binary declares nothing.
    Absent(Absent),
    /// The section is present but this reader refuses it.
    Refused(Fault),
}

/// Reads the scheduling records of the binary at `path` into `storage`.
///
/// # Errors
///
/// Returns the I/O error of opening or reading the file.
#[inline(never)]
pub fn load(path: &str, storage: &mut [u8]) -> io::Result<Loaded> {
    let file = File::open(path)?;
    let mut window = [0_u8; READ_CAPACITY];
    let mut locator = SectionLocator::new();
    let located = loop {
        match locator.request() {
            Request::Read { offset, length } => {
                let length = length.min(READ_CAPACITY);
                let filled = read_full(&file, offset, &mut window[..length])?;
                locator.supply(&window[..filled]);
            }
            Request::Done(Ok(located)) => break located,
            Request::Done(Err(absent)) => return Ok(Loaded::Absent(absent)),
        }
    };
    let size = usize::try_from(located.size).unwrap_or(usize::MAX);
    if size > storage.len() {
        return Ok(Loaded::Refused(Fault::Capacity));
    }
    let filled = read_full(&file, located.offset, &mut storage[..size])?;
    if filled < size {
        return Ok(Loaded::Absent(Absent::Malformed));
    }
    Ok(match validate(&storage[..size], located.little_endian) {
        Ok(count) => Loaded::Records { count, little_endian: located.little_endian },
        Err(fault) => Loaded::Refused(fault),
    })
}
