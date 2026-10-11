use crate::config::IgnoreMode;
use crate::filterset::{Candidate, Prepared};
use crate::ignored_set::IgnoredSet;

/// What the selection decides for one listed test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Launch the exact test.
    Launch,
    /// An excluded name: positional filter, filterset, or `--ignored` leaving out a regular test.
    Filtered,
    /// An `#[ignore]`d test that this run reports without launching.
    Ignored,
}

/// Everything that selects tests inside one test binary, applied as its listing streams in.
#[derive(Debug, Clone, Copy)]
pub struct Selection<'filter> {
    /// the positional filter text; empty admits everything.
    pub filter: &'filter str,
    /// whether the positional filter must equal the whole test name.
    pub exact: bool,
    /// the `-E` expression, when one was given.
    pub filterset: Option<&'filter Prepared<'filter>>,
    /// package name such as `proxima-model-interop`.
    pub package: &'filter str,
    /// cargo target name such as `capability_matrix`.
    pub binary: &'filter str,
    /// what to do with `#[ignore]`d tests.
    pub mode: IgnoreMode,
    /// the binary's `#[ignore]`d names; consulted unless every test runs.
    pub ignored: &'filter IgnoredSet,
}

/// Test-name matching strategy paired with its filter text.
#[derive(Debug, Clone, Copy)]
pub enum NameFilter<'filter> {
    /// Admit names that contain the filter text.
    Substring(&'filter str),
    /// Admit only names equal to the filter text.
    Exact(&'filter str),
    /// The full per-binary selection.
    Selected(&'filter Selection<'filter>),
}

impl NameFilter<'_> {
    /// Whether `name` is selected for launch.
    #[must_use]
    pub fn admits(&self, name: &str) -> bool {
        self.disposition(name) == Disposition::Launch
    }

    /// What to do with the listed `name`.
    #[must_use]
    pub fn disposition(&self, name: &str) -> Disposition {
        match self {
            Self::Substring(filter) if name.contains(filter) => Disposition::Launch,
            Self::Exact(filter) if name == *filter => Disposition::Launch,
            Self::Substring(_) | Self::Exact(_) => Disposition::Filtered,
            Self::Selected(selection) => selection.disposition(name),
        }
    }
}

impl Selection<'_> {
    fn admits_name(&self, name: &str) -> bool {
        if self.exact {
            name == self.filter
        } else {
            name.contains(self.filter)
        }
    }

    /// Positional filter first, then the filterset, then the ignore mode.
    #[must_use]
    pub fn disposition(&self, name: &str) -> Disposition {
        let candidate = Candidate {
            package: self.package,
            binary: self.binary,
            test: name,
        };
        let in_selection = self.admits_name(name)
            && self
                .filterset
                .is_none_or(|filterset| filterset.matches(&candidate));
        if !in_selection {
            return Disposition::Filtered;
        }
        let ignored = self.mode != IgnoreMode::Include && self.ignored.contains(name.as_bytes());
        match (self.mode, ignored) {
            (IgnoreMode::Skip, true) => Disposition::Ignored,
            (IgnoreMode::Only, false) => Disposition::Filtered,
            _ => Disposition::Launch,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use super::{Disposition, NameFilter, Selection};
    use crate::config::IgnoreMode;
    use crate::filterset::{Filterset, Prepared};
    use crate::ignored_set::IgnoredSet;

    #[test]
    fn name_filter_admits_substring_and_exact_by_their_own_rule() {
        assert!(NameFilter::Substring("alpha").admits("alpha"));
        assert!(NameFilter::Substring("alpha").admits("alphabet"));
        assert!(!NameFilter::Substring("alpha").admits("beta"));
        assert!(NameFilter::Exact("alpha").admits("alpha"));
        assert!(!NameFilter::Exact("alpha").admits("alphabet"));
        assert!(!NameFilter::Exact("alpha").admits("beta"));
        assert!(NameFilter::Substring("").admits("anything::at_all"));
        std::println!("{{\"substring_admitted\":3,\"exact_admitted\":1,\"exact_nonmatch\":2}}");
    }

    #[test]
    fn disposition_follows_libtest_ignore_semantics_and_the_filterset() {
        let mut ignored = Box::new(IgnoredSet::new());
        ignored.insert(b"fixture::ignored_by_default").expect("one name fits");
        let parsed = Filterset::parse(b"not test(skipped_by_expression)").expect("expression parses");
        let filterset = parsed.prepare();
        let selection = |mode, filterset| Selection {
            filter: "",
            exact: false,
            filterset,
            package: "cli_run",
            binary: "cli_run",
            mode,
            ignored: &ignored,
        };
        let names = [
            "fixture::passes",
            "fixture::ignored_by_default",
            "fixture::skipped_by_expression",
        ];
        let table: [(IgnoreMode, Option<&Prepared<'_>>, [Disposition; 3]); 4] = [
            (IgnoreMode::Skip, None, [Disposition::Launch, Disposition::Ignored, Disposition::Launch]),
            (IgnoreMode::Only, None, [Disposition::Filtered, Disposition::Launch, Disposition::Filtered]),
            (IgnoreMode::Include, None, [Disposition::Launch, Disposition::Launch, Disposition::Launch]),
            (IgnoreMode::Skip, Some(&filterset), [Disposition::Launch, Disposition::Ignored, Disposition::Filtered]),
        ];
        for (mode, expression, expected) in table {
            let chosen = selection(mode, expression);
            for (name, want) in names.iter().zip(expected) {
                assert_eq!(chosen.disposition(name), want, "{mode:?} {name}");
            }
        }
    }
}
