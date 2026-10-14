//! Cargo JSON messages that feed one test binary's launch environment.

use core::str;

use crate::{Json, ParseError, Text, text_equals};

const MAX_JSON_DEPTH: usize = 16;

/// A compiled test executable with the package that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestArtifact<'text> {
    /// executable such as `/repo/target/debug/deps/parser-0f3a9c1d2b4e5f60`.
    pub executable: &'text str,
    /// package id such as `path+file:///repo/parser#0.3.1` or `registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228`.
    pub package_id: &'text str,
    /// manifest such as `/repo/parser/Cargo.toml`; its directory is the test working directory.
    pub manifest_path: &'text str,
    /// cargo target name such as `capability_matrix`, or the crate name for unit tests; empty when the message carries none.
    pub target_name: &'text str,
    /// first cargo target kind such as `lib`, `test`, `bin`; empty when the message carries none.
    pub target_kind: &'text str,
}

impl TestArtifact<'_> {
    /// Package name from the package id: `url#name@version`, `url#version` (name is the last path segment), or `name version (url)`.
    #[must_use]
    pub fn package_name(&self) -> &str {
        let id = self.package_id;
        if let Some((name, _)) = id.split_once(' ') {
            return name;
        }
        let Some((location, fragment)) = id.rsplit_once('#') else {
            return id;
        };
        match fragment.split_once('@') {
            Some((name, _)) => name,
            None => location.rsplit('/').next().unwrap_or(location),
        }
    }

    /// Whether this target is the package's library, whose unit tests nextest ids by package name alone.
    #[must_use]
    pub fn is_unit_test_target(&self) -> bool {
        matches!(self.target_kind, "lib" | "proc-macro" | "rlib" | "cdylib" | "dylib")
    }
}

/// The `-L` search paths one build script reported, such as `native=/repo/target/debug/build/ring-4a1b/out`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkedPaths<'text> {
    separated: &'text str,
}

impl<'text> LinkedPaths<'text> {
    /// Iterates each reported path in the order the build script emitted it.
    pub fn iter(&self) -> impl Iterator<Item = &'text str> {
        self.separated.split('\0').filter(|path| !path.is_empty())
    }
}

/// The `cargo:rustc-env` pairs one build script emitted, such as `("RING_CORE_PREFIX", "ring_core_0_17_14_")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildEnvironment<'text> {
    separated: &'text str,
}

impl<'text> BuildEnvironment<'text> {
    pub(crate) const fn from_separated(separated: &'text str) -> Self {
        Self { separated }
    }

    pub(crate) const fn separated(&self) -> &'text str {
        self.separated
    }

    /// Iterates each key and value in the order the build script emitted them.
    pub fn iter(&self) -> impl Iterator<Item = (&'text str, &'text str)> {
        let mut parts = self.separated.split('\0');
        core::iter::from_fn(move || {
            let key = parts.next().filter(|key| !key.is_empty())?;
            Some((key, parts.next()?))
        })
    }
}

/// What one executed build script reported for the package that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildScript<'text> {
    /// package id such as `path+file:///repo/parser#0.3.1`; equals the owning test artifact's id.
    pub package_id: &'text str,
    /// output directory such as `/repo/target/debug/build/parser-4a1b0c/out`, exported as `OUT_DIR`.
    pub out_dir: &'text str,
    /// search paths for the platform loader.
    pub linked_paths: LinkedPaths<'text>,
    /// variables the script asked rustc to set.
    pub environment: BuildEnvironment<'text>,
}

/// One classified Cargo message; every string borrows the record or the caller's scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CargoMessage<'text> {
    /// A `compiler-artifact` for an executable built with the test profile.
    TestArtifact(TestArtifact<'text>),
    /// A `build-script-executed` for a package.
    BuildScript(BuildScript<'text>),
    /// A `compiler-artifact` for a non-test executable of a `bin` target, which `CARGO_BIN_EXE_<name>` points at.
    BinArtifact {
        /// package id such as `path+file:///repo/parser#0.3.1`; equals the owning test artifacts' id.
        package_id: &'text str,
        /// the uplifted path such as `/repo/target/debug/parser`, which cargo reports once it is linked.
        executable: &'text str,
    },
    /// A `compiler-artifact` that is neither a test executable nor a `bin`: one more compile unit done.
    Compiled {
        /// true when cargo reused the unit from a previous build instead of compiling it.
        fresh: bool,
    },
    /// `build-finished`: cargo will emit no further artifacts.
    BuildFinished {
        /// false when a unit failed to build.
        success: bool,
    },
    /// A `compiler-message` whose rustc rendering is reported verbatim when the build fails.
    Diagnostic {
        /// rustc's level such as `error` or `warning`.
        level: &'text str,
        /// the text rustc would print, such as `error[E0308]: mismatched types\n --> src/lib.rs:2:5\n...`.
        rendered: &'text str,
    },
    /// A valid message that carries nothing a test launch needs.
    Ignored,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    Other,
    CompilerMessage,
    CompilerArtifact,
    BuildScriptExecuted,
    BuildFinished,
}

#[derive(Clone, Copy, Default)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
enum Slot<'input> {
    Input(&'input [u8]),
    Scratch(Span),
}

impl<'input> Json<'input, '_> {
    fn member_key(&mut self, first: bool) -> Result<Option<Text<'input>>, ParseError> {
        self.whitespace();
        if self.input.get(self.offset) == Some(&b'}') {
            self.offset += 1;
            return Ok(None);
        }
        if !first {
            self.byte(b',')?;
            self.whitespace();
            if self.input.get(self.offset) == Some(&b'}') {
                return Err(ParseError::InvalidJson);
            }
        }
        let key = self.string(false)?;
        self.byte(b':')?;
        Ok(Some(key))
    }

    fn slot_string(&mut self) -> Result<Slot<'input>, ParseError> {
        let start = self.scratch_used;
        match self.string(true)? {
            Text::Borrowed(bytes) => Ok(Slot::Input(bytes)),
            Text::Decoded(length) => {
                self.scratch_used = start + length;
                Ok(Slot::Scratch(Span {
                    start,
                    end: start + length,
                }))
            }
            Text::Escaped(_) => Err(ParseError::InvalidJson),
        }
    }

    fn copy_string(&mut self) -> Result<(), ParseError> {
        let start = self.scratch_used;
        let end = match self.slot_string()? {
            Slot::Input(bytes) => {
                let end = start + bytes.len();
                self.scratch
                    .get_mut(start..end)
                    .ok_or(ParseError::ScratchTooSmall)?
                    .copy_from_slice(bytes);
                end
            }
            Slot::Scratch(span) => span.end,
        };
        self.scratch_used = end;
        Ok(())
    }

    fn copy_terminated_string(&mut self) -> Result<(), ParseError> {
        self.copy_string()?;
        let terminator = self
            .scratch
            .get_mut(self.scratch_used)
            .ok_or(ParseError::ScratchTooSmall)?;
        *terminator = 0;
        self.scratch_used += 1;
        Ok(())
    }

    fn copy_string_list(&mut self) -> Result<Span, ParseError> {
        self.copy_list(false)
    }

    fn copy_pair_list(&mut self) -> Result<Span, ParseError> {
        self.copy_list(true)
    }

    fn copy_list(&mut self, pairs: bool) -> Result<Span, ParseError> {
        let start = self.scratch_used;
        self.enter(1)?;
        self.byte(b'[')?;
        self.whitespace();
        if self.input.get(self.offset) == Some(&b']') {
            self.offset += 1;
            return Ok(Span { start, end: start });
        }
        loop {
            if pairs {
                self.enter(2)?;
                self.byte(b'[')?;
                self.copy_terminated_string()?;
                self.byte(b',')?;
                self.copy_terminated_string()?;
                self.byte(b']')?;
            } else {
                self.copy_terminated_string()?;
            }
            self.whitespace();
            match self.input.get(self.offset) {
                Some(b']') => {
                    self.offset += 1;
                    return Ok(Span {
                        start,
                        end: self.scratch_used,
                    });
                }
                Some(b',') => self.offset += 1,
                _ => return Err(ParseError::InvalidJson),
            }
        }
    }

    fn read_target(&mut self, fields: &mut Fields<'input>) -> Result<(), ParseError> {
        self.enter(1)?;
        self.byte(b'{')?;
        let mut first = true;
        while let Some(key) = self.member_key(first)? {
            first = false;
            if text_equals(key, b"name") {
                read_once(&mut fields.target_name, self)?;
            } else if text_equals(key, b"kind") {
                self.first_of_list(&mut fields.target_kind)?;
            } else {
                self.skip_value(2)?;
            }
        }
        Ok(())
    }

    fn read_message(&mut self, fields: &mut Fields<'input>) -> Result<(), ParseError> {
        self.whitespace();
        if self.input.get(self.offset) != Some(&b'{') {
            return self.skip_value(1);
        }
        self.enter(1)?;
        self.byte(b'{')?;
        let mut first = true;
        while let Some(key) = self.member_key(first)? {
            first = false;
            if text_equals(key, b"level") {
                read_once(&mut fields.level, self)?;
            } else if text_equals(key, b"rendered") && self.input.get(self.offset..self.offset + 4) != Some(b"null") {
                read_once(&mut fields.rendered, self)?;
            } else {
                self.skip_value(2)?;
            }
        }
        Ok(())
    }

    fn first_of_list(&mut self, slot: &mut Option<Slot<'input>>) -> Result<(), ParseError> {
        if slot.is_some() {
            return Err(ParseError::InvalidJson);
        }
        self.enter(2)?;
        self.byte(b'[')?;
        self.whitespace();
        if self.input.get(self.offset) == Some(&b']') {
            self.offset += 1;
            return Ok(());
        }
        *slot = Some(self.slot_string()?);
        loop {
            self.whitespace();
            match self.input.get(self.offset) {
                Some(b']') => {
                    self.offset += 1;
                    return Ok(());
                }
                Some(b',') => {
                    self.offset += 1;
                    self.whitespace();
                    self.skip_value(3)?;
                }
                _ => return Err(ParseError::InvalidJson),
            }
        }
    }

    fn profile_is_test(&mut self) -> Result<bool, ParseError> {
        self.enter(1)?;
        self.byte(b'{')?;
        let mut first = true;
        let mut is_test = false;
        let mut saw_test = false;
        while let Some(key) = self.member_key(first)? {
            first = false;
            if !text_equals(key, b"test") {
                self.skip_value(2)?;
                continue;
            }
            if saw_test {
                return Err(ParseError::InvalidJson);
            }
            saw_test = true;
            self.whitespace();
            if self.input.get(self.offset..self.offset + 4) == Some(b"true") {
                is_test = true;
                self.offset += 4;
            } else if self.input.get(self.offset..self.offset + 5) == Some(b"false") {
                self.offset += 5;
            } else {
                return Err(ParseError::InvalidJson);
            }
        }
        Ok(is_test)
    }
}

#[derive(Default)]
struct Fields<'input> {
    reason: Option<Reason>,
    test_profile: bool,
    saw_profile: bool,
    executable: Option<Slot<'input>>,
    saw_executable: bool,
    package_id: Option<Slot<'input>>,
    manifest_path: Option<Slot<'input>>,
    target_name: Option<Slot<'input>>,
    target_kind: Option<Slot<'input>>,
    out_dir: Option<Slot<'input>>,
    linked_paths: Option<Span>,
    environment: Option<Span>,
    success: Option<bool>,
    fresh: Option<bool>,
    level: Option<Slot<'input>>,
    rendered: Option<Slot<'input>>,
}

fn read_success(json: &mut Json<'_, '_>) -> Result<bool, ParseError> {
    json.whitespace();
    if json.input.get(json.offset) == Some(&b't') {
        json.literal(b"true")?;
        return Ok(true);
    }
    json.literal(b"false")?;
    Ok(false)
}

fn read_once<'input>(
    slot: &mut Option<Slot<'input>>,
    json: &mut Json<'input, '_>,
) -> Result<(), ParseError> {
    if slot.is_some() {
        return Err(ParseError::InvalidJson);
    }
    *slot = Some(json.slot_string()?);
    Ok(())
}

fn read_fields<'input>(json: &mut Json<'input, '_>) -> Result<Fields<'input>, ParseError> {
    json.byte(b'{')?;
    let mut fields = Fields::default();
    let mut first = true;
    loop {
        let Some(key) = json.member_key(first)? else {
            return Ok(fields);
        };
        first = false;
        if text_equals(key, b"reason") {
            if fields.reason.is_some() {
                return Err(ParseError::InvalidJson);
            }
            fields.reason = Some(read_reason(json)?);
        } else if text_equals(key, b"executable") {
            if fields.saw_executable {
                return Err(ParseError::InvalidJson);
            }
            fields.saw_executable = true;
            json.whitespace();
            if json.input.get(json.offset..json.offset + 4) == Some(b"null") {
                json.literal(b"null")?;
            } else {
                fields.executable = Some(json.slot_string()?);
            }
        } else if text_equals(key, b"profile") {
            if fields.saw_profile {
                return Err(ParseError::InvalidJson);
            }
            fields.saw_profile = true;
            fields.test_profile = json.profile_is_test()?;
        } else if text_equals(key, b"package_id") {
            read_once(&mut fields.package_id, json)?;
        } else if text_equals(key, b"manifest_path") {
            read_once(&mut fields.manifest_path, json)?;
        } else if text_equals(key, b"target") {
            json.read_target(&mut fields)?;
        } else if text_equals(key, b"message") {
            json.read_message(&mut fields)?;
        } else if text_equals(key, b"fresh") {
            fields.fresh = Some(read_success(json)?);
        } else if text_equals(key, b"success") {
            if fields.success.is_some() {
                return Err(ParseError::InvalidJson);
            }
            fields.success = Some(read_success(json)?);
        } else if text_equals(key, b"out_dir") {
            read_once(&mut fields.out_dir, json)?;
        } else if text_equals(key, b"env") {
            if fields.environment.is_some() {
                return Err(ParseError::InvalidJson);
            }
            fields.environment = Some(json.copy_pair_list()?);
        } else if text_equals(key, b"linked_paths") {
            if fields.linked_paths.is_some() {
                return Err(ParseError::InvalidJson);
            }
            fields.linked_paths = Some(json.copy_string_list()?);
        } else {
            json.skip_value(1)?;
        }
    }
}

fn read_reason(json: &mut Json<'_, '_>) -> Result<Reason, ParseError> {
    let reason = json.string(false)?;
    Ok(if text_equals(reason, b"compiler-artifact") {
        Reason::CompilerArtifact
    } else if text_equals(reason, b"build-script-executed") {
        Reason::BuildScriptExecuted
    } else if text_equals(reason, b"build-finished") {
        Reason::BuildFinished
    } else if text_equals(reason, b"compiler-message") {
        Reason::CompilerMessage
    } else {
        Reason::Other
    })
}

fn resolve<'text>(scratch: &'text [u8], slot: Slot<'text>) -> Result<&'text str, ParseError> {
    let bytes = match slot {
        Slot::Input(bytes) => bytes,
        Slot::Scratch(span) => &scratch[span.start..span.end],
    };
    str::from_utf8(bytes).map_err(|_| ParseError::InvalidUtf8)
}

/// Classifies one complete `cargo --message-format=json` line.
///
/// Unlike [`crate::parse_artifact`], a test artifact carries its package and
/// manifest next to its executable, and a build script's search paths are
/// gathered into one lent region of `scratch`. Strings without escapes stay
/// borrowed from `line`; escaped ones are decoded into `scratch`.
///
/// # Errors
///
/// Returns [`ParseError`] when the line exceeds the byte limit, the depth limit is invalid or exceeded, scratch is too small, the JSON or UTF-8 is malformed, or a test artifact or build script lacks its package id, manifest path, or output directory.
pub fn parse_cargo_message<'text>(
    line: &'text [u8],
    scratch: &'text mut [u8],
    max_record_bytes: usize,
    max_json_depth: usize,
) -> Result<CargoMessage<'text>, ParseError> {
    if line.len() > max_record_bytes {
        return Err(ParseError::RecordTooLarge);
    }
    if max_json_depth == 0 {
        return Err(ParseError::DepthExceeded);
    }
    if max_json_depth > MAX_JSON_DEPTH {
        return Err(ParseError::InvalidDepthLimit);
    }
    let mut json = Json {
        input: line,
        offset: 0,
        scratch,
        scratch_used: 0,
        max_depth: max_json_depth,
    };
    let fields = read_fields(&mut json)?;
    json.whitespace();
    if json.offset != line.len() {
        return Err(ParseError::InvalidJson);
    }
    let scratch: &'text [u8] = json.scratch;
    match fields.reason {
        Some(Reason::CompilerArtifact) if fields.test_profile => {
            let Some(executable) = fields.executable else {
                return Ok(CargoMessage::Compiled { fresh: fields.fresh == Some(true) });
            };
            let executable = resolve(scratch, executable)?;
            if executable.is_empty() {
                return Ok(CargoMessage::Compiled { fresh: fields.fresh == Some(true) });
            }
            let package_id = fields.package_id.ok_or(ParseError::MissingArtifactField)?;
            let manifest_path = fields
                .manifest_path
                .ok_or(ParseError::MissingArtifactField)?;
            Ok(CargoMessage::TestArtifact(TestArtifact {
                executable,
                package_id: resolve(scratch, package_id)?,
                manifest_path: resolve(scratch, manifest_path)?,
                target_name: fields
                    .target_name
                    .map_or(Ok(""), |slot| resolve(scratch, slot))?,
                target_kind: fields
                    .target_kind
                    .map_or(Ok(""), |slot| resolve(scratch, slot))?,
            }))
        }
        Some(Reason::CompilerArtifact) => {
            let built_bin = fields.target_kind.is_some_and(|slot| {
                resolve(scratch, slot).is_ok_and(|kind| kind == "bin")
            });
            match (built_bin, fields.executable, fields.package_id) {
                (true, Some(executable), Some(package_id)) => Ok(CargoMessage::BinArtifact {
                    package_id: resolve(scratch, package_id)?,
                    executable: resolve(scratch, executable)?,
                }),
                _ => Ok(CargoMessage::Compiled { fresh: fields.fresh == Some(true) }),
            }
        }
        Some(Reason::BuildFinished) => Ok(CargoMessage::BuildFinished {
            success: fields.success.ok_or(ParseError::MissingArtifactField)?,
        }),
        Some(Reason::CompilerMessage) => match (fields.level, fields.rendered) {
            (Some(level), Some(rendered)) => Ok(CargoMessage::Diagnostic {
                level: resolve(scratch, level)?,
                rendered: resolve(scratch, rendered)?,
            }),
            _ => Ok(CargoMessage::Ignored),
        },
        Some(Reason::BuildScriptExecuted) => {
            let package_id = fields.package_id.ok_or(ParseError::MissingArtifactField)?;
            let out_dir = fields.out_dir.ok_or(ParseError::MissingArtifactField)?;
            let empty = Span::default();
            let linked = fields.linked_paths.unwrap_or(empty);
            let environment = fields.environment.unwrap_or(empty);
            Ok(CargoMessage::BuildScript(BuildScript {
                package_id: resolve(scratch, package_id)?,
                out_dir: resolve(scratch, out_dir)?,
                linked_paths: LinkedPaths {
                    separated: resolve(scratch, Slot::Scratch(linked))?,
                },
                environment: BuildEnvironment {
                    separated: resolve(scratch, Slot::Scratch(environment))?,
                },
            }))
        }
        _ => Ok(CargoMessage::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use super::{CargoMessage, parse_cargo_message};

    const LIBRARY: &[u8] = br#"{"reason":"compiler-artifact","package_id":"path+file:///repo/proxima-model-interop#0.1.0","manifest_path":"/repo/proxima-model-interop/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"proxima_model_interop","src_path":"/repo/proxima-model-interop/src/lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":2,"test":true},"executable":"/repo/target/debug/deps/proxima_model_interop-3f2a1c9d8e7b6a50"}"#;
    const INTEGRATION: &[u8] = br#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228","manifest_path":"/repo/serde/Cargo.toml","target":{"kind":["test","bench"],"crate_types":["bin"],"name":"capability_matrix","src_path":"/repo/tests/capability_matrix.rs"},"profile":{"test":true},"executable":"/repo/target/debug/deps/capability_matrix-0a1b2c3d4e5f6071"}"#;

    fn artifact(line: &[u8]) -> (std::string::String, std::string::String, std::string::String, bool) {
        let mut scratch = [0; 4096];
        match parse_cargo_message(line, &mut scratch, 8192, 8).expect("real cargo line parses") {
            CargoMessage::TestArtifact(artifact) => (
                artifact.package_name().into(),
                artifact.target_name.into(),
                artifact.target_kind.into(),
                artifact.is_unit_test_target(),
            ),
            other => panic!("not a test artifact: {other:?}"),
        }
    }

    #[proxima::test]
    async fn target_name_kind_and_package_name_come_from_the_cargo_line() {
        assert_eq!(
            artifact(LIBRARY),
            ("proxima-model-interop".into(), "proxima_model_interop".into(), "lib".into(), true)
        );
        assert_eq!(
            artifact(INTEGRATION),
            ("serde".into(), "capability_matrix".into(), "test".into(), false)
        );
    }
}
