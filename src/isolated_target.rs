//! The target directories cargo val's own cargo children build in, so their build locks are
//! never the ones another cargo process holds, and the lock-wait detector for the one lock
//! that cannot be isolated.

use std::vec::Vec;

const OPTION: &[u8] = b"--target-dir=";
const SUBTREE: &[u8] = b"/cargo-val/";
const KEY_LENGTH: usize = 16;
const HEX: [u8; 16] = *b"0123456789abcdef";
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 64-bit FNV-1a of the workspace root as 16 lowercase hex digits; stable across runs and hosts.
#[must_use]
pub fn workspace_key(workspace_root: &[u8]) -> [u8; KEY_LENGTH] {
    let hash = workspace_root
        .iter()
        .fold(FNV_OFFSET, |state, byte| (state ^ u64::from(*byte)).wrapping_mul(FNV_PRIME));
    let mut key = [0; KEY_LENGTH];
    for (index, digit) in key.iter_mut().enumerate() {
        let shift = 60 - 4 * index;
        *digit = HEX[usize::try_from((hash >> shift) & 0xf).unwrap_or(0)];
    }
    key
}

/// One child's `--target-dir=<directory>` argument.
pub struct ChildDirectory {
    bytes: Vec<u8>,
}

impl ChildDirectory {
    fn assemble(parts: &[&[u8]]) -> Option<Self> {
        if parts.iter().any(|part| part.contains(&0)) {
            return None;
        }
        Some(Self { bytes: parts.concat() })
    }

    /// The whole `--target-dir=<directory>` argument.
    #[must_use]
    pub fn argument(&self) -> &[u8] {
        &self.bytes
    }

    /// The directory alone, for messages.
    #[must_use]
    pub fn directory(&self) -> &[u8] {
        &self.bytes[OPTION.len()..]
    }
}

/// The `test` (`cargo test --no-run`) and `doc` (`cargo test --doc`) build directories.
pub struct BuildDirectories {
    pub test: ChildDirectory,
    pub doc: ChildDirectory,
}

impl BuildDirectories {
    /// `<target_directory>/cargo-val/<workspace key>/{test,doc}`: one lock pair per workspace even when
    /// every repository on the machine shares one `CARGO_TARGET_DIR`.
    #[must_use]
    pub fn isolated(target_directory: &[u8], workspace_root: &[u8]) -> Option<Self> {
        let key = workspace_key(workspace_root);
        let leaf = |name: &'static [u8]| {
            ChildDirectory::assemble(&[OPTION, target_directory, SUBTREE, &key, b"/", name])
        };
        Some(Self {
            test: leaf(b"test")?,
            doc: leaf(b"doc")?,
        })
    }

    /// `<root>/{test,doc}` for a root the caller named.
    #[must_use]
    pub fn under(root: &[u8]) -> Option<Self> {
        let leaf = |name: &'static [u8]| ChildDirectory::assemble(&[OPTION, root, b"/", name]);
        Some(Self {
            test: leaf(b"test")?,
            doc: leaf(b"doc")?,
        })
    }
}

/// Which lock a child cargo reported waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// The target directory's build lock: isolation moves it off other cargo processes.
    BuildDirectory,
    /// `~/.cargo`'s package cache lock: global to the user, so no target directory choice avoids it.
    PackageCache,
    Other,
}

impl LockKind {
    /// The machine word agent and json output carry as `lock=`.
    #[must_use]
    pub const fn word(self) -> &'static [u8] {
        match self {
            Self::BuildDirectory => b"build_directory",
            Self::PackageCache => b"package_cache",
            Self::Other => b"other",
        }
    }

    /// The name a sentence uses, as cargo prints it after "waiting for file lock on".
    #[must_use]
    pub const fn phrase(self) -> &'static [u8] {
        match self {
            Self::BuildDirectory => b"build directory",
            Self::PackageCache => b"package cache",
            Self::Other => b"file lock",
        }
    }
}

const LOCK_NEEDLE: &[u8] = b"waiting for file lock on ";
const WHICH_CAPACITY: usize = 48;

/// Watches one child's stderr for cargo's "Blocking waiting for file lock on <which>" line, once per lock kind: a child that waits on the package cache and then on the build directory announces both.
pub struct LockWatch {
    progress: usize,
    capturing: bool,
    which: [u8; WHICH_CAPACITY],
    which_length: usize,
    capture: [u8; WHICH_CAPACITY],
    capture_length: usize,
    announced: u8,
    directory: Vec<u8>,
}

impl LockWatch {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            progress: 0,
            capturing: false,
            which: [0; WHICH_CAPACITY],
            which_length: 0,
            capture: [0; WHICH_CAPACITY],
            capture_length: 0,
            announced: 0,
            directory: Vec::new(),
        }
    }

    /// Records the target directory the child builds in.
    pub fn set_directory(&mut self, directory: &[u8]) {
        self.directory.clear();
        self.directory.extend_from_slice(directory);
    }

    #[must_use]
    pub fn directory(&self) -> &[u8] {
        &self.directory
    }

    /// The lock name as cargo printed it, such as `build directory`.
    #[must_use]
    pub fn which(&self) -> &[u8] {
        &self.which[..self.which_length]
    }

    /// Feeds stderr bytes; returns a lock kind the first time a complete wait line for it was seen.
    pub fn observe(&mut self, bytes: &[u8]) -> Option<LockKind> {
        for byte in bytes {
            if self.capturing {
                let ended = matches!(*byte, b'\n' | b'\r') || self.capture_length == WHICH_CAPACITY;
                if ended {
                    self.capturing = false;
                    let kind = Self::kind(&self.capture[..self.capture_length]);
                    if self.first_of(kind) {
                        self.which_length = self.capture_length;
                        self.which[..self.which_length].copy_from_slice(&self.capture[..self.capture_length]);
                        return Some(kind);
                    }
                    continue;
                }
                self.capture[self.capture_length] = *byte;
                self.capture_length += 1;
            } else if *byte == LOCK_NEEDLE[self.progress] {
                self.progress += 1;
                if self.progress == LOCK_NEEDLE.len() {
                    self.capturing = true;
                    self.progress = 0;
                    self.capture_length = 0;
                }
            } else {
                self.progress = usize::from(*byte == LOCK_NEEDLE[0]);
            }
        }
        None
    }

    fn first_of(&mut self, kind: LockKind) -> bool {
        let bit = 1_u8 << (kind as u8);
        let first = self.announced & bit == 0;
        self.announced |= bit;
        first
    }

    fn kind(which: &[u8]) -> LockKind {
        let names = |name: &[u8]| which.windows(name.len()).any(|window| window == name);
        if names(b"package cache") {
            LockKind::PackageCache
        } else if names(b"build directory") {
            LockKind::BuildDirectory
        } else {
            LockKind::Other
        }
    }
}

impl Default for LockWatch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildDirectories, LockKind, LockWatch, workspace_key};

    const BUILD_LINE: &[u8] =
        b"    Blocking waiting for file lock on build directory\n   Compiling resume v0.1.0\n";
    const CACHE_LINE: &[u8] = b"    Blocking waiting for file lock on package cache\n";

    #[proxima::test]
    async fn the_workspace_key_is_fnv_1a_64_in_lowercase_hex() {
        assert_eq!(&workspace_key(b""), b"cbf29ce484222325");
        assert_eq!(&workspace_key(b"a"), b"af63dc4c8601ec8c");
        assert_ne!(workspace_key(b"/repos/a"), workspace_key(b"/repos/b"));
    }

    #[proxima::test]
    async fn isolated_directories_sit_under_the_target_directory_keyed_by_workspace() {
        let directories = BuildDirectories::isolated(b"/tmp/cargo_target", b"/repos/resume")
            .expect("path fits");
        let key = workspace_key(b"/repos/resume");
        let key = core::str::from_utf8(&key).expect("hex is ascii");

        assert_eq!(
            directories.test.argument(),
            std::format!("--target-dir=/tmp/cargo_target/cargo-val/{key}/test").as_bytes()
        );
        assert_eq!(
            directories.doc.directory(),
            std::format!("/tmp/cargo_target/cargo-val/{key}/doc").as_bytes()
        );
    }

    #[proxima::test]
    async fn two_workspaces_sharing_one_target_directory_get_distinct_directories() {
        let first = BuildDirectories::isolated(b"/tmp/cargo_target", b"/repos/a").expect("fits");
        let second = BuildDirectories::isolated(b"/tmp/cargo_target", b"/repos/b").expect("fits");

        assert_ne!(first.test.directory(), second.test.directory());
        assert_ne!(first.test.directory(), first.doc.directory());
    }

    #[proxima::test]
    async fn a_named_root_holds_a_test_and_a_doc_subdirectory() {
        let directories = BuildDirectories::under(b"target/own").expect("fits");

        assert_eq!(directories.test.directory(), b"target/own/test");
        assert_eq!(directories.doc.directory(), b"target/own/doc");
    }

    #[proxima::test]
    async fn a_path_holding_nul_is_refused_and_a_long_one_is_kept_whole() {
        let long = [b'd'; 1500];

        let kept = BuildDirectories::isolated(&long, b"/w").expect("a long path has no cap");
        assert!(kept.test.directory().starts_with(&long));
        assert!(BuildDirectories::isolated(b"a\0b", b"/w").is_none());
        assert!(BuildDirectories::under(b"a\0b").is_none());
    }

    #[proxima::test]
    async fn a_wait_line_is_announced_once_wherever_the_chunks_split() {
        for split in 0..=BUILD_LINE.len() {
            let mut watch = LockWatch::new();
            let (head, tail) = BUILD_LINE.split_at(split);
            let found = [watch.observe(head), watch.observe(tail), watch.observe(BUILD_LINE)];

            assert_eq!(
                found.iter().flatten().copied().collect::<std::vec::Vec<_>>(),
                [LockKind::BuildDirectory],
                "split {split}"
            );
            assert_eq!(watch.which(), b"build directory");
        }
    }

    #[proxima::test]
    async fn each_lock_kind_is_announced_once_so_a_package_cache_wait_never_hides_a_build_directory_wait() {
        let mut watch = LockWatch::new();
        let stream = [CACHE_LINE, CACHE_LINE, BUILD_LINE, BUILD_LINE];

        let found: std::vec::Vec<LockKind> = stream.iter().filter_map(|chunk| watch.observe(chunk)).collect();

        assert_eq!(found, [LockKind::PackageCache, LockKind::BuildDirectory]);
        assert_eq!(watch.which(), b"build directory");
    }

    #[proxima::test]
    async fn the_package_cache_and_unknown_locks_are_told_apart() {
        let mut cache = LockWatch::new();
        let mut other = LockWatch::new();

        assert_eq!(cache.observe(CACHE_LINE), Some(LockKind::PackageCache));
        assert_eq!(
            other.observe(b"Blocking waiting for file lock on artifact directory\n"),
            Some(LockKind::Other)
        );
    }

    #[proxima::test]
    async fn ordinary_cargo_output_never_announces() {
        let mut watch = LockWatch::new();
        let output = b"   Compiling a v0.1.0\n    Finished `test` profile in 0.3s\nfile lock on file lock\n";

        assert_eq!(watch.observe(output), None);
    }

    #[proxima::test]
    async fn the_directory_is_kept_whole_for_the_message() {
        let mut watch = LockWatch::new();
        watch.set_directory(b"/tmp/cargo_target");
        assert_eq!(watch.directory(), b"/tmp/cargo_target");

        watch.set_directory(&[b'x'; 1500]);
        assert_eq!(watch.directory().len(), 1500);
    }
}
