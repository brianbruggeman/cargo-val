//! Dynamic library path, working directory, and package variables for one test binary.
//!
//! Cargo and nextest give a test executable a library search path, the package
//! directory as its working directory, and the package's `CARGO_*` variables.
//! This module builds those once per executable into caller-owned storage so
//! every test launched from that executable reuses the same bytes.

use core::ffi::CStr;
use core::str;

use crate::cargo_message::{BuildEnvironment, BuildScript, TestArtifact};

/// Most `KEY=VALUE` entries one executable can receive.
pub const MAX_ENTRIES: usize = 64;
pub use crate::edge_args::DYLIB_PATH_KEY;
use crate::edge_args::{DEPS_SUFFIX, LOADER_DEFAULT_FALLBACKS, MAIN_SEPARATOR, PATH_SEPARATOR};

/// Bounded failures while building one executable's environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestEnvironmentError {
    /// The caller's storage cannot hold the entries and their terminators.
    StorageTooSmall { available: usize },
    /// A value contains an interior NUL byte.
    InteriorNul,
    /// A search directory contains the platform's path-list separator.
    SeparatorInDirectory,
    /// The package id has no `#` fragment or its version is not `major.minor.patch`.
    InvalidPackageId,
    /// The manifest path has no parent directory.
    InvalidManifestPath,
    /// The executable's environment would exceed [`MAX_ENTRIES`].
    TooManyEntries,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

struct Writer<'storage> {
    storage: &'storage mut [u8],
    length: usize,
}

impl Writer<'_> {
    fn put(&mut self, text: &str) -> Result<(), TestEnvironmentError> {
        self.put_bytes(text.as_bytes())
    }

    fn put_bytes(&mut self, bytes: &[u8]) -> Result<(), TestEnvironmentError> {
        let end = self.length + bytes.len();
        let available = self.storage.len();
        self.storage
            .get_mut(self.length..end)
            .ok_or(TestEnvironmentError::StorageTooSmall { available })?
            .copy_from_slice(bytes);
        self.length = end;
        Ok(())
    }

    fn entry(&mut self, key: &str, value: &str) -> Result<Span, TestEnvironmentError> {
        let start = self.length;
        self.put(key)?;
        self.put("=")?;
        self.put(value)?;
        self.terminate(start)
    }

    fn terminate(&mut self, start: usize) -> Result<Span, TestEnvironmentError> {
        let end = self.length;
        self.put_bytes(&[0])?;
        if CStr::from_bytes_with_nul(&self.storage[start..=end]).is_err() {
            return Err(TestEnvironmentError::InteriorNul);
        }
        Ok(Span { start, end })
    }
}

/// Search directories that build scripts reported, joined by the platform separator.
#[derive(Debug)]
pub struct LinkedDirectories<'storage> {
    storage: &'storage mut [u8],
    length: usize,
}

impl<'storage> LinkedDirectories<'storage> {
    /// Binds caller storage that holds every reported directory.
    pub fn new(storage: &'storage mut [u8]) -> Self {
        Self { storage, length: 0 }
    }

    /// Adds one reported `-L` value such as `native=/repo/target/debug/build/ring-4a1b/out`.
    ///
    /// The kind before the first `=` is dropped and an already present directory is kept once.
    ///
    /// # Errors
    ///
    /// Returns [`TestEnvironmentError`] for an interior NUL, a path-list separator inside the
    /// directory, or storage that cannot hold the directory.
    pub fn add(&mut self, reported: &str) -> Result<(), TestEnvironmentError> {
        let directory = reported
            .split_once('=')
            .map_or(reported, |(_, directory)| directory);
        if directory.is_empty() {
            return Ok(());
        }
        if directory.as_bytes().contains(&0) {
            return Err(TestEnvironmentError::InteriorNul);
        }
        if directory.as_bytes().contains(&PATH_SEPARATOR) {
            return Err(TestEnvironmentError::SeparatorInDirectory);
        }
        if has_component(&self.storage[..self.length], directory.as_bytes()) {
            return Ok(());
        }
        let separator = usize::from(self.length > 0);
        let end = self.length + separator + directory.len();
        let available = self.storage.len();
        let target = self
            .storage
            .get_mut(self.length..end)
            .ok_or(TestEnvironmentError::StorageTooSmall { available })?;
        if separator == 1 {
            target[0] = PATH_SEPARATOR;
        }
        target[separator..].copy_from_slice(directory.as_bytes());
        self.length = end;
        Ok(())
    }

    /// Returns every directory joined by the platform separator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // safety: storage holds only whole `&str` copies and an ASCII separator.
        unsafe { str::from_utf8_unchecked(&self.storage[..self.length]) }
    }
}

/// What one build script exported for a package, found by [`BuildScriptOutputs::find`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildScriptOutput<'output> {
    /// output directory, exported to the test as `OUT_DIR`.
    pub out_dir: &'output str,
    /// variables the script asked rustc to set, exported to the test as written.
    pub environment: BuildEnvironment<'output>,
}

/// Build-script outputs recorded per package while Cargo streams its messages.
#[derive(Debug)]
pub struct BuildScriptOutputs<'storage> {
    storage: &'storage mut [u8],
    length: usize,
}

const LENGTH_PREFIX: usize = 4;

impl<'storage> BuildScriptOutputs<'storage> {
    /// Binds caller storage that holds every executed build script.
    pub fn new(storage: &'storage mut [u8]) -> Self {
        Self { storage, length: 0 }
    }

    /// Records one executed build script; a later script for the same package replaces an earlier one at lookup.
    ///
    /// # Errors
    ///
    /// Returns [`TestEnvironmentError::StorageTooSmall`] when the record does not fit.
    pub fn add(&mut self, script: &BuildScript<'_>) -> Result<(), TestEnvironmentError> {
        let environment = script.environment.separated();
        let payload = script.package_id.len() + 1 + script.out_dir.len() + 1 + environment.len();
        let end = self.length + LENGTH_PREFIX + payload;
        let available = self.storage.len();
        let target = self
            .storage
            .get_mut(self.length..end)
            .ok_or(TestEnvironmentError::StorageTooSmall { available })?;
        let length = u32::try_from(payload)
            .map_err(|_| TestEnvironmentError::StorageTooSmall { available })?;
        target[..LENGTH_PREFIX].copy_from_slice(&length.to_le_bytes());
        let mut offset = LENGTH_PREFIX;
        for part in [script.package_id, script.out_dir] {
            target[offset..offset + part.len()].copy_from_slice(part.as_bytes());
            target[offset + part.len()] = 0;
            offset += part.len() + 1;
        }
        target[offset..].copy_from_slice(environment.as_bytes());
        self.length = end;
        Ok(())
    }

    /// Returns the most recent output recorded for `package_id`.
    #[must_use]
    pub fn find(&self, package_id: &str) -> Option<BuildScriptOutput<'_>> {
        let mut offset = 0;
        let mut found = None;
        while offset < self.length {
            let prefix: [u8; LENGTH_PREFIX] = self.storage[offset..offset + LENGTH_PREFIX]
                .try_into()
                .ok()?;
            let payload_start = offset + LENGTH_PREFIX;
            let payload_end = payload_start + u32::from_le_bytes(prefix) as usize;
            // safety: `add` wrote only whole `&str` copies and NUL terminators.
            let payload =
                unsafe { str::from_utf8_unchecked(&self.storage[payload_start..payload_end]) };
            let (recorded_package, rest) = payload.split_once('\0')?;
            if recorded_package == package_id {
                let (out_dir, environment) = rest.split_once('\0')?;
                found = Some(BuildScriptOutput {
                    out_dir,
                    environment: BuildEnvironment::from_separated(environment),
                });
            }
            offset = payload_end;
        }
        found
    }
}

/// Everything one executable's environment is derived from.
#[derive(Debug, Clone, Copy)]
pub struct TestEnvironmentInput<'input> {
    /// executable, package id, and manifest from Cargo's `compiler-artifact`.
    pub artifact: TestArtifact<'input>,
    /// build-script search directories in [`LinkedDirectories::as_str`] form.
    pub linked_directories: &'input str,
    /// `rustc --print target-libdir` results, such as `/Users/dev/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/lib`.
    pub library_directories: &'input [&'input str],
    /// value of [`DYLIB_PATH_KEY`] in this process, if present.
    pub inherited_dylib_path: Option<&'input str>,
    /// `HOME`, which supplies `$HOME/lib` to the macOS loader fallback when no inherited path exists.
    pub home: Option<&'input str>,
    /// the owning package's build script output, which supplies `OUT_DIR` and `rustc-env` variables.
    pub build_script: Option<BuildScriptOutput<'input>>,
}

/// Offsets of one executable's entries inside caller storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestEnvironmentLayout {
    entries: [Span; MAX_ENTRIES],
    count: usize,
    current_directory: Span,
}

impl TestEnvironmentLayout {
    /// Writes the library path, package directory, and package variables into `storage`.
    ///
    /// The library path lists build-script directories, the Cargo artifact
    /// directory, its `deps` directory, then the rustc library directories,
    /// followed by the inherited path (nextest `rust_build_meta.rs` `dylib_paths`
    /// and `test_list.rs` `create_dylib_path`).
    ///
    /// # Errors
    ///
    /// Returns [`TestEnvironmentError`] for malformed package data, interior NUL bytes, or
    /// storage that cannot hold every entry.
    pub fn build(
        input: &TestEnvironmentInput<'_>,
        storage: &mut [u8],
    ) -> Result<Self, TestEnvironmentError> {
        let identity = PackageIdentity::parse(input.artifact.package_id)?;
        let manifest_directory = parent(input.artifact.manifest_path)
            .ok_or(TestEnvironmentError::InvalidManifestPath)?;
        let mut writer = Writer { storage, length: 0 };
        let mut layout = Self {
            entries: [Span::default(); MAX_ENTRIES],
            count: 0,
            current_directory: Span::default(),
        };
        layout.push(write_dylib_path(&mut writer, input)?)?;
        layout.push(writer.entry("CARGO_MANIFEST_DIR", manifest_directory)?)?;
        layout.push(writer.entry("CARGO_PKG_NAME", identity.name)?)?;
        layout.push(writer.entry("CARGO_PKG_VERSION", identity.version)?)?;
        layout.push(writer.entry("CARGO_PKG_VERSION_MAJOR", identity.major)?)?;
        layout.push(writer.entry("CARGO_PKG_VERSION_MINOR", identity.minor)?)?;
        layout.push(writer.entry("CARGO_PKG_VERSION_PATCH", identity.patch)?)?;
        layout.push(writer.entry("CARGO_PKG_VERSION_PRE", identity.pre)?)?;
        if let Some(script) = input.build_script {
            layout.push(writer.entry("OUT_DIR", script.out_dir)?)?;
            for (key, value) in script.environment.iter() {
                if !layout.has_key(writer.storage, key) {
                    layout.push(writer.entry(key, value)?)?;
                }
            }
        }
        let start = writer.length;
        writer.put(manifest_directory)?;
        layout.current_directory = writer.terminate(start)?;
        Ok(layout)
    }

    fn push(&mut self, span: Span) -> Result<(), TestEnvironmentError> {
        let slot = self
            .entries
            .get_mut(self.count)
            .ok_or(TestEnvironmentError::TooManyEntries)?;
        *slot = span;
        self.count += 1;
        Ok(())
    }

    fn has_key(&self, storage: &[u8], key: &str) -> bool {
        self.entries[..self.count].iter().any(|span| {
            text(storage, *span)
                .strip_prefix(key)
                .is_some_and(|rest| rest.starts_with('='))
        })
    }

    /// Returns how many `KEY=VALUE` entries were written.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Returns whether no entries were written.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns one `KEY=VALUE` entry written by [`Self::build`] into the same `storage`.
    #[must_use]
    pub fn entry<'storage>(&self, storage: &'storage [u8], index: usize) -> Option<&'storage str> {
        (index < self.count).then(|| text(storage, self.entries[index]))
    }

    /// Returns the package directory every test of this executable starts in.
    #[must_use]
    pub fn current_directory<'storage>(&self, storage: &'storage [u8]) -> &'storage str {
        text(storage, self.current_directory)
    }
}

fn text(storage: &[u8], span: Span) -> &str {
    // safety: `build` wrote only whole `&str` copies and ASCII separators into this span.
    unsafe { str::from_utf8_unchecked(&storage[span.start..span.end]) }
}

fn write_dylib_path(
    writer: &mut Writer<'_>,
    input: &TestEnvironmentInput<'_>,
) -> Result<Span, TestEnvironmentError> {
    let start = writer.length;
    writer.put(DYLIB_PATH_KEY)?;
    writer.put("=")?;
    let value_start = writer.length;
    if !input.linked_directories.is_empty() {
        writer.put(input.linked_directories)?;
    }
    let executable = input.artifact.executable;
    if let Some(executable_directory) = parent(executable) {
        let leaf = file_name(executable_directory);
        if leaf == "deps" || leaf == "examples" {
            if let Some(base) = parent(executable_directory) {
                push_directory(writer, value_start, &[base])?;
                push_directory(writer, value_start, &[base, DEPS_SUFFIX])?;
            }
        } else {
            push_directory(writer, value_start, &[executable_directory])?;
        }
    }
    for library_directory in input.library_directories {
        push_directory(writer, value_start, &[library_directory])?;
    }
    let inherited = input.inherited_dylib_path.filter(|value| !value.is_empty());
    match inherited {
        Some(inherited) => push_tail(writer, value_start, &[inherited])?,
        None => push_default_fallbacks(writer, value_start, input.home)?,
    }
    writer.terminate(start)
}

fn push_default_fallbacks(
    writer: &mut Writer<'_>,
    value_start: usize,
    home: Option<&str>,
) -> Result<(), TestEnvironmentError> {
    if !LOADER_DEFAULT_FALLBACKS {
        return Ok(());
    }
    if let Some(home) = home {
        push_tail(writer, value_start, &[home, "/lib"])?;
    }
    push_tail(writer, value_start, &["/usr/local/lib"])?;
    push_tail(writer, value_start, &["/usr/lib"])
}

fn push_tail(
    writer: &mut Writer<'_>,
    value_start: usize,
    parts: &[&str],
) -> Result<(), TestEnvironmentError> {
    if writer.length > value_start {
        writer.put_bytes(&[PATH_SEPARATOR])?;
    }
    for part in parts {
        writer.put(part)?;
    }
    Ok(())
}

fn push_directory(
    writer: &mut Writer<'_>,
    value_start: usize,
    parts: &[&str],
) -> Result<(), TestEnvironmentError> {
    let mark = writer.length;
    push_tail(writer, value_start, parts)?;
    let separator = usize::from(mark > value_start);
    let directory = &writer.storage[mark + separator..writer.length];
    if has_component(&writer.storage[value_start..mark], directory) {
        writer.length = mark;
    }
    Ok(())
}

fn has_component(list: &[u8], directory: &[u8]) -> bool {
    !list.is_empty()
        && list
            .split(|byte| *byte == PATH_SEPARATOR)
            .any(|component| component == directory)
}

fn parent(path: &str) -> Option<&str> {
    let separator = path.bytes().rposition(|byte| byte == MAIN_SEPARATOR)?;
    if separator == 0 {
        return None;
    }
    Some(&path[..separator])
}

fn file_name(path: &str) -> &str {
    match path.bytes().rposition(|byte| byte == MAIN_SEPARATOR) {
        Some(separator) => &path[separator + 1..],
        None => path,
    }
}

struct PackageIdentity<'id> {
    name: &'id str,
    version: &'id str,
    major: &'id str,
    minor: &'id str,
    patch: &'id str,
    pre: &'id str,
}

impl<'id> PackageIdentity<'id> {
    fn parse(package_id: &'id str) -> Result<Self, TestEnvironmentError> {
        let (source, fragment) = package_id
            .rsplit_once('#')
            .ok_or(TestEnvironmentError::InvalidPackageId)?;
        let (name, version) = match fragment.split_once('@') {
            Some(named) => named,
            None => (source.rsplit('/').next().unwrap_or(""), fragment),
        };
        let without_build = version
            .split_once('+')
            .map_or(version, |(release, _)| release);
        let (core, pre) = without_build.split_once('-').unwrap_or((without_build, ""));
        let mut numbers = core.split('.');
        let (Some(major), Some(minor), Some(patch), None) = (
            numbers.next(),
            numbers.next(),
            numbers.next(),
            numbers.next(),
        ) else {
            return Err(TestEnvironmentError::InvalidPackageId);
        };
        let numeric =
            |number: &str| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit());
        if name.is_empty() || !numeric(major) || !numeric(minor) || !numeric(patch) {
            return Err(TestEnvironmentError::InvalidPackageId);
        }
        Ok(Self {
            name,
            version,
            major,
            minor,
            patch,
            pre,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use super::{
        BuildScriptOutputs, DYLIB_PATH_KEY, LinkedDirectories, TestEnvironmentError,
        TestEnvironmentInput, TestEnvironmentLayout,
    };
    use crate::cargo_message::{CargoMessage, TestArtifact, parse_cargo_message};

    static BUILD_SCRIPT_RECORD: &[u8] =
        include_bytes!("../tests/fixtures/build_script_executed.jsonl");

    const SYSROOT_LIBDIR: &str = "/Users/dev/.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/lib";

    const MANIFEST_DIR: &str = if cfg!(windows) {
        "C:\\repo\\tests\\fixtures\\dylib_env"
    } else {
        "/repo/tests/fixtures/dylib_env"
    };
    const MANIFEST_PATH: &str = if cfg!(windows) {
        "C:\\repo\\tests\\fixtures\\dylib_env\\Cargo.toml"
    } else {
        "/repo/tests/fixtures/dylib_env/Cargo.toml"
    };
    const DEPS_EXECUTABLE: &str = if cfg!(windows) {
        "C:\\repo\\target\\debug\\deps\\dylib_env-3f2a1c9d8e7b6a50"
    } else {
        "/repo/target/debug/deps/dylib_env-3f2a1c9d8e7b6a50"
    };
    const EXAMPLES_EXECUTABLE: &str = if cfg!(windows) {
        "C:\\repo\\target\\debug\\examples\\demo-9c8b7a"
    } else {
        "/repo/target/debug/examples/demo-9c8b7a"
    };
    const TARGET_DEBUG: &str = if cfg!(windows) {
        "C:\\repo\\target\\debug"
    } else {
        "/repo/target/debug"
    };
    const TARGET_DEPS: &str = if cfg!(windows) {
        "C:\\repo\\target\\debug\\deps"
    } else {
        "/repo/target/debug/deps"
    };

    fn artifact() -> TestArtifact<'static> {
        TestArtifact {
            executable: DEPS_EXECUTABLE,
            package_id: "path+file:///repo/tests/fixtures/dylib_env#0.1.0",
            manifest_path: MANIFEST_PATH,
            target_name: "",
            target_kind: "",
        }
    }

    fn input<'input>(
        artifact: TestArtifact<'input>,
        linked: &'input str,
        libraries: &'input [&'input str],
        inherited: Option<&'input str>,
    ) -> TestEnvironmentInput<'input> {
        TestEnvironmentInput {
            artifact,
            linked_directories: linked,
            library_directories: libraries,
            inherited_dylib_path: inherited,
            home: Some("/Users/dev"),
            build_script: None,
        }
    }

    fn build_script_message<'text>(
        record: &'text [u8],
        scratch: &'text mut [u8],
    ) -> crate::cargo_message::BuildScript<'text> {
        match parse_cargo_message(record, scratch, 4096, 8) {
            Ok(CargoMessage::BuildScript(script)) => script,
            other => panic!("record is not a build script: {other:?}"),
        }
    }

    fn lookup<'storage>(
        layout: &TestEnvironmentLayout,
        storage: &'storage [u8],
        key: &str,
    ) -> Option<&'storage str> {
        (0..layout.len())
            .filter_map(|index| layout.entry(storage, index))
            .find_map(|entry| entry.strip_prefix(key)?.strip_prefix('='))
    }

    #[test]
    fn library_path_lists_linked_then_artifact_then_deps_then_sysroot_then_inherited() {
        let mut linked_storage = [0; 256];
        let mut linked = LinkedDirectories::new(&mut linked_storage);
        linked
            .add("native=/repo/target/debug/build/ring-4a1b0c/out")
            .expect("native directory fits");
        linked
            .add("dependency=/repo/target/debug/build/ring-4a1b0c/out")
            .expect("repeat is accepted");
        linked
            .add("/opt/homebrew/lib")
            .expect("bare directory fits");
        let libraries = [SYSROOT_LIBDIR];
        let mut storage = [0; 2048];

        let layout = TestEnvironmentLayout::build(
            &input(artifact(), linked.as_str(), &libraries, Some("/usr/lib")),
            &mut storage,
        )
        .expect("environment fits");

        let separator = if cfg!(windows) { ";" } else { ":" };
        let expected = [
            "/repo/target/debug/build/ring-4a1b0c/out",
            "/opt/homebrew/lib",
            TARGET_DEBUG,
            TARGET_DEPS,
            SYSROOT_LIBDIR,
            "/usr/lib",
        ]
        .join(separator);
        assert_eq!(
            lookup(&layout, &storage, DYLIB_PATH_KEY),
            Some(expected.as_str())
        );
    }

    #[test]
    fn package_variables_and_working_directory_come_from_cargo_json() {
        let mut storage = [0; 2048];
        let artifact = TestArtifact {
            package_id: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228-rc.1+build.5",
            ..artifact()
        };

        let layout =
            TestEnvironmentLayout::build(&input(artifact, "", &[], Some("/x")), &mut storage)
                .expect("environment fits");

        assert_eq!(layout.current_directory(&storage), MANIFEST_DIR);
        assert_eq!(
            layout.current_directory(&storage).as_bytes(),
            MANIFEST_DIR.as_bytes()
        );
        let expected = [
            ("CARGO_MANIFEST_DIR", MANIFEST_DIR),
            ("CARGO_PKG_NAME", "serde"),
            ("CARGO_PKG_VERSION", "1.0.228-rc.1+build.5"),
            ("CARGO_PKG_VERSION_MAJOR", "1"),
            ("CARGO_PKG_VERSION_MINOR", "0"),
            ("CARGO_PKG_VERSION_PATCH", "228"),
            ("CARGO_PKG_VERSION_PRE", "rc.1"),
        ];
        for (key, value) in expected {
            assert_eq!(lookup(&layout, &storage, key), Some(value), "{key}");
        }
    }

    #[test]
    fn path_package_without_a_name_fragment_is_named_by_its_directory() {
        let mut storage = [0; 2048];

        let layout =
            TestEnvironmentLayout::build(&input(artifact(), "", &[], Some("/x")), &mut storage)
                .expect("environment fits");

        assert_eq!(
            lookup(&layout, &storage, "CARGO_PKG_NAME"),
            Some("dylib_env")
        );
        assert_eq!(lookup(&layout, &storage, "CARGO_PKG_VERSION_PRE"), Some(""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_without_an_inherited_path_appends_the_loader_defaults() {
        let mut storage = [0; 2048];

        let layout = TestEnvironmentLayout::build(
            &input(artifact(), "", &[SYSROOT_LIBDIR], None),
            &mut storage,
        )
        .expect("environment fits");

        let tail = lookup(&layout, &storage, DYLIB_PATH_KEY).expect("library path entry exists");
        assert!(
            tail.ends_with(":/Users/dev/lib:/usr/local/lib:/usr/lib"),
            "{tail}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_without_an_inherited_path_appends_no_loader_defaults() {
        let mut storage = [0; 2048];

        let layout = TestEnvironmentLayout::build(
            &input(artifact(), "", &[SYSROOT_LIBDIR], None),
            &mut storage,
        )
        .expect("environment fits");

        let expected = [
            "/repo/target/debug",
            "/repo/target/debug/deps",
            SYSROOT_LIBDIR,
        ]
        .join(":");
        assert_eq!(
            lookup(&layout, &storage, DYLIB_PATH_KEY),
            Some(expected.as_str())
        );
    }

    #[test]
    fn examples_directory_maps_to_the_artifact_and_deps_directories() {
        let mut storage = [0; 2048];
        let artifact = TestArtifact {
            executable: EXAMPLES_EXECUTABLE,
            ..artifact()
        };

        let layout =
            TestEnvironmentLayout::build(&input(artifact, "", &[], Some("/x")), &mut storage)
                .expect("environment fits");

        let separator = if cfg!(windows) { ";" } else { ":" };
        let expected = [TARGET_DEBUG, TARGET_DEPS, "/x"].join(separator);
        assert_eq!(
            lookup(&layout, &storage, DYLIB_PATH_KEY),
            Some(expected.as_str())
        );
    }

    #[test]
    fn malformed_package_id_and_short_storage_are_rejected_with_their_reason() {
        let mut storage = [0; 2048];
        let no_fragment = TestArtifact {
            package_id: "path+file:///repo/tests/fixtures/dylib_env",
            ..artifact()
        };
        let no_parent = TestArtifact {
            manifest_path: "Cargo.toml",
            ..artifact()
        };
        let mut tiny = [0; 16];

        assert_eq!(
            TestEnvironmentLayout::build(&input(no_fragment, "", &[], None), &mut storage),
            Err(TestEnvironmentError::InvalidPackageId)
        );
        assert_eq!(
            TestEnvironmentLayout::build(&input(no_parent, "", &[], None), &mut storage),
            Err(TestEnvironmentError::InvalidManifestPath)
        );
        assert_eq!(
            TestEnvironmentLayout::build(&input(artifact(), "", &[], None), &mut tiny),
            Err(TestEnvironmentError::StorageTooSmall { available: 16 })
        );
    }

    #[test]
    fn linked_directories_reject_a_separator_and_report_overflow() {
        let mut storage = [0; 24];
        let mut linked = LinkedDirectories::new(&mut storage);
        let separator_path = if cfg!(windows) {
            "native=C:\\a;b"
        } else {
            "native=/a:b"
        };

        assert_eq!(
            linked.add(separator_path),
            Err(TestEnvironmentError::SeparatorInDirectory)
        );
        linked.add("native=/repo/target/out").expect("first fits");
        assert_eq!(
            linked.add("native=/repo/target/second"),
            Err(TestEnvironmentError::StorageTooSmall { available: 24 })
        );
    }

    #[test]
    fn one_build_serves_every_test_launch_without_allocating() {
        let mut storage = [0; 2048];
        let libraries = [SYSROOT_LIBDIR];
        let (layout, build_allocations) = crate::test_alloc::measure_allocations(|| {
            TestEnvironmentLayout::build(
                &input(
                    artifact(),
                    "/repo/target/debug/build/a/out",
                    &libraries,
                    Some("/usr/lib"),
                ),
                &mut storage,
            )
            .expect("environment fits")
        });

        let (checksum, launch_allocations) = crate::test_alloc::measure_allocations(|| {
            let mut checksum = 0_usize;
            for _ in 0..1000 {
                for index in 0..layout.len() {
                    checksum += layout
                        .entry(&storage, index)
                        .expect("index is below len")
                        .len();
                }
                checksum += layout.current_directory(&storage).len();
            }
            checksum
        });

        assert_eq!(build_allocations, 0);
        assert_eq!(launch_allocations, 0);
        assert!(checksum > 0);
    }

    #[test]
    fn build_script_output_supplies_out_dir_and_rustc_env_to_its_own_package_only() {
        let mut scratch = [0_u8; 1024];
        let script = build_script_message(BUILD_SCRIPT_RECORD, &mut scratch);
        let mut outputs_storage = [0_u8; 1024];
        let mut outputs = BuildScriptOutputs::new(&mut outputs_storage);
        outputs.add(&script).expect("record fits");
        let artifact = TestArtifact {
            package_id: script.package_id,
            ..artifact()
        };
        let mut storage = [0_u8; 2048];

        let layout = TestEnvironmentLayout::build(
            &TestEnvironmentInput {
                build_script: outputs.find(script.package_id),
                ..input(artifact, "", &[], Some("/x"))
            },
            &mut storage,
        )
        .expect("environment fits");

        assert_eq!(outputs.find("path+file:///work/another#0.1.0"), None);
        assert_eq!(lookup(&layout, &storage, "OUT_DIR"), Some(script.out_dir));
        assert_eq!(
            lookup(&layout, &storage, "PROBE_GREETING"),
            Some("hello \"quoted\" world")
        );
        assert_eq!(lookup(&layout, &storage, "PROBE_ANSWER"), Some("42"));
    }

    #[test]
    fn rustc_env_cannot_replace_package_variables_and_the_latest_script_wins() {
        let package_id = "path+file:///repo/tests/fixtures/dylib_env#0.1.0";
        let first = serde_json::json!({
            "reason": "build-script-executed", "package_id": package_id,
            "linked_libs": [], "linked_paths": [], "cfgs": [],
            "env": [["CARGO_PKG_NAME", "hijacked"], ["PROBE_ORDER", "first"]],
            "out_dir": "/repo/target/debug/build/dylib_env-aaaa/out",
        })
        .to_string();
        let second = serde_json::json!({
            "reason": "build-script-executed", "package_id": package_id,
            "linked_libs": [], "linked_paths": [], "cfgs": [],
            "env": [["CARGO_PKG_NAME", "hijacked"], ["PROBE_ORDER", "second"]],
            "out_dir": "/repo/target/debug/build/dylib_env-bbbb/out",
        })
        .to_string();
        let mut first_scratch = [0_u8; 1024];
        let mut second_scratch = [0_u8; 1024];
        let mut outputs_storage = [0_u8; 1024];
        let mut outputs = BuildScriptOutputs::new(&mut outputs_storage);
        outputs
            .add(&build_script_message(first.as_bytes(), &mut first_scratch))
            .expect("first fits");
        outputs
            .add(&build_script_message(
                second.as_bytes(),
                &mut second_scratch,
            ))
            .expect("second fits");
        let mut storage = [0_u8; 2048];

        let layout = TestEnvironmentLayout::build(
            &TestEnvironmentInput {
                build_script: outputs.find(package_id),
                ..input(artifact(), "", &[], Some("/x"))
            },
            &mut storage,
        )
        .expect("environment fits");

        assert_eq!(
            lookup(&layout, &storage, "CARGO_PKG_NAME"),
            Some("dylib_env")
        );
        assert_eq!(lookup(&layout, &storage, "PROBE_ORDER"), Some("second"));
        assert_eq!(
            lookup(&layout, &storage, "OUT_DIR"),
            Some("/repo/target/debug/build/dylib_env-bbbb/out")
        );
        let names: std::vec::Vec<&str> = (0..layout.len())
            .filter_map(|index| layout.entry(&storage, index))
            .filter(|entry| entry.starts_with("CARGO_PKG_NAME="))
            .collect();
        assert_eq!(names.len(), 1);
    }
}
