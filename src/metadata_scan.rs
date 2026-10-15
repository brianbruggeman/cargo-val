//! Each package's targets, which are workspace members, which the resolve graph reaches, plus the workspace root and target directory, read from
//! `cargo metadata --filter-platform <triple> --format-version 1` one byte at a time over fixed storage, so the scan allocates only its package table
//! once and keeps no whole-document buffer.

use std::boxed::Box;
use std::vec;

const MAX_PACKAGES: usize = 4096;
const ID_CAPACITY: usize = 192;
const MAX_DEPTH: usize = 8;
/// Longest `target_directory` or `workspace_root` the scan keeps; a longer path makes it report none.
pub const PATH_CAPACITY: usize = 512;
const TEXT_CAPACITY: usize = PATH_CAPACITY;

/// Why the metadata could not be used; the caller falls back to waiting for `build-finished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFault {
    /// Nesting passed the fixed stack, so the document is not cargo's metadata shape.
    TooDeep,
    /// More packages with bins than the fixed table holds, or an id longer than it holds.
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Other,
    Packages,
    Id,
    Targets,
    Kind,
    Name,
    Test,
    Doctest,
    DefaultMembers,
    TargetDirectory,
    WorkspaceRoot,
    WorkspaceMembers,
    Resolve,
    Nodes,
    Deps,
    Pkg,
    DepKinds,
}

const LIB: u8 = 1;
const BIN: u8 = 2;
const TEST: u8 = 4;
const EXAMPLE: u8 = 8;
const BENCH: u8 = 16;
const UNTESTED: u8 = 32;
const NODOCTEST: u8 = 64;
const LIBRARY_KINDS: [&[u8]; 6] = [b"lib", b"rlib", b"dylib", b"cdylib", b"staticlib", b"proc-macro"];
const TARGET_SELECTORS: [&[u8]; 10] = [
    b"--lib", b"--bins", b"--bin", b"--tests", b"--test", b"--examples", b"--example", b"--benches", b"--bench", b"--all-targets",
];

fn flags_selector(arguments: &[&[u8]], selector: &[u8]) -> bool {
    arguments.iter().any(|argument| *argument == selector || argument.strip_prefix(selector).is_some_and(|rest| rest.first() == Some(&b'=')))
}

fn has_flag(arguments: &[&[u8]], flag: &[u8]) -> bool {
    arguments.contains(&flag)
}

// the values given to a repeatable cargo option, in `--long value`, `--long=value` and `-s value` forms
fn option_values<'a>(arguments: &'a [&'a [u8]], long: &[u8], short: &[u8]) -> impl Iterator<Item = &'a [u8]> {
    arguments.iter().enumerate().filter_map(move |(index, argument)| {
        if *argument == long || (!short.is_empty() && *argument == short) {
            arguments.get(index + 1).copied()
        } else {
            argument.strip_prefix(long).and_then(|rest| rest.strip_prefix(b"="))
        }
    })
}

// a package spec is a name, optionally followed by `@version`
fn spec_names(spec: &[u8], name: &[u8]) -> bool {
    spec.split(|byte| *byte == b'@').next() == Some(name)
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    object: bool,
    expect_key: bool,
    key: Key,
}

/// What one package's targets contribute to a test build, counted once per target.
#[derive(Clone, Copy, Default)]
struct Targets {
    libs: u16,
    scripts: u16,
    tests: u16,
    examples: u16,
    selected_tests: u16,
    selected_bins: u16,
    doctested: u16,
}

#[derive(Clone, Copy)]
struct Entry {
    id: [u8; ID_CAPACITY],
    id_length: usize,
    bins: usize,
    built: usize,
    tests: usize,
    targets: Targets,
    named: bool,
    excluded: bool,
    default: bool,
    marks: u8,
}

const MEMBER: u8 = 1;
const REACHABLE: u8 = 2;
const SELECTED: u8 = 4;
const NEEDED: u8 = 8;
const DEPENDED: u8 = 16;

/// Every package the document lists: how many `bin` targets it has and how many were built so far, the test executables the run's flags select in it,
/// whether it is a workspace member and whether the resolve graph reaches it.
pub struct BinCounts {
    entries: Box<[Entry]>,
    count: usize,
    unselectable: bool,
    default_members_seen: bool,
    members_marked: bool,
    edges: vec::Vec<(usize, usize, bool)>,
}

impl BinCounts {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: vec![Entry {
                id: [0; ID_CAPACITY],
                id_length: 0,
                bins: 0,
                built: 0,
                tests: 0,
                targets: Targets::default(),
                named: false,
                excluded: false,
                default: false,
                marks: 0,
            }; MAX_PACKAGES].into_boxed_slice(),
            count: 0,
            unselectable: false,
            default_members_seen: false,
            members_marked: false,
            edges: vec::Vec::new(),
        }
    }

    // a document that lists workspace members (the empty list included in fixtures) filters to them; one without keeps every package
    fn is_member(&self, entry: &Entry) -> bool {
        entry.marks & MEMBER != 0 || !self.members_marked
    }

    fn find(&self, package_id: &[u8]) -> Option<usize> {
        self.entries[..self.count]
            .iter()
            .position(|entry| &entry.id[..entry.id_length] == package_id)
    }

    /// Records that one `bin` executable of `package_id` was built.
    pub fn note_built(&mut self, package_id: &str) {
        if let Some(index) = self.find(package_id.as_bytes()) {
            self.entries[index].built += 1;
        }
    }

    /// Whether `package_id` is a workspace member that still has a `bin` target that has not been built.
    #[must_use]
    pub fn awaiting(&self, package_id: &str) -> bool {
        self.find(package_id.as_bytes())
            .is_some_and(|index| self.is_member(&self.entries[index]) && self.entries[index].built < self.entries[index].bins)
    }

    /// Number of workspace members with at least one `bin` target.
    #[must_use]
    pub fn packages(&self) -> usize {
        self.entries[..self.count].iter().filter(|entry| self.is_member(entry) && entry.bins > 0).count()
    }

    fn mark(&mut self, id: &[u8], member: bool) {
        if let Some(index) = self.find(id) {
            self.entries[index].marks |= if member { MEMBER } else { REACHABLE };
        }
    }

    /// The directory of each workspace member whose id is a `path+file://` url, in document order.
    pub fn member_directories(&self) -> impl Iterator<Item = &[u8]> {
        self.entries[..self.count].iter().filter(|entry| self.is_member(entry)).filter_map(|entry| {
            let rest = entry.id[..entry.id_length].strip_prefix(b"path+file://")?;
            let end = memchr::memchr(b'#', rest).unwrap_or(rest.len());
            Some(&rest[..end])
        })
    }

    /// Compile units a `cargo test --no-run` is expected to report as compiler artifacts, from the resolve graph and the run's selection; `None` when the document had no resolve.
    /// Evidence for each rule is the artifact list `cargo test --no-run --message-format=json` printed for the `build_eta` fixture.
    /// A package the selection needs only as a dependency counts its library and build script. A selected member counts its build script, each selected test executable
    /// (library, bin and integration test harnesses, none for `test = false`), its normal library when a selected bin or integration test, an example or another needed package uses it,
    /// every bin when a selected integration test exists to need them, and each example. Resolver-2 host/target feature duplicates are not visible in the graph, so the count can fall short of the build.
    #[must_use]
    pub fn predicted_units(&self) -> Option<usize> {
        if self.unselectable || !self.entries[..self.count].iter().any(|entry| entry.marks & REACHABLE != 0) {
            return None;
        }
        let units = self.entries[..self.count].iter().filter(|entry| entry.marks & NEEDED != 0).map(Self::units_of).sum();
        Some(units)
    }

    fn units_of(entry: &Entry) -> usize {
        let targets = entry.targets;
        let libs = usize::from(targets.libs);
        let scripts = usize::from(targets.scripts);
        if entry.marks & SELECTED == 0 {
            return libs + scripts;
        }
        let integration = usize::from(targets.selected_tests);
        let library_needed = targets.doctested > 0 || integration > 0 || targets.selected_bins > 0 || targets.examples > 0 || entry.marks & DEPENDED != 0;
        let normal_lib = if library_needed { libs } else { 0 };
        let normal_bins = if integration > 0 { entry.bins } else { 0 };
        scripts + entry.tests + normal_lib + normal_bins + usize::from(targets.examples)
    }

    // grows the selection to everything it builds: a dependency edge counts unless it is a dev-dependency of a package the run does not test
    fn close_needed(&mut self) {
        loop {
            let mut changed = false;
            for index in 0..self.edges.len() {
                let (from, to, dev_only) = self.edges[index];
                let carries = self.entries[from].marks & NEEDED != 0 && (!dev_only || self.entries[from].marks & SELECTED != 0);
                if carries {
                    changed |= self.entries[to].marks & NEEDED == 0;
                    self.entries[to].marks |= NEEDED | DEPENDED;
                }
            }
            if !changed {
                return;
            }
        }
    }

    fn add(&mut self, id: &[u8], bins: usize) -> Result<(), ScanFault> {
        if self.count == MAX_PACKAGES || id.len() > ID_CAPACITY {
            return Err(ScanFault::TooLarge);
        }
        let entry = &mut self.entries[self.count];
        entry.id[..id.len()].copy_from_slice(id);
        entry.id_length = id.len();
        entry.bins = bins;
        entry.built = 0;
        self.count += 1;
        Ok(())
    }

    fn mark_default(&mut self, package_id: &[u8]) {
        if let Some(index) = self.find(package_id) {
            self.entries[index].default = true;
        }
    }
}

impl Default for BinCounts {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
struct PathText {
    bytes: [u8; PATH_CAPACITY],
    length: usize,
}

impl PathText {
    const fn new() -> Self {
        Self {
            bytes: [0; PATH_CAPACITY],
            length: 0,
        }
    }

    // a JSON escape means the raw bytes are not the path, so the path is reported as absent
    fn set(&mut self, text: &[u8], overflow: bool) {
        if overflow || text.contains(&b'\\') {
            self.length = 0;
            return;
        }
        self.bytes[..text.len()].copy_from_slice(text);
        self.length = text.len();
    }

    fn get(&self) -> Option<&[u8]> {
        (self.length > 0).then(|| &self.bytes[..self.length])
    }
}

/// What the metadata document held: bin counts per package and the two directories.
pub struct MetadataFacts {
    /// Per package with at least one `bin` target.
    pub bins: BinCounts,
    /// Test executables the run builds: the targets with `test = true` that the package and target flags select. `None` when the packages overflowed the fixed table.
    pub test_targets: Option<usize>,
    target_directory: PathText,
    workspace_root: PathText,
}

impl MetadataFacts {
    /// Cargo's resolved target directory (`--target-dir`, `CARGO_TARGET_DIR`, `build.target-dir`, else `<workspace>/target`).
    #[must_use]
    pub fn target_directory(&self) -> Option<&[u8]> {
        self.target_directory.get()
    }

    /// The workspace root directory.
    #[must_use]
    pub fn workspace_root(&self) -> Option<&[u8]> {
        self.workspace_root.get()
    }
}

/// Byte-fed scanner; feed the whole metadata document, then read [`Self::finish`].
pub struct MetadataScan<'a> {
    arguments: &'a [&'a [u8]],
    stack: [Frame; MAX_DEPTH],
    depth: usize,
    lexing: Lexing,
    text: [u8; TEXT_CAPACITY],
    text_length: usize,
    text_overflow: bool,
    package_id: [u8; ID_CAPACITY],
    package_id_length: usize,
    package_id_overflow: bool,
    package_bins: usize,
    package_tests: usize,
    package_targets: Targets,
    target_script: bool,
    node_index: Option<usize>,
    dependency_index: Option<usize>,
    dependency_kinds: u16,
    dependency_dev_kinds: u16,
    package_name: [u8; ID_CAPACITY],
    package_name_length: Option<usize>,
    target_kinds: u8,
    target_name: [u8; ID_CAPACITY],
    target_name_length: Option<usize>,
    counts: BinCounts,
    target_directory: PathText,
    workspace_root: PathText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lexing {
    Outside,
    InString,
    Escaped,
}

const FRAME: Frame = Frame {
    object: false,
    expect_key: false,
    key: Key::Other,
};

impl MetadataScan<'static> {
    /// A scan with no cargo flags: every workspace package, every target with `test = true`.
    #[must_use]
    pub fn new() -> Self {
        Self::selecting(&[])
    }
}

impl<'a> MetadataScan<'a> {
    /// A scan whose test count follows the cargo flags the run forwards: `-p`/`--package`, `--workspace`/`--all`, `--exclude`, and the target selectors (`--lib`, `--bins`, `--bin`, `--tests`, `--test`, `--examples`, `--example`, `--benches`, `--bench`, `--all-targets`).
    /// Doc tests are a separate lane and are never counted here.
    #[must_use]
    pub fn selecting(arguments: &'a [&'a [u8]]) -> Self {
        Self {
            arguments,
            stack: [FRAME; MAX_DEPTH],
            depth: 0,
            lexing: Lexing::Outside,
            text: [0; TEXT_CAPACITY],
            text_length: 0,
            text_overflow: false,
            package_id: [0; ID_CAPACITY],
            package_id_length: 0,
            package_id_overflow: false,
            package_bins: 0,
            package_tests: 0,
            package_targets: Targets::default(),
            target_script: false,
            node_index: None,
            dependency_index: None,
            dependency_kinds: 0,
            dependency_dev_kinds: 0,
            package_name: [0; ID_CAPACITY],
            package_name_length: Some(0),
            target_kinds: 0,
            target_name: [0; ID_CAPACITY],
            target_name_length: Some(0),
            counts: BinCounts::new(),
            target_directory: PathText::new(),
            workspace_root: PathText::new(),
        }
    }

    /// Consumes more document bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ScanFault`] when the document does not fit the fixed storage.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), ScanFault> {
        for byte in bytes {
            if self.lexing == Lexing::Outside {
                self.structural_byte(*byte)?;
            } else {
                self.string_byte(*byte);
            }
        }
        Ok(())
    }

    /// The facts, once the whole document was fed.
    #[must_use]
    pub fn finish(mut self) -> MetadataFacts {
        let test_targets = self.selected_test_targets();
        self.mark_selection();
        MetadataFacts {
            test_targets,
            bins: self.counts,
            target_directory: self.target_directory,
            workspace_root: self.workspace_root,
        }
    }

    // `--workspace` takes every member (less `--exclude`), else `-p` takes the named ones, else cargo's default members
    fn chosen(&self, entry: &Entry) -> bool {
        let workspace = has_flag(self.arguments, b"--workspace") || has_flag(self.arguments, b"--all");
        let named = option_values(self.arguments, b"--package", b"-p").next().is_some();
        self.counts.is_member(entry)
            && match (workspace, named) {
                (true, _) => !entry.excluded,
                (false, true) => entry.named,
                (false, false) => !self.counts.default_members_seen || entry.default,
            }
    }

    fn selected_test_targets(&self) -> Option<usize> {
        if self.counts.unselectable {
            return None;
        }
        let entries = &self.counts.entries[..self.counts.count];
        Some(entries.iter().filter(|entry| self.chosen(entry)).map(|entry| entry.tests).sum())
    }

    fn mark_selection(&mut self) {
        for index in 0..self.counts.count {
            if self.chosen(&self.counts.entries[index]) {
                self.counts.entries[index].marks |= SELECTED | NEEDED;
            }
        }
        self.counts.close_needed();
    }

    fn target_selected(&self) -> bool {
        let kinds = self.target_kinds & !(UNTESTED | NODOCTEST);
        let tested = self.target_kinds & UNTESTED == 0;
        let Some(name_length) = self.target_name_length else {
            return false;
        };
        if kinds == 0 {
            return false;
        }
        let arguments = self.arguments;
        let name = &self.target_name[..name_length];
        let flagged = |selector: &[u8]| flags_selector(arguments, selector);
        if !TARGET_SELECTORS.iter().any(|selector| flagged(selector)) {
            return tested;
        }
        let named = |long: &[u8], kind: u8| kinds & kind != 0 && option_values(arguments, long, b"").any(|value| value == name);
        let by_kind = |flag: &[u8], mask: u8| tested && kinds & mask != 0 && has_flag(arguments, flag);
        by_kind(b"--lib", LIB)
            || by_kind(b"--bins", BIN)
            || by_kind(b"--tests", u8::MAX)
            || by_kind(b"--all-targets", u8::MAX)
            || by_kind(b"--examples", EXAMPLE)
            || by_kind(b"--benches", BENCH)
            || (tested && (named(b"--bin", BIN) || named(b"--example", EXAMPLE) || named(b"--bench", BENCH)))
            || named(b"--test", TEST)
    }

    fn string_byte(&mut self, byte: u8) {
        if self.lexing == Lexing::InString && byte == b'"' {
            self.lexing = Lexing::Outside;
            self.string_done();
            return;
        }
        self.lexing = if self.lexing == Lexing::InString && byte == b'\\' {
            Lexing::Escaped
        } else {
            Lexing::InString
        };
        if self.text_length < TEXT_CAPACITY {
            self.text[self.text_length] = byte;
            self.text_length += 1;
        } else {
            self.text_overflow = true;
        }
    }

    fn top(&mut self) -> Option<&mut Frame> {
        self.depth.checked_sub(1).map(|index| &mut self.stack[index])
    }

    fn string_done(&mut self) {
        let text_length = self.text_length;
        let overflow = self.text_overflow;
        let depth = self.depth;
        let is_key = self.top().is_some_and(|frame| frame.object && frame.expect_key);
        if is_key {
            let key = match &self.text[..text_length] {
                b"packages" => Key::Packages,
                b"id" => Key::Id,
                b"targets" => Key::Targets,
                b"kind" => Key::Kind,
                b"name" => Key::Name,
                b"test" => Key::Test,
                b"doctest" => Key::Doctest,
                b"workspace_default_members" => Key::DefaultMembers,
                b"target_directory" => Key::TargetDirectory,
                b"workspace_root" => Key::WorkspaceRoot,
                b"workspace_members" => Key::WorkspaceMembers,
                b"resolve" => Key::Resolve,
                b"nodes" => Key::Nodes,
                b"deps" => Key::Deps,
                b"pkg" => Key::Pkg,
                b"dep_kinds" => Key::DepKinds,
                _ => Key::Other,
            };
            self.counts.default_members_seen |= key == Key::DefaultMembers && depth == 1;
            if let Some(frame) = self.top() {
                frame.key = key;
                frame.expect_key = false;
            }
        } else if depth == 1 && self.stack[0].object && self.stack[0].key == Key::TargetDirectory {
            self.target_directory.set(&self.text[..text_length], overflow);
        } else if depth == 1 && self.stack[0].object && self.stack[0].key == Key::WorkspaceRoot {
            self.workspace_root.set(&self.text[..text_length], overflow);
        } else if depth == 3 && self.stack[2].key == Key::Id && self.in_package() {
            self.package_id_overflow = overflow || text_length > ID_CAPACITY;
            if !self.package_id_overflow {
                self.package_id_length = text_length;
                self.package_id[..text_length].copy_from_slice(&self.text[..text_length]);
            }
        } else if depth == 3 && self.stack[2].key == Key::Name && self.in_package() {
            let fits = !overflow && text_length <= ID_CAPACITY;
            self.package_name_length = fits.then_some(text_length);
            if fits {
                self.package_name[..text_length].copy_from_slice(&self.text[..text_length]);
            }
        } else if depth == 5 && self.stack[4].key == Key::Name && self.in_target() {
            let fits = !overflow && text_length <= ID_CAPACITY;
            self.target_name_length = fits.then_some(text_length);
            if fits {
                self.target_name[..text_length].copy_from_slice(&self.text[..text_length]);
            }
        } else if depth == 2 && self.stack[0].object && self.stack[0].key == Key::DefaultMembers && !self.stack[1].object {
            let member = &self.text[..text_length];
            self.counts.mark_default(member);
        } else if depth == 2 && self.stack[0].key == Key::WorkspaceMembers && !self.stack[1].object {
            self.counts.members_marked = true;
            self.counts.mark(&self.text[..text_length], true);
        } else if depth == 4 && self.in_resolve_node() {
            self.counts.mark(&self.text[..text_length], false);
            self.node_index = self.counts.find(&self.text[..text_length]);
        } else if depth == 6 && self.in_dependency() && self.stack[5].key == Key::Pkg {
            self.dependency_index = self.counts.find(&self.text[..text_length]);
        } else if depth == 8 && self.in_dependency_kind() && self.stack[7].key == Key::Kind {
            self.dependency_dev_kinds += u16::from(&self.text[..text_length] == b"dev");
        } else if depth == 6 && self.in_kind_list() {
            self.count_kind(text_length);
        }
        self.text_length = 0;
        self.text_overflow = false;
    }

    fn count_kind(&mut self, text_length: usize) {
        let kind = &self.text[..text_length];
        if kind == b"bin" {
            self.package_bins += 1;
        }
        self.target_script |= kind == b"custom-build";
        self.target_kinds |= match kind {
            b"bin" => BIN,
            b"test" => TEST,
            b"example" => EXAMPLE,
            b"bench" => BENCH,
            library if LIBRARY_KINDS.contains(&library) => LIB,
            _ => 0,
        };
    }

    // cargo test runs a library's doc tests only when no target selector narrows the run
    fn narrowed_by_target_selectors(&self) -> bool {
        TARGET_SELECTORS.iter().any(|selector| flags_selector(self.arguments, selector))
    }

    // a target with several kinds (`lib` and `cdylib`) is one compile unit, so each counter moves once per target
    fn finish_target(&mut self) {
        let kinds = self.target_kinds;
        let narrowed = self.narrowed_by_target_selectors();
        let totals = &mut self.package_targets;
        totals.libs += u16::from(kinds & LIB != 0);
        totals.doctested += u16::from(kinds & LIB != 0 && kinds & NODOCTEST == 0 && !narrowed);
        totals.tests += u16::from(kinds & TEST != 0);
        totals.examples += u16::from(kinds & EXAMPLE != 0);
        totals.scripts += u16::from(self.target_script);
    }

    // root object, `resolve` object, `nodes` array, one node object
    fn in_resolve_node(&self) -> bool {
        self.depth == 4
            && self.stack[0].key == Key::Resolve
            && self.stack[1].object
            && self.stack[1].key == Key::Nodes
            && !self.stack[2].object
            && self.stack[3].object
            && self.stack[3].key == Key::Id
    }

    // package -> targets array -> target object
    fn in_target(&self) -> bool {
        self.in_package() && self.depth >= 5 && self.stack[2].key == Key::Targets && !self.stack[3].object && self.stack[4].object
    }

    // root object, its `packages` array, one package object
    fn in_package(&self) -> bool {
        self.depth >= 3
            && self.stack[0].object
            && self.stack[0].key == Key::Packages
            && !self.stack[1].object
            && self.stack[2].object
    }

    // package -> targets array -> target object -> kind array
    fn in_kind_list(&self) -> bool {
        self.in_package()
            && self.depth == 6
            && self.stack[2].key == Key::Targets
            && !self.stack[3].object
            && self.stack[4].object
            && self.stack[4].key == Key::Kind
            && !self.stack[5].object
    }

    // root object, `resolve` object, `nodes` array, one node object, its `deps` array, one dependency object
    fn in_dependency(&self) -> bool {
        self.depth >= 6
            && self.stack[0].key == Key::Resolve
            && self.stack[1].object
            && self.stack[1].key == Key::Nodes
            && !self.stack[2].object
            && self.stack[3].object
            && self.stack[3].key == Key::Deps
            && !self.stack[4].object
            && self.stack[5].object
    }

    // a dependency object -> its `dep_kinds` array -> one kind object
    fn in_dependency_kind(&self) -> bool {
        self.in_dependency() && self.depth >= 8 && self.stack[5].key == Key::DepKinds && !self.stack[6].object && self.stack[7].object
    }

    fn open(&mut self, object: bool) -> Result<(), ScanFault> {
        if self.depth == MAX_DEPTH {
            return Err(ScanFault::TooDeep);
        }
        self.stack[self.depth] = Frame {
            object,
            expect_key: object,
            key: Key::Other,
        };
        self.depth += 1;
        if object && self.depth == 5 && self.in_target() {
            self.target_kinds = 0;
            self.target_script = false;
            self.target_name_length = Some(0);
        }
        if object && self.depth == 6 && self.in_dependency() {
            self.dependency_index = None;
            self.dependency_kinds = 0;
            self.dependency_dev_kinds = 0;
        }
        if object && self.depth == 8 && self.in_dependency_kind() {
            self.dependency_kinds += 1;
        }
        if object && self.depth == 3 && self.in_package() {
            self.package_id_length = 0;
            self.package_id_overflow = false;
            self.package_bins = 0;
            self.package_tests = 0;
            self.package_targets = Targets::default();
            self.package_name_length = Some(0);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), ScanFault> {
        let closing_package = self.depth == 3 && self.in_package();
        if self.depth == 6 && self.in_dependency() {
            self.record_edge();
        }
        if self.depth == 5 && self.in_target() {
            self.finish_target();
            if self.target_selected() {
                self.package_tests += 1;
                self.package_targets.selected_tests += u16::from(self.target_kinds & TEST != 0);
                self.package_targets.selected_bins += u16::from(self.target_kinds & BIN != 0);
            }
        }
        self.depth = self.depth.saturating_sub(1);
        if closing_package {
            self.record_package()?;
        }
        Ok(())
    }

    // a dependency whose every kind entry is `dev` only matters to a package the run tests
    fn record_edge(&mut self) {
        if let (Some(from), Some(to)) = (self.node_index, self.dependency_index) {
            let dev_only = self.dependency_kinds > 0 && self.dependency_dev_kinds == self.dependency_kinds;
            self.counts.edges.push((from, to, dev_only));
        }
    }

    // a package with bins that cannot be stored refuses the document; one without only costs the test count
    fn record_package(&mut self) -> Result<(), ScanFault> {
        let id = self.package_id;
        let stored = if self.package_id_overflow { Err(ScanFault::TooLarge) } else { self.counts.add(&id[..self.package_id_length], self.package_bins) };
        if let Err(fault) = stored {
            self.counts.unselectable = true;
            return if self.package_bins > 0 { Err(fault) } else { Ok(()) };
        }
        let name = &self.package_name[..self.package_name_length.unwrap_or(0)];
        let matches = |long: &[u8], short: &[u8]| self.package_name_length.is_some() && option_values(self.arguments, long, short).any(|spec| spec_names(spec, name));
        let (named, excluded) = (matches(b"--package", b"-p"), matches(b"--exclude", b""));
        let entry = &mut self.counts.entries[self.counts.count - 1];
        entry.tests = self.package_tests;
        entry.targets = self.package_targets;
        entry.named = named;
        entry.excluded = excluded;
        Ok(())
    }

    fn structural_byte(&mut self, byte: u8) -> Result<(), ScanFault> {
        match byte {
            b'"' => {
                self.lexing = Lexing::InString;
            }
            b'{' => self.open(true)?,
            b'[' => self.open(false)?,
            b'}' | b']' => self.close()?,
            b'f' if self.depth == 5 && self.in_target() && self.stack[4].key == Key::Test => self.target_kinds |= UNTESTED,
            b'f' if self.depth == 5 && self.in_target() && self.stack[4].key == Key::Doctest => self.target_kinds |= NODOCTEST,
            b':' => {
                if let Some(frame) = self.top() {
                    frame.expect_key = false;
                }
            }
            b',' => {
                if let Some(frame) = self.top() {
                    frame.expect_key = frame.object;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Default for MetadataScan<'static> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use super::{MetadataScan, ScanFault};

    const TWO_PACKAGES: &str = r#"{"packages":[{"name":"parser","version":"0.3.1","id":"path+file:///repo/parser#0.3.1","dependencies":[{"name":"x","kind":null}],"targets":[{"kind":["lib"],"crate_types":["lib"],"name":"parser"},{"kind":["bin"],"crate_types":["bin"],"name":"parse"},{"kind":["bin"],"crate_types":["bin"],"name":"lint"},{"kind":["test"],"crate_types":["bin"],"name":"cli"}],"manifest_path":"/repo/parser/Cargo.toml"},{"name":"plain","version":"0.1.0","id":"path+file:///repo/plain#0.1.0","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"plain"},{"kind":["test"],"crate_types":["bin"],"name":"it"}]}],"workspace_members":["path+file:///repo/parser#0.3.1","path+file:///repo/plain#0.1.0"],"resolve":null,"target_directory":"/repo/target","version":1}"#;

    const WORKSPACE: &str = r#"{"packages":[{"name":"core","version":"0.2.0","id":"path+file:///w/core#0.2.0","targets":[{"kind":["lib","cdylib"],"crate_types":["lib","cdylib"],"name":"core","test":true},{"kind":["example"],"crate_types":["bin"],"name":"demo","test":false},{"kind":["test"],"crate_types":["bin"],"name":"it","test":true}]},{"name":"cli","version":"0.2.0","id":"path+file:///w/cli#0.2.0","dependencies":[{"name":"core","kind":null}],"targets":[{"kind":["lib"],"crate_types":["lib"],"name":"cli","test":true},{"kind":["bin"],"crate_types":["bin"],"name":"cli-main","test":true},{"kind":["bench"],"crate_types":["bin"],"name":"speed","test":false}]},{"name":"extra","version":"0.1.0","id":"path+file:///w/extra#0.1.0","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"extra","test":true}]}],"workspace_members":[],"workspace_default_members":["path+file:///w/core#0.2.0","path+file:///w/extra#0.1.0"],"resolve":null,"target_directory":"/w/target","version":1}"#;

    #[proxima::test]
    async fn counts_bin_targets_per_package_and_ignores_every_other_kind() {
        let mut scan = Box::new(MetadataScan::new());
        for chunk in TWO_PACKAGES.as_bytes().chunks(7) {
            scan.feed(chunk).expect("document fits");
        }
        let facts = scan.finish();
        let mut counts = facts.bins;

        assert_eq!(counts.packages(), 1);
        assert!(counts.awaiting("path+file:///repo/parser#0.3.1"));
        assert!(!counts.awaiting("path+file:///repo/plain#0.1.0"));
        counts.note_built("path+file:///repo/parser#0.3.1");
        assert!(counts.awaiting("path+file:///repo/parser#0.3.1"));
        counts.note_built("path+file:///repo/parser#0.3.1");
        assert!(!counts.awaiting("path+file:///repo/parser#0.3.1"));
    }

    #[proxima::test]
    async fn with_no_flags_the_default_members_test_executables_are_counted_once() {
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(WORKSPACE.as_bytes()).expect("document fits");

        assert_eq!(scan.finish().test_targets, Some(3));
    }

    #[proxima::test]
    async fn the_two_package_document_has_six_test_targets() {
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(TWO_PACKAGES.as_bytes()).expect("document fits");

        assert_eq!(scan.finish().test_targets, Some(6));
    }

    fn counted(arguments: &[&[u8]]) -> Option<usize> {
        let mut scan = Box::new(MetadataScan::selecting(arguments));
        for chunk in WORKSPACE.as_bytes().chunks(11) {
            scan.feed(chunk).expect("document fits");
        }
        scan.finish().test_targets
    }

    #[proxima::test]
    async fn the_count_follows_the_forwarded_selection() {
        let table: [(&[&[u8]], usize); 10] = [
            (&[], 3),
            (&[b"--workspace"], 5),
            (&[b"--workspace", b"--exclude", b"cli"], 3),
            (&[b"-p", b"cli"], 2),
            (&[b"--package=cli@0.2.0"], 2),
            (&[b"--workspace", b"--lib"], 3),
            (&[b"--workspace", b"--bins"], 1),
            (&[b"--workspace", b"--test", b"it"], 1),
            (&[b"-p", b"cli", b"--bin", b"cli-main"], 1),
            (&[b"--workspace", b"--tests"], 5),
        ];
        for (arguments, expected) in table {
            assert_eq!(counted(arguments), Some(expected), "arguments {arguments:?}");
        }
    }

    #[proxima::test]
    async fn a_target_with_test_false_builds_no_harness() {
        let document = r#"{"packages":[{"name":"w","id":"path+file:///w#0.1.0","targets":[{"kind":["lib"],"name":"w","test":false},{"kind":["bin"],"name":"w","test":true}]}],"workspace_default_members":["path+file:///w#0.1.0"],"version":1}"#;
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(document.as_bytes()).expect("document fits");

        assert_eq!(scan.finish().test_targets, Some(1));
    }

    const DEMO_METADATA: &str = include_str!("../tests/fixtures/demo_metadata.json");

    #[proxima::test]
    async fn the_demo_resolve_graph_predicts_312_of_the_328_artifacts_of_a_cold_build_the_gap_is_host_and_target_feature_duplicates() {
        let mut scan = Box::new(MetadataScan::new());
        for chunk in DEMO_METADATA.as_bytes().chunks(65_536) {
            scan.feed(chunk).expect("document fits");
        }
        let facts = scan.finish();

        assert_eq!(facts.bins.predicted_units(), Some(312));
        assert_eq!(facts.test_targets, Some(2));
    }

    #[proxima::test]
    async fn dependencies_with_bins_never_hold_back_a_member_and_members_are_the_only_directories() {
        let document = r#"{"packages":[{"name":"app","id":"path+file:///w/app#0.1.0","targets":[{"kind":["lib"],"name":"app","test":true}]},{"name":"tool","id":"registry+https://github.com/rust-lang/crates.io-index#tool@1.0.0","targets":[{"kind":["bin"],"name":"tool","test":true}]}],"workspace_members":["path+file:///w/app#0.1.0"],"workspace_default_members":["path+file:///w/app#0.1.0"],"resolve":{"nodes":[{"id":"path+file:///w/app#0.1.0","dependencies":["registry+https://github.com/rust-lang/crates.io-index#tool@1.0.0"],"deps":[{"name":"tool","pkg":"registry+https://github.com/rust-lang/crates.io-index#tool@1.0.0","dep_kinds":[{"kind":null,"target":null}]}],"features":[]},{"id":"registry+https://github.com/rust-lang/crates.io-index#tool@1.0.0","dependencies":[],"deps":[],"features":[]}],"root":null},"target_directory":"/w/target","version":1}"#;
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(document.as_bytes()).expect("document fits");
        let facts = scan.finish();

        assert!(!facts.bins.awaiting("registry+https://github.com/rust-lang/crates.io-index#tool@1.0.0"));
        assert_eq!(facts.bins.member_directories().collect::<std::vec::Vec<_>>(), [b"/w/app".as_slice()]);
        assert_eq!(facts.test_targets, Some(1));
        assert_eq!(facts.bins.predicted_units(), Some(2));
    }

    #[proxima::test]
    async fn a_document_deeper_than_the_fixed_stack_is_refused() {
        let mut scan = Box::new(MetadataScan::new());
        assert_eq!(scan.feed(b"[[[[[[[[["), Err(ScanFault::TooDeep));
    }

    #[proxima::test]
    async fn reports_the_target_directory_and_workspace_root_of_the_root_object_only() {
        let document = r#"{"packages":[{"name":"x","id":"path+file:///w#0.1.0","manifest_path":"/w/Cargo.toml","targets":[]}],"workspace_members":[],"workspace_root":"/Users/dev/work space","target_directory":"/tmp/cargo_target","version":1}"#;
        let mut scan = Box::new(MetadataScan::new());
        for chunk in document.as_bytes().chunks(5) {
            scan.feed(chunk).expect("document fits");
        }
        let facts = scan.finish();

        assert_eq!(facts.target_directory(), Some(&b"/tmp/cargo_target"[..]));
        assert_eq!(facts.workspace_root(), Some(&b"/Users/dev/work space"[..]));
    }

    #[proxima::test]
    async fn an_escaped_or_overlong_directory_is_reported_as_absent() {
        let overlong = std::format!(
            r#"{{"packages":[],"workspace_root":"/{}","target_directory":"/a\"b","version":1}}"#,
            "d".repeat(super::PATH_CAPACITY)
        );
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(overlong.as_bytes()).expect("document fits");
        let facts = scan.finish();

        assert_eq!(facts.workspace_root(), None);
        assert_eq!(facts.target_directory(), None);
    }

    const BUILD_ETA_METADATA: &str = include_str!("../tests/fixtures/build_eta_metadata.json");
    const BUILD_ETA_DEFAULT: &str = include_str!("../tests/fixtures/build_eta_artifacts_default.txt");
    const BUILD_ETA_WORKSPACE: &str = include_str!("../tests/fixtures/build_eta_artifacts_workspace.txt");
    const BUILD_ETA_ETA_ROUTE: &str = include_str!("../tests/fixtures/build_eta_artifacts_eta_route.txt");

    fn predicted(arguments: &[&[u8]]) -> Option<usize> {
        let mut scan = Box::new(MetadataScan::selecting(arguments));
        for chunk in BUILD_ETA_METADATA.as_bytes().chunks(997) {
            scan.feed(chunk).expect("document fits");
        }
        scan.finish().bins.predicted_units()
    }

    #[proxima::test]
    async fn the_default_selection_predicts_every_artifact_cargo_listed_for_a_root_with_a_bin_a_test_and_member_dependencies() {
        assert_eq!(BUILD_ETA_DEFAULT.lines().count(), 9);
        assert_eq!(predicted(&[]), Some(BUILD_ETA_DEFAULT.lines().count()));
    }

    #[proxima::test]
    async fn the_workspace_selection_counts_members_that_other_members_depend_on_and_leaves_out_a_test_false_bin() {
        assert!(!BUILD_ETA_WORKSPACE.contains("eta_route_probe"));
        assert_eq!(BUILD_ETA_WORKSPACE.lines().count(), 12);
        assert_eq!(predicted(&[b"--workspace"]), Some(BUILD_ETA_WORKSPACE.lines().count()));
    }

    #[proxima::test]
    async fn a_package_selection_with_a_test_false_bin_builds_its_library_for_doc_tests_and_no_bin() {
        assert_eq!(BUILD_ETA_ETA_ROUTE.lines().count(), 5);
        assert_eq!(predicted(&[b"-p", b"eta_route"]), Some(BUILD_ETA_ETA_ROUTE.lines().count()));
    }

    #[proxima::test]
    async fn a_wrong_expected_artifact_count_is_not_matched_by_the_prediction() {
        assert_ne!(predicted(&[]), Some(BUILD_ETA_DEFAULT.lines().count() - 1));
        assert_ne!(predicted(&[b"--workspace"]), Some(BUILD_ETA_DEFAULT.lines().count()));
    }
}
