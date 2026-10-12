#![no_std]

#[cfg(any(test, feature = "edge-os"))]
extern crate std;

#[cfg(test)]
mod test_dir;

#[cfg(test)]
pub(crate) mod test_alloc;

use core::str;

pub mod build_defaults {
    include!(concat!(env!("OUT_DIR"), "/cargo_val_build.rs"));
}

pub mod cargo_message;
pub mod child_completion;
pub mod config;
pub mod discover_cargo;
pub mod filter_glob;
pub mod filterset;
pub mod ignored_set;
pub mod isolated_target;
pub mod metadata_scan;
pub mod parked_artifacts;
pub mod schedule;
pub mod stop;
pub mod summary;
pub mod test_environment;

#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod artifact_stream;
#[cfg(all(
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod cargo_source;
#[cfg(unix)]
pub mod edge_signal_unix;
#[cfg(all(test, feature = "edge-os", any(target_os = "macos", target_os = "linux")))]
pub(crate) mod edge_exit;
#[cfg(all(feature = "edge-os", target_os = "linux"))]
pub mod edge_exit_linux;
#[cfg(all(feature = "edge-os", target_os = "macos"))]
pub mod edge_exit_macos;
#[cfg(all(feature = "edge-os", unix))]
pub mod edge_stream_unix;
#[cfg(all(
    feature = "edge-os",
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives",
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))
))]
pub mod edge_output_unix;
pub mod cli;
pub mod host;
#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod host_event;
#[cfg(all(
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod discover_listing;
#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod outcomes;
#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod reporter;
#[cfg(all(
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
pub mod runner;
#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod selection;
#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod test_intent;
#[cfg(all(unix, feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod unix_arguments;

#[cfg(all(feature = "proxima-codec", feature = "proxima-primitives"))]
pub mod windows_arguments;

/// One Cargo artifact outcome returned by the record state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactRecord<'input, 'scratch> {
    /// An executable compiled with Cargo's test profile.
    TestExecutable(ExecutablePath<'input, 'scratch>),
    /// A valid Cargo message that does not describe a test executable.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutablePath<'input, 'scratch> {
    Borrowed(&'input str),
    Scratch(&'scratch str),
}

/// Bounded failures from parsing one Cargo JSON record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    RecordTooLarge,
    DepthExceeded,
    InvalidDepthLimit,
    ScratchTooSmall,
    MissingArtifactField,
    InvalidJson,
    InvalidUtf8,
}

const MAX_JSON_DEPTH: usize = 16;

#[derive(Clone, Copy)]
enum Text<'a> {
    Borrowed(&'a [u8]),
    Escaped(&'a [u8]),
    Decoded(usize),
}

struct Json<'input, 'scratch> {
    input: &'input [u8],
    offset: usize,
    scratch: &'scratch mut [u8],
    scratch_used: usize,
    max_depth: usize,
}

impl<'input> Json<'input, '_> {
    fn whitespace(&mut self) {
        while matches!(
            self.input.get(self.offset),
            Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            self.offset += 1;
        }
    }

    fn byte(&mut self, expected: u8) -> Result<(), ParseError> {
        self.whitespace();
        if self.input.get(self.offset) != Some(&expected) {
            return Err(ParseError::InvalidJson);
        }
        self.offset += 1;
        Ok(())
    }

    fn string(&mut self, decode: bool) -> Result<Text<'input>, ParseError> {
        self.whitespace();
        self.byte(b'"')?;
        let start = self.offset;
        let mut segment_start = start;
        let base = self.scratch_used;
        let mut decoded_length = 0;
        let mut escaped = false;
        loop {
            let value = *self.input.get(self.offset).ok_or(ParseError::InvalidJson)?;
            match value {
                b'"' => {
                    let end = self.offset;
                    self.offset += 1;
                    if escaped {
                        if !decode {
                            str::from_utf8(&self.input[segment_start..end])
                                .map_err(|_| ParseError::InvalidUtf8)?;
                        }
                        str::from_utf8(&self.scratch[base..base + decoded_length])
                            .map_err(|_| ParseError::InvalidUtf8)?;
                        return if decode {
                            Ok(Text::Decoded(decoded_length))
                        } else {
                            Ok(Text::Escaped(&self.input[start..end]))
                        };
                    }
                    let text = &self.input[start..end];
                    str::from_utf8(text).map_err(|_| ParseError::InvalidUtf8)?;
                    return Ok(Text::Borrowed(text));
                }
                b'\\' => {
                    if !decode {
                        str::from_utf8(&self.input[segment_start..self.offset])
                            .map_err(|_| ParseError::InvalidUtf8)?;
                    }
                    escaped = true;
                    let code = *self
                        .input
                        .get(self.offset + 1)
                        .ok_or(ParseError::InvalidJson)?;
                    if !decode && code != b'u' {
                        if !matches!(code, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                            return Err(ParseError::InvalidJson);
                        }
                        self.offset += 2;
                        continue;
                    }
                    if decode {
                        if !escaped || decoded_length == 0 {
                            let prefix = &self.input[start..self.offset];
                            copy_to_scratch(prefix, &mut self.scratch[base..], &mut decoded_length)?;
                        }
                        self.offset += 1;
                        decode_escape(
                            self.input,
                            &mut self.offset,
                            &mut self.scratch[base..],
                            &mut decoded_length,
                        )?;
                        segment_start = self.offset;
                    } else if code == b'u' {
                        self.offset += 2;
                        let first = skip_hex(self.input, &mut self.offset, 4)?;
                        if (0xd800..=0xdbff).contains(&first) {
                            if self.input.get(self.offset..self.offset + 2) != Some(b"\\u") {
                                return Err(ParseError::InvalidJson);
                            }
                            self.offset += 2;
                            let second = skip_hex(self.input, &mut self.offset, 4)?;
                            if !(0xdc00..=0xdfff).contains(&second) {
                                return Err(ParseError::InvalidJson);
                            }
                        } else if (0xdc00..=0xdfff).contains(&first) {
                            return Err(ParseError::InvalidJson);
                        }
                        segment_start = self.offset;
                    }
                }
                0..=0x1f => return Err(ParseError::InvalidJson),
                _ => {
                    if decode && escaped {
                        let byte = self.input[self.offset];
                        push(&mut self.scratch[base..], &mut decoded_length, byte)?;
                    }
                    self.offset += 1;
                }
            }
        }
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), ParseError> {
        self.whitespace();
        match self.input.get(self.offset) {
            Some(b'"') => {
                self.string(false)?;
            }
            Some(b'{') => {
                self.enter(depth)?;
                self.offset += 1;
                self.whitespace();
                if self.input.get(self.offset) == Some(&b'}') {
                    self.offset += 1;
                    return Ok(());
                }
                loop {
                    self.string(false)?;
                    self.byte(b':')?;
                    self.skip_value(depth + 1)?;
                    self.whitespace();
                    if self.input.get(self.offset) == Some(&b'}') {
                        self.offset += 1;
                        return Ok(());
                    }
                    self.byte(b',')?;
                    self.whitespace();
                    if self.input.get(self.offset) == Some(&b'}') {
                        return Err(ParseError::InvalidJson);
                    }
                }
            }
            Some(b'[') => {
                self.enter(depth)?;
                self.offset += 1;
                self.whitespace();
                if self.input.get(self.offset) == Some(&b']') {
                    self.offset += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    self.whitespace();
                    if self.input.get(self.offset) == Some(&b']') {
                        self.offset += 1;
                        return Ok(());
                    }
                    self.byte(b',')?;
                    self.whitespace();
                    if self.input.get(self.offset) == Some(&b']') {
                        return Err(ParseError::InvalidJson);
                    }
                }
            }
            Some(b't') => self.literal(b"true")?,
            Some(b'f') => self.literal(b"false")?,
            Some(b'n') => self.literal(b"null")?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            _ => return Err(ParseError::InvalidJson),
        }
        Ok(())
    }

    fn enter(&self, depth: usize) -> Result<(), ParseError> {
        if depth >= self.max_depth {
            Err(ParseError::DepthExceeded)
        } else {
            Ok(())
        }
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), ParseError> {
        let end = self.offset + literal.len();
        if self.input.get(self.offset..end) != Some(literal) {
            return Err(ParseError::InvalidJson);
        }
        self.offset = end;
        Ok(())
    }

    fn number(&mut self) -> Result<(), ParseError> {
        if self.input.get(self.offset) == Some(&b'-') {
            self.offset += 1;
        }
        match self.input.get(self.offset) {
            Some(b'0') => {
                self.offset += 1;
                if matches!(self.input.get(self.offset), Some(b'0'..=b'9')) {
                    return Err(ParseError::InvalidJson);
                }
            }
            Some(b'1'..=b'9') => {
                self.offset += 1;
                while matches!(self.input.get(self.offset), Some(b'0'..=b'9')) {
                    self.offset += 1;
                }
            }
            _ => return Err(ParseError::InvalidJson),
        }
        if self.input.get(self.offset) == Some(&b'.') {
            self.offset += 1;
            if !matches!(self.input.get(self.offset), Some(b'0'..=b'9')) {
                return Err(ParseError::InvalidJson);
            }
            while matches!(self.input.get(self.offset), Some(b'0'..=b'9')) {
                self.offset += 1;
            }
        }
        if matches!(self.input.get(self.offset), Some(b'e' | b'E')) {
            self.offset += 1;
            if matches!(self.input.get(self.offset), Some(b'+' | b'-')) {
                self.offset += 1;
            }
            if !matches!(self.input.get(self.offset), Some(b'0'..=b'9')) {
                return Err(ParseError::InvalidJson);
            }
            while matches!(self.input.get(self.offset), Some(b'0'..=b'9')) {
                self.offset += 1;
            }
        }
        Ok(())
    }
}

fn push(buffer: &mut [u8], length: &mut usize, byte: u8) -> Result<(), ParseError> {
    let cell = buffer.get_mut(*length).ok_or(ParseError::ScratchTooSmall)?;
    *cell = byte;
    *length += 1;
    Ok(())
}

fn copy_to_scratch(source: &[u8], buffer: &mut [u8], length: &mut usize) -> Result<(), ParseError> {
    for byte in source {
        push(buffer, length, *byte)?;
    }
    Ok(())
}

fn skip_hex(input: &[u8], offset: &mut usize, count: usize) -> Result<u32, ParseError> {
    let mut value = 0_u32;
    for _ in 0..count {
        let digit = match input.get(*offset) {
            Some(b'0'..=b'9') => input[*offset] - b'0',
            Some(b'a'..=b'f') => input[*offset] - b'a' + 10,
            Some(b'A'..=b'F') => input[*offset] - b'A' + 10,
            _ => return Err(ParseError::InvalidJson),
        };
        value = (value << 4) | u32::from(digit);
        *offset += 1;
    }
    Ok(value)
}

fn encode_utf8(buffer: &mut [u8], length: &mut usize, scalar: u32) -> Result<(), ParseError> {
    let character = char::from_u32(scalar).ok_or(ParseError::InvalidJson)?;
    let mut encoded = [0_u8; 4];
    let bytes = character.encode_utf8(&mut encoded).as_bytes();
    copy_to_scratch(bytes, buffer, length)
}

fn text_equals(text: Text<'_>, expected: &[u8]) -> bool {
    match text {
        Text::Borrowed(bytes) => bytes == expected,
        Text::Decoded(_) => false,
        Text::Escaped(bytes) => {
            let mut offset = 0;
            let mut expected_offset = 0;
            while offset < bytes.len() {
                let scalar = if bytes[offset] == b'\\' {
                    offset += 1;
                    let Some(escape) = bytes.get(offset) else {
                        return false;
                    };
                    offset += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => u32::from(*escape),
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'u' => {
                            let Ok(first) = skip_hex(bytes, &mut offset, 4) else {
                                return false;
                            };
                            if (0xd800..=0xdbff).contains(&first) {
                                if bytes.get(offset..offset + 2) != Some(b"\\u") {
                                    return false;
                                }
                                offset += 2;
                                let Ok(second) = skip_hex(bytes, &mut offset, 4) else {
                                    return false;
                                };
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return false;
                                }
                                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            } else if (0xdc00..=0xdfff).contains(&first) {
                                return false;
                            } else {
                                first
                            }
                        }
                        _ => return false,
                    }
                } else {
                    let scalar = u32::from(bytes[offset]);
                    offset += 1;
                    scalar
                };
                let matches_expected = u8::try_from(scalar)
                    .is_ok_and(|byte| expected.get(expected_offset) == Some(&byte));
                if !matches_expected {
                    return false;
                }
                expected_offset += 1;
            }
            expected_offset == expected.len()
        }
    }
}

fn decode_escape(
    input: &[u8],
    offset: &mut usize,
    buffer: &mut [u8],
    length: &mut usize,
) -> Result<(), ParseError> {
    let escape = *input.get(*offset).ok_or(ParseError::InvalidJson)?;
    *offset += 1;
    match escape {
        b'"' | b'\\' | b'/' => push(buffer, length, escape),
        b'b' => push(buffer, length, 8),
        b'f' => push(buffer, length, 12),
        b'n' => push(buffer, length, b'\n'),
        b'r' => push(buffer, length, b'\r'),
        b't' => push(buffer, length, b'\t'),
        b'u' => {
            let first = skip_hex(input, offset, 4)?;
            let scalar = if (0xd800..=0xdbff).contains(&first) {
                if input.get(*offset..*offset + 2) != Some(b"\\u") {
                    return Err(ParseError::InvalidJson);
                }
                *offset += 2;
                let second = skip_hex(input, offset, 4)?;
                if !(0xdc00..=0xdfff).contains(&second) {
                    return Err(ParseError::InvalidJson);
                }
                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
            } else if (0xdc00..=0xdfff).contains(&first) {
                return Err(ParseError::InvalidJson);
            } else {
                first
            };
            encode_utf8(buffer, length, scalar)
        }
        _ => Err(ParseError::InvalidJson),
    }
}

/// Selects a test executable from one complete `cargo --message-format=json` line.
///
/// The caller owns the input, scratch storage, byte limit, and effective depth
/// limit. Depth is capped to keep parser recursion bounded on small stacks.
///
/// # Errors
///
/// Returns [`ParseError`] when the line exceeds the byte limit, the depth limit is invalid or exceeded, scratch is too small, or the JSON or UTF-8 is malformed.
pub fn parse_artifact<'input, 'scratch>(
    line: &'input [u8],
    scratch: &'scratch mut [u8],
    max_record_bytes: usize,
    max_json_depth: usize,
) -> Result<ArtifactRecord<'input, 'scratch>, ParseError> {
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
    json.byte(b'{')?;
    let mut reason = false;
    let mut test_profile = false;
    let mut saw_reason = false;
    let mut saw_profile = false;
    let mut saw_executable = false;
    let mut executable = None;
    json.whitespace();
    if json.input.get(json.offset) != Some(&b'}') {
        loop {
            let key = json.string(false)?;
            json.byte(b':')?;
            if text_equals(key, b"reason") {
                if saw_reason {
                    return Err(ParseError::InvalidJson);
                }
                saw_reason = true;
                reason = text_equals(json.string(false)?, b"compiler-artifact");
            } else if text_equals(key, b"executable") {
                if saw_executable {
                    return Err(ParseError::InvalidJson);
                }
                saw_executable = true;
                json.whitespace();
                if json.input.get(json.offset..json.offset + 4) == Some(b"null") {
                    json.literal(b"null")?;
                    executable = None;
                } else {
                    executable = Some(json.string(true)?);
                }
            } else if text_equals(key, b"profile") {
                if saw_profile {
                    return Err(ParseError::InvalidJson);
                }
                saw_profile = true;
                test_profile = parse_nested_boolean(&mut json, b"test")?;
            } else {
                json.skip_value(1)?;
            }
            json.whitespace();
            if json.input.get(json.offset) == Some(&b'}') {
                break;
            }
            json.byte(b',')?;
            json.whitespace();
            if json.input.get(json.offset) == Some(&b'}') {
                return Err(ParseError::InvalidJson);
            }
        }
    }
    json.byte(b'}')?;
    json.whitespace();
    if json.offset != line.len() {
        return Err(ParseError::InvalidJson);
    }
    if !reason || !test_profile {
        return Ok(ArtifactRecord::Ignored);
    }
    match executable {
        Some(Text::Borrowed(path)) if !path.is_empty() => Ok(ArtifactRecord::TestExecutable(
            ExecutablePath::Borrowed(str::from_utf8(path).map_err(|_| ParseError::InvalidUtf8)?),
        )),
        Some(Text::Decoded(length)) if length > 0 => {
            Ok(ArtifactRecord::TestExecutable(ExecutablePath::Scratch(
                str::from_utf8(&json.scratch[..length]).map_err(|_| ParseError::InvalidUtf8)?,
            )))
        }
        _ => Ok(ArtifactRecord::Ignored),
    }
}

fn parse_nested_boolean(json: &mut Json<'_, '_>, field: &[u8]) -> Result<bool, ParseError> {
    json.enter(1)?;
    json.byte(b'{')?;
    json.whitespace();
    let mut value = false;
    let mut saw_field = false;
    if json.input.get(json.offset) != Some(&b'}') {
        loop {
            let key = json.string(false)?;
            json.byte(b':')?;
            if text_equals(key, field) {
                if saw_field {
                    return Err(ParseError::InvalidJson);
                }
                saw_field = true;
                value = json.input.get(json.offset..json.offset + 4) == Some(b"true");
                if value {
                    json.offset += 4;
                } else if json.input.get(json.offset..json.offset + 5) == Some(b"false") {
                    json.offset += 5;
                } else {
                    return Err(ParseError::InvalidJson);
                }
            } else {
                json.skip_value(2)?;
            }
            json.whitespace();
            if json.input.get(json.offset) == Some(&b'}') {
                break;
            }
            json.byte(b',')?;
            json.whitespace();
            if json.input.get(json.offset) == Some(&b'}') {
                return Err(ParseError::InvalidJson);
            }
        }
    }
    json.byte(b'}')?;
    Ok(value)
}
