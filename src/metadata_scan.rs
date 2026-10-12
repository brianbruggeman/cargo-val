//! Which packages have `bin` targets, plus the workspace root and target directory, read from `cargo metadata --no-deps --format-version 1`
//! one byte at a time over fixed storage, so the scan allocates only its package table once and keeps no whole-document buffer.

use std::boxed::Box;
use std::vec;

const MAX_PACKAGES: usize = 256;
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
    TargetDirectory,
    WorkspaceRoot,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    object: bool,
    expect_key: bool,
    key: Key,
}

#[derive(Clone, Copy)]
struct Entry {
    id: [u8; ID_CAPACITY],
    id_length: usize,
    bins: usize,
    built: usize,
}

/// Per package with at least one `bin` target: how many it has and how many were built so far.
pub struct BinCounts {
    entries: Box<[Entry]>,
    count: usize,
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
            }; MAX_PACKAGES].into_boxed_slice(),
            count: 0,
        }
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

    /// Whether `package_id` still has a `bin` target that has not been built.
    #[must_use]
    pub fn awaiting(&self, package_id: &str) -> bool {
        self.find(package_id.as_bytes())
            .is_some_and(|index| self.entries[index].built < self.entries[index].bins)
    }

    /// Number of packages with at least one `bin` target.
    #[must_use]
    pub const fn packages(&self) -> usize {
        self.count
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
pub struct MetadataScan {
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

impl MetadataScan {
    #[must_use]
    pub fn new() -> Self {
        Self {
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
    pub fn finish(self) -> MetadataFacts {
        MetadataFacts {
            bins: self.counts,
            target_directory: self.target_directory,
            workspace_root: self.workspace_root,
        }
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
                b"target_directory" => Key::TargetDirectory,
                b"workspace_root" => Key::WorkspaceRoot,
                _ => Key::Other,
            };
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
        } else if depth == 6 && self.in_kind_list() && &self.text[..text_length] == b"bin" {
            self.package_bins += 1;
        }
        self.text_length = 0;
        self.text_overflow = false;
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
        if object && self.depth == 3 && self.in_package() {
            self.package_id_length = 0;
            self.package_id_overflow = false;
            self.package_bins = 0;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), ScanFault> {
        let closing_package = self.depth == 3 && self.in_package();
        self.depth = self.depth.saturating_sub(1);
        if closing_package && self.package_bins > 0 {
            if self.package_id_overflow {
                return Err(ScanFault::TooLarge);
            }
            let id = self.package_id;
            self.counts.add(&id[..self.package_id_length], self.package_bins)?;
        }
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

impl Default for MetadataScan {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use super::{MetadataScan, ScanFault};

    const TWO_PACKAGES: &str = r#"{"packages":[{"name":"parser","version":"0.3.1","id":"path+file:///repo/parser#0.3.1","dependencies":[{"name":"x","kind":null}],"targets":[{"kind":["lib"],"crate_types":["lib"],"name":"parser"},{"kind":["bin"],"crate_types":["bin"],"name":"parse"},{"kind":["bin"],"crate_types":["bin"],"name":"lint"},{"kind":["test"],"crate_types":["bin"],"name":"cli"}],"manifest_path":"/repo/parser/Cargo.toml"},{"name":"plain","version":"0.1.0","id":"path+file:///repo/plain#0.1.0","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"plain"},{"kind":["test"],"crate_types":["bin"],"name":"it"}]}],"workspace_members":["path+file:///repo/parser#0.3.1","path+file:///repo/plain#0.1.0"],"resolve":null,"target_directory":"/repo/target","version":1}"#;

    #[test]
    fn counts_bin_targets_per_package_and_ignores_every_other_kind() {
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

    #[test]
    fn a_document_deeper_than_the_fixed_stack_is_refused() {
        let mut scan = Box::new(MetadataScan::new());
        assert_eq!(scan.feed(b"[[[[[[[[["), Err(ScanFault::TooDeep));
    }

    #[test]
    fn reports_the_target_directory_and_workspace_root_of_the_root_object_only() {
        let document = r#"{"packages":[{"name":"x","id":"path+file:///w#0.1.0","manifest_path":"/w/Cargo.toml","targets":[]}],"workspace_members":[],"workspace_root":"/Users/dev/work space","target_directory":"/tmp/cargo_target","version":1}"#;
        let mut scan = Box::new(MetadataScan::new());
        for chunk in document.as_bytes().chunks(5) {
            scan.feed(chunk).expect("document fits");
        }
        let facts = scan.finish();

        assert_eq!(facts.target_directory(), Some(&b"/tmp/cargo_target"[..]));
        assert_eq!(facts.workspace_root(), Some(&b"/Users/dev/work space"[..]));
    }

    #[test]
    fn an_escaped_or_overlong_directory_is_reported_as_absent() {
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
}
