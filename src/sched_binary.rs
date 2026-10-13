//! Scheduling records a `#[proxima::test]` binary carries, read from the file's bytes without running it.
//!
//! The writer is `proxima_test::sched::SchedRecord` (proxima at the pinned rev, `proxima-test/src/sched.rs`): one fixed 328-byte
//! record per generated test in a linker section named `__DATA,__prox_test` (Mach-O), `.proxima_test` (ELF) or `.prxtst$m`,
//! which the PE linker merges into `.prxtst` (COFF section names past eight bytes are not a concern at seven).

use crate::build_defaults::SCHED_RECORD_BYTES;


pub const RECORD_BYTES: usize = 328;
pub const SCHED_VERSION: u32 = 2;
pub const GROUP_CAPACITY: usize = 64;
pub const PATH_CAPACITY: usize = 256;
pub const FLAG_SERIAL: u8 = 1;
pub const FLAG_PROCESS: u8 = 2;
pub const FLAG_HAS_GROUP: u8 = 4;

/// Longest header run a locator asks the reader for in one read.
pub const READ_CAPACITY: usize = 65_536;
const HEADER_BYTES: usize = 64;
const PE_TABLE_BYTES: usize = 24 + 240 + 96 * 40;

const MACHO_MAGIC_64: u32 = 0xfeed_facf;
const MACHO_SEGMENT_64: u32 = 0x19;
const MACHO_SECTION: &[u8] = b"__prox_test";
const MACHO_SEGMENT: &[u8] = b"__DATA";
const ELF_SECTION: &[u8] = b".proxima_test";
const PE_SECTION: &[u8] = b".prxtst";

/// Where the record section sits in the file, and how its integers are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub offset: u64,
    pub size: u64,
    pub little_endian: bool,
}

/// Why a file yields no records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// A well-formed object file with no scheduling section: nothing was declared.
    NoSection,
    /// Not an object format this reader knows (32-bit, fat, big-endian ELF, or garbage).
    Unsupported,
    /// A header table is truncated or points past the bytes supplied.
    Malformed,
}

/// What the locator needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Supply the file's bytes from `offset`, at most `length`; a short read at the end of the file is fine.
    Read { offset: u64, length: usize },
    Done(Result<Located, Absent>),
}

const NAMES_CAPACITY: usize = 16384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Header,
    MachCommands { count: u32, size: usize },
    ElfNamesHeader(ElfTable),
    ElfNames(ElfTable, usize),
    ElfTable(ElfTable),
    PeTable,
    Finished(Result<Located, Absent>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ElfTable {
    offset: u64,
    count: usize,
    entry: usize,
    names_index: usize,
}

/// Sans-IO locator: it names the reads it needs and is fed their bytes, so the file I/O stays in the caller.
#[derive(Debug)]
pub struct SectionLocator {
    phase: Phase,
    next: (u64, usize),
    names: [u8; NAMES_CAPACITY],
}

impl SectionLocator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::Header,
            next: (0, HEADER_BYTES),
            names: [0; NAMES_CAPACITY],
        }
    }

    #[must_use]
    pub const fn request(&self) -> Request {
        match self.phase {
            Phase::Finished(result) => Request::Done(result),
            _ => Request::Read { offset: self.next.0, length: self.next.1 },
        }
    }

    /// Takes the bytes read for the last request.
    pub fn supply(&mut self, bytes: &[u8]) {
        self.phase = match self.phase {
            Phase::Header => self.read_header(bytes),
            Phase::MachCommands { count, size } => Phase::Finished(macho_section(bytes, count, size)),
            Phase::ElfNamesHeader(table) => self.read_names_header(bytes, table),
            Phase::ElfNames(table, length) => self.read_names(bytes, table, length),
            Phase::ElfTable(table) => Phase::Finished(elf_section(bytes, table, &self.names)),
            Phase::PeTable => Phase::Finished(pe_section(bytes)),
            Phase::Finished(done) => Phase::Finished(done),
        };
    }

    fn read_header(&mut self, header: &[u8]) -> Phase {
        if header.len() >= 32 && u32_at(header, 0, true) == Some(MACHO_MAGIC_64) {
            let (Some(count), Some(size)) = (u32_at(header, 16, true), u32_at(header, 20, true)) else {
                return Phase::Finished(Err(Absent::Malformed));
            };
            let size = size as usize;
            if size > READ_CAPACITY {
                return Phase::Finished(Err(Absent::Unsupported));
            }
            self.next = (32, size);
            return Phase::MachCommands { count, size };
        }
        if header.len() >= HEADER_BYTES && header.starts_with(b"\x7fELF") {
            return self.begin_elf(header);
        }
        if header.len() >= 0x40 && header.starts_with(b"MZ") {
            let Some(pe_offset) = u32_at(header, 0x3c, true) else {
                return Phase::Finished(Err(Absent::Malformed));
            };
            self.next = (u64::from(pe_offset), PE_TABLE_BYTES);
            return Phase::PeTable;
        }
        Phase::Finished(Err(Absent::Unsupported))
    }

    fn begin_elf(&mut self, header: &[u8]) -> Phase {
        if header[4] != 2 || header[5] != 1 {
            return Phase::Finished(Err(Absent::Unsupported));
        }
        let (Some(offset), Some(entry), Some(count), Some(names_index)) = (
            u64_at(header, 0x28, true),
            u16_at(header, 0x3a, true),
            u16_at(header, 0x3c, true),
            u16_at(header, 0x3e, true),
        ) else {
            return Phase::Finished(Err(Absent::Malformed));
        };
        let (entry, count, names_index) = (usize::from(entry), usize::from(count), usize::from(names_index));
        if count == 0 || names_index >= count || entry < 64 {
            return Phase::Finished(Err(Absent::NoSection));
        }
        if count * entry > READ_CAPACITY {
            return Phase::Finished(Err(Absent::Unsupported));
        }
        let table = ElfTable { offset, count, entry, names_index };
        self.next = (offset + (names_index * entry) as u64, entry);
        Phase::ElfNamesHeader(table)
    }

    fn read_names_header(&mut self, header: &[u8], table: ElfTable) -> Phase {
        let (Some(offset), Some(size)) = (u64_at(header, 24, true), u64_at(header, 32, true)) else {
            return Phase::Finished(Err(Absent::Malformed));
        };
        let length = usize::try_from(size).unwrap_or(usize::MAX);
        if length > NAMES_CAPACITY {
            return Phase::Finished(Err(Absent::Unsupported));
        }
        self.next = (offset, length);
        Phase::ElfNames(table, length)
    }

    fn read_names(&mut self, names: &[u8], table: ElfTable, length: usize) -> Phase {
        if names.len() < length {
            return Phase::Finished(Err(Absent::Malformed));
        }
        self.names[..length].copy_from_slice(&names[..length]);
        self.names[length..].fill(0);
        self.next = (table.offset, table.count * table.entry);
        Phase::ElfTable(table)
    }
}

impl Default for SectionLocator {
    fn default() -> Self {
        Self::new()
    }
}

fn u16_at(bytes: &[u8], at: usize, little: bool) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(at..at + 2)?.try_into().ok()?;
    Some(if little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) })
}

fn u32_at(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(if little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
}

fn u64_at(bytes: &[u8], at: usize, little: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
    Some(if little { u64::from_le_bytes(raw) } else { u64::from_be_bytes(raw) })
}

fn trimmed(name: &[u8]) -> &[u8] {
    name.split(|byte| *byte == 0).next().unwrap_or(name)
}

fn macho_section(commands: &[u8], count: u32, size: usize) -> Result<Located, Absent> {
    let commands = commands.get(..size).ok_or(Absent::Malformed)?;
    let mut at = 0;
    for _ in 0..count {
        let (Some(kind), Some(length)) = (u32_at(commands, at, true), u32_at(commands, at + 4, true)) else {
            return Err(Absent::Malformed);
        };
        let length = length as usize;
        if length < 8 || at + length > commands.len() {
            return Err(Absent::Malformed);
        }
        if kind == MACHO_SEGMENT_64 {
            let sections = u32_at(commands, at + 64, true).ok_or(Absent::Malformed)? as usize;
            for index in 0..sections {
                let section = at + 72 + index * 80;
                let header = commands.get(section..section + 80).ok_or(Absent::Malformed)?;
                if trimmed(&header[..16]) == MACHO_SECTION && trimmed(&header[16..32]) == MACHO_SEGMENT {
                    let (Some(bytes), Some(offset)) = (u64_at(header, 40, true), u32_at(header, 48, true)) else {
                        return Err(Absent::Malformed);
                    };
                    return Ok(Located { offset: u64::from(offset), size: bytes, little_endian: true });
                }
            }
        }
        at += length;
    }
    Err(Absent::NoSection)
}

fn elf_section(headers: &[u8], table: ElfTable, names: &[u8; NAMES_CAPACITY]) -> Result<Located, Absent> {
    for index in 0..table.count {
        let header = headers
            .get(index * table.entry..(index + 1) * table.entry)
            .ok_or(Absent::Malformed)?;
        let name_offset = u32_at(header, 0, true).ok_or(Absent::Malformed)? as usize;
        let name = names.get(name_offset..).map(trimmed).ok_or(Absent::Malformed)?;
        if name == ELF_SECTION {
            let (Some(offset), Some(size)) = (u64_at(header, 24, true), u64_at(header, 32, true)) else {
                return Err(Absent::Malformed);
            };
            return Ok(Located { offset, size, little_endian: true });
        }
    }
    Err(Absent::NoSection)
}

fn pe_section(table: &[u8]) -> Result<Located, Absent> {
    if !table.starts_with(b"PE\0\0") {
        return Err(Absent::Malformed);
    }
    let count = usize::from(u16_at(table, 6, true).ok_or(Absent::Malformed)?);
    let optional = usize::from(u16_at(table, 20, true).ok_or(Absent::Malformed)?);
    let first = 24 + optional;
    for index in 0..count {
        let header = table.get(first + index * 40..first + (index + 1) * 40).ok_or(Absent::Malformed)?;
        if trimmed(&header[..8]) == PE_SECTION {
            let (Some(virtual_size), Some(raw_size), Some(offset)) =
                (u32_at(header, 8, true), u32_at(header, 16, true), u32_at(header, 20, true))
            else {
                return Err(Absent::Malformed);
            };
            return Ok(Located { offset: u64::from(offset), size: u64::from(virtual_size.min(raw_size)), little_endian: true });
        }
    }
    Err(Absent::NoSection)
}

/// One decoded scheduling record, borrowed from the section bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'record> {
    pub flags: u8,
    pub group: &'record [u8],
    pub path: &'record [u8],
}

impl Entry<'_> {
    #[must_use]
    pub const fn serial(&self) -> bool {
        self.flags & FLAG_SERIAL != 0
    }

    #[must_use]
    pub const fn process(&self) -> bool {
        self.flags & FLAG_PROCESS != 0
    }
}

/// Why a record is refused; a refused table schedules nothing from the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The record's version is not one this reader knows.
    Version(u32),
    /// The section is not a whole number of records.
    Partial,
    /// A length field exceeds its capacity.
    Length,
    /// More records than the fixed table holds.
    Capacity,
}

/// Decodes one record.
///
/// # Errors
///
/// Returns [`Fault`] for an unknown version or a length past its capacity.
pub fn decode(record: &[u8; RECORD_BYTES], little_endian: bool) -> Result<Entry<'_>, Fault> {
    let version = u32_at(record, 0, little_endian).unwrap_or(0);
    if version != SCHED_VERSION {
        return Err(Fault::Version(version));
    }
    let group_length = usize::from(record[5]);
    let path_length = usize::from(u16_at(record, 6, little_endian).unwrap_or(u16::MAX));
    if group_length > GROUP_CAPACITY || path_length > PATH_CAPACITY {
        return Err(Fault::Length);
    }
    Ok(Entry {
        flags: record[4],
        group: &record[8..8 + group_length],
        path: &record[72..72 + path_length],
    })
}

/// Checks a section's bytes are whole, versioned records and returns how many.
///
/// # Errors
///
/// Returns [`Fault`] for a partial record, an unknown version, or more records than `SCHED_RECORD_BYTES` holds.
pub fn validate(section: &[u8], little_endian: bool) -> Result<usize, Fault> {
    if !section.len().is_multiple_of(RECORD_BYTES) {
        return Err(Fault::Partial);
    }
    if section.len() > SCHED_RECORD_BYTES {
        return Err(Fault::Capacity);
    }
    let (records, _) = section.as_chunks::<RECORD_BYTES>();
    records.iter().try_for_each(|record| decode(record, little_endian).map(|_| ()))?;
    Ok(records.len())
}
