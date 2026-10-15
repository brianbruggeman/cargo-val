//! One test binary's selected names, held until the listing ends so shards can be sized from the whole count.

use crate::build_defaults::{SCHED_RECORD_BYTES, SHARD_COMMAND_UNITS, SHARD_NAMES, SHARD_RETRY_SLOTS};
use crate::sched_binary::RECORD_BYTES;

const PATH_CAPACITY: usize = 512;
const MAX_RESERVE: usize = 8193;
const FLAG_WORDS: usize = SHARD_NAMES.div_ceil(64);

/// Bytes for one declared group: every scheduling record's path, each with its NUL, so a group fits whatever the table holds.
pub const DECLARED_BYTES: usize = (SCHED_RECORD_BYTES / RECORD_BYTES) * (PATH_CAPACITY + 1);

/// A contiguous run of arena names, each followed by its NUL, that one child runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    pub start: usize,
    pub end: usize,
    pub count: usize,
    /// The scheduling class's rank for a shard of declared tests; 0 for plain names.
    pub class: u8,
    /// The first declared test's position in the launch order, which names the group a relaunch still counts against.
    pub anchor: u32,
    /// Set when the shard is one name copied out to run again alone; `None` for a shard launched fresh.
    pub rerun: Option<Rerun>,
}

/// A single name run again alone, and what its exit means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rerun {
    /// Its exit decides the test. `after_shard_failure` marks a name that failed inside its shard, so a pass alone is a shared-process failure.
    Decides { after_shard_failure: bool },
    /// Only its output is wanted, because its failure was already counted at its shard line.
    OutputOnly,
}

/// Why a name did not enter the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// The fixed storage is full; the shards sealed so far must run before more names arrive.
    Full,
    /// A name with a NUL byte cannot be an argument.
    InteriorNul,
    /// The name is longer than the reserve that holds the one name that overflows a full arena.
    TooLong,
}

/// Which names of one shard have reported a result, one bit per name in shard order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reported {
    words: [u64; FLAG_WORDS],
    total: usize,
}

impl Reported {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            words: [0; FLAG_WORDS],
            total: 0,
        }
    }

    /// Marks `ordinal` reported; false when it already was.
    pub fn mark(&mut self, ordinal: usize) -> bool {
        let (word, bit) = (ordinal / 64, 1_u64 << (ordinal % 64));
        let fresh = self.words[word] & bit == 0;
        self.words[word] |= bit;
        self.total += usize::from(fresh);
        fresh
    }

    #[must_use]
    pub const fn has(&self, ordinal: usize) -> bool {
        self.words[ordinal / 64] & (1_u64 << (ordinal % 64)) != 0
    }

    #[must_use]
    pub const fn total(&self) -> usize {
        self.total
    }

    /// Clears and returns the lowest marked ordinal.
    pub fn take_first(&mut self) -> Option<usize> {
        let (word, bits) = self.words.iter().enumerate().find(|(_, bits)| **bits != 0)?;
        let bit = bits.trailing_zeros() as usize;
        let ordinal = word * 64 + bit;
        self.words[word] &= !(1_u64 << bit);
        self.total -= 1;
        Some(ordinal)
    }
}

impl Default for Reported {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a result line's name sits in its shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub ordinal: usize,
    pub next: usize,
}

/// A finished shard whose marked names still owe a solo run, drained one name at a time from the shard's own bytes.
#[derive(Debug, Clone, Copy)]
struct Pending {
    shard: Shard,
    owed: Reported,
    failed: Reported,
    decides: bool,
    cursor: (usize, usize),
}

/// Caller-owned storage for the names of the test binary being listed, with the carving of those names into shards.
#[derive(Debug)]
pub struct ShardArena<'storage> {
    names: &'storage mut [u8],
    half: usize,
    declared_base: usize,
    declared_work: usize,
    reserve: usize,
    carry: usize,
    length: usize,
    count: usize,
    cursor: usize,
    unlaunched: usize,
    bound: usize,
    sealed: bool,
    inflight: usize,
    retry: [Option<Pending>; SHARD_RETRY_SLOTS],
    solos: usize,
}

/// Command line units a name costs, counted for the strictest quoting any platform applies.
#[must_use]
pub fn command_units(name: &[u8]) -> usize {
    let Ok(text) = core::str::from_utf8(name) else {
        return name.len() * 2 + 3;
    };
    let units = text.encode_utf16().count();
    let quoted = text.bytes().any(|byte| matches!(byte, b' ' | b'\t' | b'"' | b'\\'));
    if quoted { units * 2 + 3 } else { units + 1 }
}

impl<'storage> ShardArena<'storage> {
    #[must_use]
    pub fn new(names: &'storage mut [u8]) -> Self {
        Self::with_declared(names, 0)
    }

    /// Splits `names` into the arena and a trailing declared region of `declared_length` bytes.
    #[must_use]
    pub fn with_declared(names: &'storage mut [u8], declared_length: usize) -> Self {
        let declared_base = names.len().saturating_sub(declared_length);
        let half = declared_base;
        let reserve = (half / 16).min(MAX_RESERVE);
        Self {
            names,
            half,
            declared_base,
            declared_work: declared_base,
            reserve,
            carry: 0,
            length: 0,
            count: 0,
            cursor: 0,
            unlaunched: 0,
            bound: 1,
            sealed: false,
            inflight: 0,
            retry: [None; SHARD_RETRY_SLOTS],
            solos: 0,
        }
    }

    /// Appends one selected name.
    ///
    /// When the arena is full the name is parked in a reserve that [`Self::reset`] moves to the front, so the
    /// caller can seal the batch without losing the name it was holding.
    ///
    /// # Errors
    ///
    /// Returns [`PushError::Full`] after parking the name, [`PushError::InteriorNul`] for a name with a NUL, and
    /// [`PushError::TooLong`] when the name cannot even be parked.
    pub fn push(&mut self, name: &str) -> Result<(), PushError> {
        if name.as_bytes().contains(&0) {
            return Err(PushError::InteriorNul);
        }
        let need = name.len() + 1;
        let main = self.half - self.reserve;
        if self.length + need <= main {
            self.names[self.length..self.length + need - 1].copy_from_slice(name.as_bytes());
            self.names[self.length + need - 1] = 0;
            self.length += need;
            self.count += 1;
            self.unlaunched += 1;
            return Ok(());
        }
        if need > self.reserve || self.carry != 0 {
            return Err(if need > self.reserve { PushError::TooLong } else { PushError::Full });
        }
        self.names[main..main + need - 1].copy_from_slice(name.as_bytes());
        self.names[main + need - 1] = 0;
        self.carry = need;
        Err(PushError::Full)
    }

    /// Fixes the shard size as `ceil(names / jobs)`, so a binary launches about `jobs` children.
    pub fn seal(&mut self, jobs: usize) {
        self.bound = self.count.div_ceil(jobs.max(1)).clamp(1, SHARD_NAMES);
        self.sealed = true;
    }

    #[must_use]
    pub const fn sealed(&self) -> bool {
        self.sealed
    }

    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Whether nothing is waiting to launch and nothing is running.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.unlaunched == 0 && self.inflight == 0 && !self.retry_pending()
    }

    #[must_use]
    pub fn has_work(&self) -> bool {
        self.unlaunched > 0 || self.retry_pending()
    }

    fn retry_pending(&self) -> bool {
        self.retry.iter().any(Option::is_some)
    }

    // every running shard may finish owing solo runs, so a new one launches only while its record slot is guaranteed free
    fn has_record_slot(&self) -> bool {
        let records = self.retry.iter().flatten().count();
        records + (self.inflight - self.solos) < SHARD_RETRY_SLOTS
    }

    /// Whether a name is parked in the reserve.
    #[must_use]
    pub const fn carrying(&self) -> bool {
        self.carry != 0
    }

    /// Takes the next shard: a solo run owed by a finished shard first, else names from the cursor until the count bound or the command line budget.
    /// `fixed_units` is what the program path and fixed arguments already cost.
    pub fn carve(&mut self, fixed_units: usize) -> Option<Shard> {
        self.carve_owed(|_| true).or_else(|| self.carve_fresh(fixed_units))
    }

    /// Takes the next owed output-only rerun, leaving deciding solo runs and unlaunched names alone.
    pub fn carve_rerun(&mut self) -> Option<Shard> {
        self.carve_owed(|pending| !pending.decides)
    }

    /// Records that `owed` names of a finished `shard` each need one solo run; `failed` marks those that failed inside it.
    ///
    /// # Errors
    ///
    /// Returns [`PushError::Full`] when no record slot is free, which launching only while a slot is reserved rules out.
    pub fn owe_reruns(&mut self, shard: Shard, (owed, failed): (Reported, Reported), decides: bool) -> Result<(), PushError> {
        if owed.total() == 0 {
            return Ok(());
        }
        let slot = self.retry.iter_mut().find(|slot| slot.is_none()).ok_or(PushError::Full)?;
        *slot = Some(Pending { shard, owed, failed, decides, cursor: (0, shard.start) });
        Ok(())
    }

    fn carve_owed(&mut self, wanted: impl Fn(&Pending) -> bool) -> Option<Shard> {
        let slot = self.retry.iter().position(|pending| pending.as_ref().is_some_and(&wanted))?;
        let mut pending = self.retry[slot]?;
        let ordinal = pending.owed.take_first()?;
        let (start, end) = self.advance(&mut pending, ordinal)?;
        self.retry[slot] = (pending.owed.total() != 0).then_some(pending);
        self.inflight += 1;
        self.solos += 1;
        Some(Shard {
            start,
            end,
            count: 1,
            class: 0,
            anchor: 0,
            rerun: Some(if pending.decides {
                Rerun::Decides { after_shard_failure: pending.failed.has(ordinal) }
            } else {
                Rerun::OutputOnly
            }),
        })
    }

    // the owed ordinals ascend, so the cursor only ever walks forward through the shard's own bytes
    fn advance(&self, pending: &mut Pending, ordinal: usize) -> Option<(usize, usize)> {
        let (mut at, mut position) = pending.cursor;
        while at < ordinal {
            position += self.names[position..pending.shard.end].iter().position(|byte| *byte == 0)? + 1;
            at += 1;
        }
        let length = self.names[position..pending.shard.end].iter().position(|byte| *byte == 0)?;
        pending.cursor = (at, position);
        Some((position, position + length + 1))
    }

    fn carve_fresh(&mut self, fixed_units: usize) -> Option<Shard> {
        if !self.sealed || self.unlaunched == 0 || !self.has_record_slot() {
            return None;
        }
        let budget = SHARD_COMMAND_UNITS.saturating_sub(fixed_units);
        let (mut end, mut count, mut units) = (self.cursor, 0, 0);
        while count < self.bound && count < self.unlaunched {
            let name_end = end + self.names[end..self.length].iter().position(|byte| *byte == 0)?;
            units += command_units(&self.names[end..name_end]);
            if count > 0 && units > budget {
                break;
            }
            end = name_end + 1;
            count += 1;
        }
        let shard = Shard {
            start: self.cursor,
            end,
            count,
            class: 0,
            anchor: 0,
            rerun: None,
        };
        self.cursor = end;
        self.unlaunched -= count;
        self.inflight += 1;
        Some(shard)
    }

    /// Copies declared tests' names, which are not contiguous in the listing, into the declared region as one shard.
    /// Returns `None` when the declared region cannot hold them or no record slot is reserved.
    pub fn adopt<'name>(&mut self, names: impl Iterator<Item = &'name [u8]>) -> Option<Shard> {
        if !self.has_record_slot() {
            return None;
        }
        let start = self.declared_work;
        let (mut end, mut count) = (start, 0);
        for name in names {
            let next = end + name.len() + 1;
            if next > self.names.len() {
                return None;
            }
            self.names[end..next - 1].copy_from_slice(name);
            self.names[next - 1] = 0;
            end = next;
            count += 1;
        }
        self.declared_work = end;
        self.inflight += 1;
        Some(Shard { start, end, count, class: 0, anchor: 0, rerun: None })
    }

    /// The shard's names, each followed by its NUL.
    #[must_use]
    pub fn names(&self, shard: Shard) -> &[u8] {
        &self.names[shard.start..shard.end]
    }

    /// Finds `name` in `shard`, trying the byte offset `expected` first because libtest reports in listing order.
    #[must_use]
    pub fn locate(&self, shard: Shard, expected: (usize, usize), name: &[u8]) -> Option<Located> {
        let (offset, ordinal) = expected;
        let at = |from: usize| self.names[from..shard.end].split(|byte| *byte == 0).next();
        if offset < shard.end && at(offset) == Some(name) {
            return Some(Located { ordinal, next: offset + name.len() + 1 });
        }
        let mut position = shard.start;
        for ordinal in 0..shard.count {
            let found = at(position)?;
            if found == name {
                return Some(Located { ordinal, next: position + name.len() + 1 });
            }
            position += found.len() + 1;
        }
        None
    }

    /// The name at `ordinal` of `shard`.
    #[must_use]
    pub fn nth(&self, shard: Shard, ordinal: usize) -> Option<&[u8]> {
        self.names(shard).split(|byte| *byte == 0).take(shard.count).nth(ordinal)
    }

    /// Records that a launched shard has fully finished.
    pub fn finished(&mut self, shard: Shard) {
        self.inflight = self.inflight.saturating_sub(1);
        self.solos = self.solos.saturating_sub(usize::from(shard.rerun.is_some()));
    }

    /// Drops every name not yet launched and every queued retry for an interrupted run.
    /// Returns how many names were uncounted; an output-only rerun's name is already counted, so it is dropped without being returned.
    pub fn discard_everything(&mut self) -> usize {
        let queued = self.discard_retries(|_| true);
        let dropped = self.unlaunched + queued;
        self.cursor = self.length;
        self.unlaunched = 0;
        dropped
    }

    /// Pops the next queued shard of a stopped run and returns how many uncounted names it held: a deciding retry first, else up to one bound of unlaunched names.
    /// Output-only reruns stay queued: their names were already counted Failed at their shard line.
    pub fn pop_queued(&mut self) -> Option<usize> {
        let deciding = self.retry.iter_mut().find(|slot| slot.as_ref().is_some_and(|pending| pending.decides));
        if let Some(slot) = deciding {
            return slot.take().map(|pending| pending.owed.total());
        }
        if self.unlaunched == 0 {
            return None;
        }
        let popped = self.unlaunched.min(self.bound.max(1));
        self.unlaunched -= popped;
        if self.unlaunched == 0 {
            self.cursor = self.length;
        }
        Some(popped)
    }

    // returns the names of the dropped retries that are uncounted, so an already-counted output rerun never adds to the total
    fn discard_retries(&mut self, dropping: impl Fn(&Pending) -> bool) -> usize {
        let mut uncounted = 0;
        for slot in &mut self.retry {
            if slot.as_ref().is_some_and(&dropping) {
                uncounted += slot.take().map_or(0, |pending| if pending.decides { pending.owed.total() } else { 0 });
            }
        }
        uncounted
    }

    /// Empties the arena for the next batch of names once every shard has finished.
    pub fn reset(&mut self) {
        let main = self.half - self.reserve;
        let carried = core::mem::take(&mut self.carry);
        self.names.copy_within(main..main + carried, 0);
        self.length = carried;
        self.count = usize::from(carried != 0);
        self.cursor = 0;
        self.unlaunched = self.count;
        self.sealed = false;
        self.retry = [None; SHARD_RETRY_SLOTS];
        self.declared_work = self.declared_base;
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::{PushError, Rerun, Shard, ShardArena, command_units};

    const LISTING: &str = include_str!("../tests/fixtures/listing/ignore_modes_list.txt");

    fn listed_names() -> Vec<&'static str> {
        LISTING.lines().filter_map(|line| line.strip_suffix(": test")).collect()
    }

    fn arena_with<'storage>(storage: &'storage mut [u8], names: &[&str]) -> ShardArena<'storage> {
        let mut arena = ShardArena::new(storage);
        for name in names {
            arena.push(name).expect("fixture names fit the arena");
        }
        arena
    }

    fn carved_counts(arena: &mut ShardArena<'_>) -> Vec<usize> {
        let mut counts = Vec::new();
        while let Some(shard) = arena.carve(64) {
            counts.push(shard.count);
        }
        counts
    }

    #[proxima::test]
    async fn shard_size_is_ceil_of_names_over_jobs_so_a_binary_launches_about_jobs_children() {
        let table = [(13_usize, 4_usize, [4_usize, 4, 4, 1].as_slice()), (3, 4, &[1, 1, 1]), (8, 2, &[4, 4]), (5, 1, &[5])];
        for (names, jobs, expected) in table {
            let listed: Vec<std::string::String> = (0..names).map(|index| std::format!("suite::case_{index:04}")).collect();
            let borrowed: Vec<&str> = listed.iter().map(std::string::String::as_str).collect();
            let mut storage = std::vec![0_u8; 4096];
            let mut arena = arena_with(&mut storage, &borrowed);

            arena.seal(jobs);

            assert_eq!(carved_counts(&mut arena), expected, "{names} names, {jobs} jobs");
        }
    }

    #[proxima::test]
    async fn real_listing_names_round_trip_through_a_shard_in_listing_order() {
        let names = listed_names();
        let mut storage = std::vec![0_u8; 1024];
        let mut arena = arena_with(&mut storage, &names);
        arena.seal(1);

        let shard = arena.carve(0).expect("sealed arena carves a shard");
        let joined: Vec<&[u8]> = arena.names(shard).split(|byte| *byte == 0).filter(|name| !name.is_empty()).collect();

        assert_eq!(joined, names.iter().map(|name| name.as_bytes()).collect::<Vec<_>>());
        assert_eq!(shard.count, 3);
    }

    #[proxima::test]
    async fn long_names_split_a_shard_at_the_command_line_budget() {
        let long: Vec<std::string::String> = (0..40).map(|index| std::format!("{}::case_{index:02}", "module_".repeat(150))).collect();
        let borrowed: Vec<&str> = long.iter().map(std::string::String::as_str).collect();
        let mut storage = std::vec![0_u8; 128 * 1024];
        let mut arena = arena_with(&mut storage, &borrowed);
        arena.seal(1);

        let mut units = Vec::new();
        while let Some(shard) = arena.carve(300) {
            units.push(arena.names(shard).split(|byte| *byte == 0).take(shard.count).map(command_units).sum::<usize>() + 300);
        }

        assert!(units.len() > 1, "one job still needs several shards for 40 names of ~1000 units");
        assert!(units.iter().all(|total| *total <= crate::build_defaults::SHARD_COMMAND_UNITS), "{units:?}");
    }

    #[proxima::test]
    async fn shard_size_never_exceeds_the_name_count_build_setting_even_for_one_job() {
        let total = crate::build_defaults::SHARD_NAMES * 2 + 5;
        let mut storage = std::vec![0_u8; total * 8];
        let mut arena = ShardArena::new(&mut storage);
        (0..total).for_each(|_| arena.push("a").expect("short names fit"));
        arena.seal(1);

        let counts = carved_counts_with(&mut arena, 0);

        assert_eq!(counts, [crate::build_defaults::SHARD_NAMES, crate::build_defaults::SHARD_NAMES, 5]);
    }

    fn carved_counts_with(arena: &mut ShardArena<'_>, fixed_units: usize) -> Vec<usize> {
        let mut counts = Vec::new();
        while let Some(shard) = arena.carve(fixed_units) {
            counts.push(shard.count);
        }
        counts
    }

    #[proxima::test]
    async fn units_count_quoting_for_names_a_windows_command_line_must_escape() {
        assert_eq!(command_units(b"plain::name"), 12);
        assert_eq!(command_units(b"has space"), 21);
        assert_eq!(command_units("naive::caf\u{e9}".as_bytes()), 12);
    }

    #[proxima::test]
    async fn push_refuses_a_name_past_the_storage_and_a_name_with_nul() {
        let mut storage = std::vec![0_u8; 80];
        let mut arena = ShardArena::new(&mut storage);

        assert_eq!(arena.push("a\0b"), Err(PushError::InteriorNul));
        assert_eq!(arena.push(&"m".repeat(74)), Ok(()), "74 bytes and a NUL fill the 75-byte main area");
        assert_eq!(arena.push(&"x".repeat(5)), Err(PushError::TooLong), "a name past the five-byte reserve cannot be parked");
        assert_eq!(arena.push("tail"), Err(PushError::Full), "the overflowing name is parked, not lost");
        assert!(arena.carrying());
    }

    #[proxima::test]
    async fn a_parked_name_leads_the_next_batch_after_reset() {
        let mut storage = std::vec![0_u8; 80];
        let mut arena = ShardArena::new(&mut storage);
        arena.push(&"m".repeat(74)).expect("fits the main area");
        assert_eq!(arena.push("tail"), Err(PushError::Full));
        arena.seal(1);
        let first = arena.carve(0).expect("sealed batch carves");
        arena.finished(first);
        assert_eq!(first.count, 1);

        arena.reset();
        arena.seal(1);

        let next = arena.carve(0).expect("parked name is the next batch");
        assert_eq!(arena.names(next), b"tail\0");
        assert!(!arena.carrying());
    }

    #[proxima::test]
    async fn locate_hits_the_expected_offset_first_and_falls_back_to_a_scan() {
        let mut storage = std::vec![0_u8; 256];
        let mut arena = arena_with(&mut storage, &["m::a", "m::b", "m::c"]);
        arena.seal(1);
        let shard = arena.carve(0).expect("one shard");

        let in_order = arena.locate(shard, (shard.start, 0), b"m::a").expect("first name");
        let out_of_order = arena.locate(shard, (shard.start, 0), b"m::c").expect("scan finds the third");
        let missing = arena.locate(shard, (shard.start, 0), b"m::zzz");

        assert_eq!((in_order.ordinal, out_of_order.ordinal, missing), (0, 2, None));
        assert_eq!(arena.nth(shard, 1), Some(b"m::b".as_slice()));
    }

    fn owed_all(count: usize) -> (super::Reported, super::Reported) {
        let mut owed = super::Reported::new();
        (0..count).for_each(|ordinal| _ = owed.mark(ordinal));
        (owed, super::Reported::new())
    }

    #[proxima::test]
    async fn owed_names_are_carved_one_solo_each_from_the_shards_own_bytes_and_idle_waits_for_them() {
        let mut storage = std::vec![0_u8; 512];
        let mut arena = arena_with(&mut storage, &["m::a", "m::b", "m::c"]);
        arena.seal(1);
        let shard = arena.carve(0).expect("one shard");
        arena.finished(shard);
        arena.owe_reruns(shard, owed_all(3), true).expect("a record slot is free");

        let solos: Vec<Shard> = std::iter::from_fn(|| arena.carve(0)).collect();

        let names: Vec<&[u8]> = solos.iter().map(|solo| arena.names(*solo)).collect();
        assert_eq!(names, [b"m::a\0".as_slice(), b"m::b\0", b"m::c\0"]);
        assert!(solos.iter().all(|solo| solo.count == 1 && matches!(solo.rerun, Some(Rerun::Decides { .. })) && solo.start >= shard.start && solo.end <= shard.end));
        assert!(!arena.idle(), "three solos are in flight");
        for solo in &solos {
            arena.finished(*solo);
        }
        assert!(arena.idle());
    }

    #[proxima::test]
    async fn a_shard_launches_only_while_a_record_slot_is_reserved_for_it() {
        let slots = crate::build_defaults::SHARD_RETRY_SLOTS;
        let names: Vec<std::string::String> = (0..=slots).map(|index| std::format!("m::t{index}")).collect();
        let mut storage = std::vec![0_u8; 8192];
        let mut arena = ShardArena::new(&mut storage);
        for name in &names {
            arena.push(name).expect("fits");
        }
        arena.seal(names.len());

        let launched: Vec<Shard> = std::iter::from_fn(|| arena.carve(0)).collect();

        assert_eq!(launched.len(), slots, "the shard past the slot count waits instead of launching");
        arena.finished(launched[0]);
        arena.owe_reruns(launched[0], owed_all(1), true).expect("the finished shard's slot is reserved");
        assert_eq!(
            arena.carve(0).map(|solo| solo.rerun),
            Some(Some(Rerun::Decides { after_shard_failure: false })),
            "a solo drains before a fresh shard may launch"
        );
    }

    #[proxima::test]
    async fn only_output_reruns_are_taken_by_carve_rerun_and_stopped_discard_keeps_them() {
        let mut storage = std::vec![0_u8; 256];
        let mut arena = arena_with(&mut storage, &["m::a", "m::b", "m::c"]);
        arena.seal(1);
        let shard = arena.carve(0).expect("one shard");
        arena.finished(shard);
        let (mut owed, failed) = owed_all(0);
        owed.mark(1);
        arena.owe_reruns(shard, (owed, failed), false).expect("a slot is free");
        arena.owe_reruns(shard, owed_all(2), true).expect("a slot is free");

        assert_eq!(std::iter::from_fn(|| arena.pop_queued()).sum::<usize>(), 2, "only the deciding record's two names were uncounted");
        let rerun = arena.carve_rerun().expect("the output rerun survived");

        assert_eq!(arena.names(rerun), b"m::b\0");
        assert_eq!((rerun.count, rerun.rerun), (1, Some(Rerun::OutputOnly)));
        assert!(arena.carve_rerun().is_none());
    }

    #[proxima::test]
    async fn reset_clears_owed_reruns_for_the_next_batch() {
        let mut storage = std::vec![0_u8; 256];
        let mut arena = arena_with(&mut storage, &["m::a", "m::b"]);
        arena.seal(1);
        let shard = arena.carve(0).expect("one shard");
        arena.finished(shard);
        arena.owe_reruns(shard, owed_all(2), true).expect("a slot is free");
        arena.discard_everything();
        arena.reset();

        assert!(arena.idle() && !arena.sealed() && arena.count() == 0);
    }
}
