//! The `#[ignore]`d test names of one test binary, from `--list --ignored`, in caller-free fixed storage.

use std::boxed::Box;
use std::vec;

const BYTES_CAPACITY: usize = 262_144;
const SLOT_COUNT: usize = 4096;
const MAX_ENTRIES: usize = SLOT_COUNT / 4 * 3;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Why a name could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoredSetFull {
    /// Name bytes would pass 256 KiB.
    Bytes,
    /// Entries would pass 3072.
    Entries,
}

/// Open-addressing set of test names; membership is O(1) and never allocates.
pub struct IgnoredSet {
    bytes: Box<[u8]>,
    used: usize,
    slots: Box<[u32]>,
    entries: usize,
}

fn hash(name: &[u8]) -> u64 {
    name.iter()
        .fold(FNV_OFFSET, |state, byte| (state ^ u64::from(*byte)).wrapping_mul(FNV_PRIME))
}

impl IgnoredSet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: vec![0; BYTES_CAPACITY].into_boxed_slice(),
            used: 0,
            slots: vec![0; SLOT_COUNT].into_boxed_slice(),
            entries: 0,
        }
    }

    /// Forgets every name so the same storage serves the next binary.
    pub fn clear(&mut self) {
        self.slots.fill(0);
        self.used = 0;
        self.entries = 0;
    }

    /// Number of distinct names held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries == 0
    }

    // a slot holds the offset of a length-prefixed name plus one; zero is empty
    fn stored(&self, slot: u32) -> &[u8] {
        let start = slot as usize - 1;
        let length = usize::from(u16::from_le_bytes([self.bytes[start], self.bytes[start + 1]]));
        &self.bytes[start + 2..start + 2 + length]
    }

    fn probe(&self, name: &[u8]) -> (usize, bool) {
        let mut index = usize::try_from(hash(name) % SLOT_COUNT as u64).unwrap_or(0);
        loop {
            let slot = self.slots[index];
            if slot == 0 {
                return (index, false);
            }
            if self.stored(slot) == name {
                return (index, true);
            }
            index = (index + 1) % SLOT_COUNT;
        }
    }

    /// Adds `name`; a name already present is left as is.
    ///
    /// # Errors
    ///
    /// Returns [`IgnoredSetFull`] when the fixed byte or entry capacity is exhausted.
    pub fn insert(&mut self, name: &[u8]) -> Result<(), IgnoredSetFull> {
        let (index, present) = self.probe(name);
        if present {
            return Ok(());
        }
        let length = u16::try_from(name.len()).map_err(|_| IgnoredSetFull::Bytes)?;
        if self.used + 2 + name.len() > BYTES_CAPACITY {
            return Err(IgnoredSetFull::Bytes);
        }
        if self.entries == MAX_ENTRIES {
            return Err(IgnoredSetFull::Entries);
        }
        self.bytes[self.used..self.used + 2].copy_from_slice(&length.to_le_bytes());
        self.bytes[self.used + 2..self.used + 2 + name.len()].copy_from_slice(name);
        self.slots[index] = u32::try_from(self.used + 1).map_err(|_| IgnoredSetFull::Bytes)?;
        self.used += 2 + name.len();
        self.entries += 1;
        Ok(())
    }

    #[must_use]
    pub fn contains(&self, name: &[u8]) -> bool {
        self.probe(name).1
    }
}

impl core::fmt::Debug for IgnoredSet {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("IgnoredSet")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl Default for IgnoredSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use super::{IgnoredSet, IgnoredSetFull};

    const IGNORED: &str = include_str!("../tests/fixtures/filterset/interop-ignored.txt");

    #[test]
    fn holds_exactly_the_real_ignored_names_of_the_interop_selection() {
        let mut set = Box::new(IgnoredSet::new());
        let names: Vec<&str> = IGNORED.lines().collect();
        for name in &names {
            set.insert(name.as_bytes()).expect("32 names fit");
        }
        set.insert(names[0].as_bytes()).expect("a repeat is accepted");

        assert_eq!(set.len(), 32);
        assert!(names.iter().all(|name| set.contains(name.as_bytes())));
        assert!(!set.contains(b"bind::tests::bf8_refuses_ggml_layout_and_cpu_packed_decode"));
        assert!(!set.contains(b""));
        let prefixes_absent = names.iter().all(|name| !set.contains(&name.as_bytes()[..name.len() - 1]));
        assert!(prefixes_absent);
    }

    #[test]
    fn entry_capacity_is_refused_not_overwritten() {
        let mut set = Box::new(IgnoredSet::new());
        let mut accepted = 0;
        let mut refusal = None;
        for index in 0..4000 {
            match set.insert(format!("module::test_{index}").as_bytes()) {
                Ok(()) => accepted += 1,
                Err(full) => {
                    refusal = Some(full);
                    break;
                }
            }
        }
        assert_eq!(accepted, 3072);
        assert_eq!(refusal, Some(IgnoredSetFull::Entries));
        let all_found = (0..accepted).all(|index| set.contains(format!("module::test_{index}").as_bytes()));
        assert!(all_found);
        assert!(!set.contains(String::from("module::test_3072").as_bytes()));
    }
}
