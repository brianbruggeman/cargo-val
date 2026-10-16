//! Per-binary and total count lines, and the accounting invariant between them.

use std::vec::Vec;

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

struct Entry {
    id: Vec<u8>,
    counts: Counts,
}

/// Counters per test binary, indexed by listing order; one entry per binary cargo reports, each id kept whole.
pub struct TargetTable {
    entries: Vec<Entry>,
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
    /// A launched test that a stopped run never ran: it leaves the discovered count it entered at launch.
    Withdrawn,
}

impl TargetTable {
    #[must_use]
    pub const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Registers the next binary under `package` for a unit-test target or `package::binary`.
    pub fn begin(&mut self, package: &str, binary: Option<&str>) {
        let mut id = Vec::from(package.as_bytes());
        if let Some(binary) = binary {
            id.extend_from_slice(b"::");
            id.extend_from_slice(binary.as_bytes());
        }
        self.entries.push(Entry { id, counts: Counts::default() });
    }

    pub fn tally(&mut self, target: usize, what: Tally) {
        let Some(entry) = self.entries.get_mut(target) else {
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
            Tally::Withdrawn => counts.discovered = counts.discovered.saturating_sub(1),
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    #[must_use]
    pub fn id(&self, target: usize) -> &[u8] {
        self.entries
            .get(target)
            .map_or(&[][..], |entry| entry.id.as_slice())
    }

    #[must_use]
    pub fn counts(&self, target: usize) -> Counts {
        self.entries.get(target).map_or_else(Counts::default, |entry| entry.counts)
    }

    #[must_use]
    pub fn total(&self) -> Counts {
        self.entries
            .iter()
            .fold(Counts::default(), |sum, entry| sum.plus(&entry.counts))
    }

    /// The first binary whose counters do not balance.
    #[must_use]
    pub fn first_unbalanced(&self) -> Option<usize> {
        (0..self.entries.len()).find(|target| !self.counts(*target).balanced())
    }
}

impl Default for TargetTable {
    fn default() -> Self {
        Self::new()
    }
}

/// One count line, sized by its label.
pub struct Rendered {
    bytes: Vec<u8>,
}

impl Rendered {
    fn text(&mut self, text: &[u8]) {
        self.bytes.extend_from_slice(text);
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
        &self.bytes
    }
}

/// `<label>: discovered N, passed P, failed F, ignored I, filtered X`, plus `, wall S.mmms` for the total line.
#[must_use]
pub fn render(label: &[u8], counts: &Counts, wall_millis: Option<u64>) -> Rendered {
    let mut line = Rendered { bytes: Vec::new() };
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

    #[proxima::test]
    async fn fixture_lines_have_the_exact_documented_shape() {
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

    #[proxima::test]
    async fn a_launched_name_that_never_finishes_is_unbalanced_and_named() {
        let mut table = Box::new(TargetTable::new());
        table.begin("cli_run", None);
        table.begin("cli_run", Some("extra"));
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

    #[proxima::test]
    async fn an_id_and_a_table_of_any_size_are_kept_whole() {
        let mut table = Box::new(TargetTable::new());
        let long = "x".repeat(2000);
        for _ in 0..1000 {
            table.begin(&long, Some("serve"));
        }
        assert_eq!(table.len(), 1000);
        assert_eq!(table.id(999).len(), 2000 + 2 + 5);
    }
}
