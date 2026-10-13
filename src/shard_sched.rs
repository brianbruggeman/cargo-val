//! Which of a binary's tests declared scheduling, and the order and grouping their shards launch in.
//!
//! Meanings follow the writer, `proxima_test::sched`: `serial` runs with nothing else in flight; `group = "x"` allows at most one
//! test of x at a time (nextest's default group size); `process` makes the tests of one group, or of the binary when no group
//! is named, share one process.

use core::cmp::Ordering;

use crate::build_defaults::SHARD_NAMES;
use crate::sched_binary::{Entry, FLAG_HAS_GROUP, RECORD_BYTES, decode};
use crate::shard_arena::command_units;

const TAKEN: u32 = 1 << 31;
const BINARY_GROUP: &[u8] = b"@binary";

/// How a test's declarations schedule it, in the order the classes launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// No declaration: sharded with the other plain tests.
    Plain,
    /// `process`: shares one process with its group (the binary when ungrouped).
    Shared,
    /// `group` alone: its own process, at most one of the group at a time.
    Grouped,
    /// `serial, process`: shares one process, which runs with nothing else in flight.
    SerialShared,
    /// `serial`: its own process, with nothing else in flight.
    Serial,
}

impl Class {
    #[must_use]
    pub const fn of(entry: &Entry<'_>) -> Self {
        match (entry.serial(), entry.process(), entry.flags & FLAG_HAS_GROUP != 0) {
            (true, true, _) => Self::SerialShared,
            (true, false, _) => Self::Serial,
            (false, true, _) => Self::Shared,
            (false, false, true) => Self::Grouped,
            (false, false, false) => Self::Plain,
        }
    }

    #[must_use]
    pub const fn from_rank(rank: u8) -> Self {
        match rank {
            1 => Self::Shared,
            2 => Self::Grouped,
            3 => Self::SerialShared,
            4 => Self::Serial,
            _ => Self::Plain,
        }
    }

    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Plain => 0,
            Self::Shared => 1,
            Self::Grouped => 2,
            Self::SerialShared => 3,
            Self::Serial => 4,
        }
    }

    #[must_use]
    pub const fn text(self) -> &'static [u8] {
        match self {
            Self::Plain => b"plain",
            Self::Shared => b"shared",
            Self::Grouped => b"grouped",
            Self::SerialShared => b"serial-shared",
            Self::Serial => b"serial",
        }
    }

    #[must_use]
    pub const fn serial(self) -> bool {
        matches!(self, Self::SerialShared | Self::Serial)
    }
}

/// The group a class of shard counts against, for the one-at-a-time limit.
#[must_use]
pub fn group_key(class: Class, group: &[u8]) -> &[u8] {
    if class == Class::Shared && group.is_empty() { BINARY_GROUP } else { group }
}

/// One binary's scheduling records, sorted by test path so a listed name finds its record by binary search.
#[derive(Debug)]
pub struct SchedTable<'storage> {
    bytes: &'storage mut [u8],
    count: usize,
    little_endian: bool,
}

fn path_of(record: &[u8; RECORD_BYTES], little_endian: bool) -> &[u8] {
    decode(record, little_endian).map_or(&[][..], |entry| entry.path)
}

impl<'storage> SchedTable<'storage> {
    #[must_use]
    pub const fn new(bytes: &'storage mut [u8]) -> Self {
        Self {
            bytes,
            count: 0,
            little_endian: true,
        }
    }

    /// The storage a section is read into.
    pub fn storage(&mut self) -> &mut [u8] {
        self.count = 0;
        self.bytes
    }

    /// Takes `count` validated records now in the storage and sorts them by path.
    pub fn adopt(&mut self, count: usize, little_endian: bool) {
        self.count = count;
        self.little_endian = little_endian;
        let (records, _) = self.bytes[..count * RECORD_BYTES].as_chunks_mut::<RECORD_BYTES>();
        records.sort_unstable_by(|first, second| path_of(first, little_endian).cmp(path_of(second, little_endian)));
    }

    pub const fn clear(&mut self) {
        self.count = 0;
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn records(&self) -> &[[u8; RECORD_BYTES]] {
        self.bytes[..self.count * RECORD_BYTES].as_chunks::<RECORD_BYTES>().0
    }

    /// The record at `index` in path order.
    #[must_use]
    pub fn entry(&self, index: usize) -> Option<Entry<'_>> {
        decode(self.records().get(index)?, self.little_endian).ok()
    }

    /// The path-order index of the record for `name`.
    #[must_use]
    pub fn find(&self, name: &[u8]) -> Option<usize> {
        self.records()
            .binary_search_by(|record| path_of(record, self.little_endian).cmp(name))
            .ok()
    }
}

/// A run of declared tests that one child runs, as positions in the plan's launch order; they stay readable through [`SpecialPlan::at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Special {
    pub first: usize,
    pub last: usize,
    pub class: Class,
}

/// The declared tests of the listed binary, as table indices ordered for launch.
#[derive(Debug)]
pub struct SpecialPlan<'storage> {
    picked: &'storage mut [u32],
    count: usize,
    next: usize,
}

fn launch_order(table: &SchedTable<'_>, first: u32, second: u32) -> Ordering {
    let (Some(left), Some(right)) = (table.entry(first as usize), table.entry(second as usize)) else {
        return Ordering::Equal;
    };
    Class::of(&left)
        .cmp(&Class::of(&right))
        .then_with(|| left.group.cmp(right.group))
        .then_with(|| left.path.cmp(right.path))
}

impl<'storage> SpecialPlan<'storage> {
    #[must_use]
    pub const fn new(picked: &'storage mut [u32]) -> Self {
        Self {
            picked,
            count: 0,
            next: 0,
        }
    }

    /// Adds the record at `index`; false when the plan is full.
    pub fn push(&mut self, index: usize) -> bool {
        let (Some(slot), Ok(index)) = (self.picked.get_mut(self.count), u32::try_from(index)) else {
            return false;
        };
        *slot = index;
        self.count += 1;
        true
    }

    /// Orders the declared tests by class, then group, then path.
    pub fn seal(&mut self, table: &SchedTable<'_>) {
        self.picked[..self.count].sort_unstable_by(|first, second| launch_order(table, *first, *second));
        self.next = 0;
    }

    #[must_use]
    pub const fn has_work(&self) -> bool {
        self.next < self.count
    }

    pub const fn reset(&mut self) {
        self.count = 0;
        self.next = 0;
    }

    /// Drops every test not yet taken and returns how many there were.
    pub fn discard(&mut self) -> usize {
        let dropped = self.picked[self.next..self.count].iter().filter(|index| **index & TAKEN == 0).count();
        self.next = self.count;
        dropped
    }

    /// The table index at `position`, taken or not.
    #[must_use]
    pub fn at(&self, position: usize) -> Option<usize> {
        self.picked.get(position).map(|index| (*index & !TAKEN) as usize)
    }

    /// Finds the next run that may launch now, without marking it taken; [`Self::commit`] marks it once its shard exists.
    ///
    /// `busy` says whether a group already has a child running (one at a time); `alone` says nothing else is in flight or
    /// waiting, which a serial class needs. Classes launch in order, so a serial class is only reached once the rest is taken.
    pub fn take(
        &mut self,
        table: &SchedTable<'_>,
        busy: impl Fn(&[u8]) -> bool,
        alone: bool,
        budget: usize,
    ) -> Option<Special> {
        while self.next < self.count && self.picked[self.next] & TAKEN != 0 {
            self.next += 1;
        }
        let mut position = self.next;
        while position < self.count {
            let Some(entry) = self.at(position).filter(|_| self.picked[position] & TAKEN == 0).and_then(|index| table.entry(index)) else {
                position += 1;
                continue;
            };
            let class = Class::of(&entry);
            if class.serial() && !alone {
                return None;
            }
            if !busy(group_key(class, entry.group)) {
                return Some(self.claim(table, position, class, budget));
            }
            position = self.skip_group(table, position, class);
        }
        None
    }

    fn skip_group(&self, table: &SchedTable<'_>, from: usize, class: Class) -> usize {
        let group = self.at(from).and_then(|index| table.entry(index)).map_or(&[][..], |entry| entry.group);
        (from + 1..self.count)
            .find(|position| {
                self.at(*position)
                    .and_then(|index| table.entry(index))
                    .is_none_or(|entry| Class::of(&entry) != class || entry.group != group)
            })
            .unwrap_or(self.count)
    }

    fn claim(&mut self, table: &SchedTable<'_>, first: usize, class: Class, budget: usize) -> Special {
        let group = self.at(first).and_then(|index| table.entry(index)).map_or(&[][..], |entry| entry.group);
        let shares = matches!(class, Class::Shared | Class::SerialShared);
        let (mut last, mut units) = (first, 0);
        while last < self.count {
            let Some(entry) = self.at(last).and_then(|index| table.entry(index)) else {
                break;
            };
            let same = Class::of(&entry) == class && entry.group == group;
            units += command_units(entry.path);
            if !same || (last > first && (!shares || units > budget || last - first >= SHARD_NAMES)) {
                break;
            }
            last += 1;
        }
        Special { first, last, class }
    }

    /// Marks a run returned by [`Self::take`] as launched, so no later take hands out its names again.
    pub fn commit(&mut self, special: &Special) {
        self.picked[special.first..special.last].iter_mut().for_each(|index| *index |= TAKEN);
    }
}
