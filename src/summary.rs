//! Per-binary and total count lines, and the accounting invariant between them.

pub const MAX_TARGETS: usize = 512;
pub const ID_CAPACITY: usize = 128;
pub const LINE_CAPACITY: usize = ID_CAPACITY + 160;

/// What one test binary (or the whole run) did with each test name it listed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub discovered: usize,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub filtered: usize,
}

impl Counts {
    /// Whether every discovered name ended as passed, failed, ignored, or filtered.
    #[must_use]
    pub const fn balanced(&self) -> bool {
        self.discovered == self.passed + self.failed + self.ignored + self.filtered
    }

    #[must_use]
    pub const fn plus(&self, other: &Self) -> Self {
        Self {
            discovered: self.discovered + other.discovered,
            passed: self.passed + other.passed,
            failed: self.failed + other.failed,
            ignored: self.ignored + other.ignored,
            filtered: self.filtered + other.filtered,
        }
    }
}

#[derive(Clone, Copy)]
struct Entry {
    id: [u8; ID_CAPACITY],
    id_length: usize,
    counts: Counts,
}

/// Counters per test binary, indexed by listing order; storage is fixed.
pub struct TargetTable {
    entries: [Entry; MAX_TARGETS],
    count: usize,
}

/// Which counter a test name moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tally {
    Launched,
    Filtered,
    Ignored,
    Passed,
    Failed,
    Skipped,
}

impl TargetTable {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [Entry {
                id: [0; ID_CAPACITY],
                id_length: 0,
                counts: Counts {
                    discovered: 0,
                    passed: 0,
                    failed: 0,
                    ignored: 0,
                    filtered: 0,
                },
            }; MAX_TARGETS],
            count: 0,
        }
    }

    /// Registers the next binary under `package` for a unit-test target or `package::binary`; false when storage is full or the id is too long.
    pub fn begin(&mut self, package: &str, binary: Option<&str>) -> bool {
        let separator = if binary.is_some() { 2 } else { 0 };
        let needed = package.len() + separator + binary.map_or(0, str::len);
        if self.count == MAX_TARGETS || needed > ID_CAPACITY {
            return false;
        }
        let entry = &mut self.entries[self.count];
        entry.id[..package.len()].copy_from_slice(package.as_bytes());
        if let Some(binary) = binary {
            entry.id[package.len()..package.len() + 2].copy_from_slice(b"::");
            entry.id[package.len() + 2..needed].copy_from_slice(binary.as_bytes());
        }
        entry.id_length = needed;
        self.count += 1;
        true
    }

    pub fn tally(&mut self, target: usize, what: Tally) {
        let Some(entry) = self.entries[..self.count].get_mut(target) else {
            return;
        };
        let counts = &mut entry.counts;
        match what {
            Tally::Launched => counts.discovered += 1,
            Tally::Filtered => {
                counts.discovered += 1;
                counts.filtered += 1;
            }
            Tally::Ignored => {
                counts.discovered += 1;
                counts.ignored += 1;
            }
            Tally::Passed => counts.passed += 1,
            Tally::Failed => counts.failed += 1,
            Tally::Skipped => counts.ignored += 1,
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[must_use]
    pub fn id(&self, target: usize) -> &[u8] {
        self.entries
            .get(target)
            .map_or(&[][..], |entry| &entry.id[..entry.id_length])
    }

    #[must_use]
    pub fn counts(&self, target: usize) -> Counts {
        self.entries.get(target).map_or_else(Counts::default, |entry| entry.counts)
    }

    #[must_use]
    pub fn total(&self) -> Counts {
        self.entries[..self.count]
            .iter()
            .fold(Counts::default(), |sum, entry| sum.plus(&entry.counts))
    }

    /// The first binary whose counters do not balance.
    #[must_use]
    pub fn first_unbalanced(&self) -> Option<usize> {
        (0..self.count).find(|target| !self.counts(*target).balanced())
    }
}

impl Default for TargetTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A fixed-capacity line builder.
pub struct Rendered {
    bytes: [u8; LINE_CAPACITY],
    length: usize,
}

impl Rendered {
    fn text(&mut self, text: &[u8]) {
        let taken = text.len().min(LINE_CAPACITY - self.length);
        self.bytes[self.length..self.length + taken].copy_from_slice(&text[..taken]);
        self.length += taken;
    }

    fn number(&mut self, value: usize) {
        let mut digits = [0; 20];
        let mut start = digits.len();
        let mut remaining = value;
        loop {
            start -= 1;
            digits[start] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        self.text(&digits[start..]);
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

/// `<label>: discovered N, passed P, failed F, ignored I, filtered X`, plus `, wall S.mmms` for the total line.
#[must_use]
pub fn render(label: &[u8], counts: &Counts, wall_millis: Option<u64>) -> Rendered {
    let mut line = Rendered {
        bytes: [0; LINE_CAPACITY],
        length: 0,
    };
    line.text(label);
    for (name, value) in [
        (&b": discovered "[..], counts.discovered),
        (b", passed ", counts.passed),
        (b", failed ", counts.failed),
        (b", ignored ", counts.ignored),
        (b", filtered ", counts.filtered),
    ] {
        line.text(name);
        line.number(value);
    }
    if let Some(millis) = wall_millis {
        line.text(b", wall ");
        line.number(usize::try_from(millis / 1000).unwrap_or(usize::MAX));
        line.text(b".");
        let fraction = usize::try_from(millis % 1000).unwrap_or(0);
        for place in [100, 10, 1] {
            line.number(fraction / place % 10);
        }
        line.text(b"s");
    }
    line.text(b"\n");
    line
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;
    use std::string::String;

    use super::{Counts, Tally, TargetTable, render};

    fn text(label: &str, counts: &Counts, wall: Option<u64>) -> String {
        String::from_utf8(render(label.as_bytes(), counts, wall).bytes().to_vec()).expect("ascii")
    }

    #[test]
    fn fixture_lines_have_the_exact_documented_shape() {
        let counts = Counts {
            discovered: 3,
            passed: 1,
            failed: 1,
            ignored: 1,
            filtered: 0,
        };
        assert_eq!(
            text("cli_run", &counts, None),
            "cli_run: discovered 3, passed 1, failed 1, ignored 1, filtered 0\n"
        );
        assert_eq!(
            text("total", &counts, Some(1234)),
            "total: discovered 3, passed 1, failed 1, ignored 1, filtered 0, wall 1.234s\n"
        );
        assert_eq!(text("t", &Counts::default(), Some(7)), "t: discovered 0, passed 0, failed 0, ignored 0, filtered 0, wall 0.007s\n");
    }

    #[test]
    fn a_launched_name_that_never_finishes_is_unbalanced_and_named() {
        let mut table = Box::new(TargetTable::new());
        assert!(table.begin("cli_run", None));
        assert!(table.begin("cli_run", Some("extra")));
        table.tally(0, Tally::Launched);
        table.tally(0, Tally::Passed);
        table.tally(1, Tally::Filtered);
        table.tally(1, Tally::Ignored);
        table.tally(1, Tally::Launched);
        assert_eq!(table.first_unbalanced(), Some(1));
        assert_eq!(table.id(1), b"cli_run::extra");
        table.tally(1, Tally::Failed);
        assert_eq!(table.first_unbalanced(), None);
        assert_eq!(table.total().discovered, 4);
    }

    #[test]
    fn an_over_long_id_is_refused() {
        let mut table = Box::new(TargetTable::new());
        let long = "x".repeat(129);
        assert!(!table.begin(&long, None));
        assert!(table.is_empty());
    }
}
