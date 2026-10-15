//! Failure classifier: a test's captured output plus the driver's outcome facts in, a borrowed
//! record out.
//!
//! Shape: a transform `In = (capture bytes, Exit, dropped)`, `Out = Result<Record, Reject>`. It is
//! sans-IO and core-only; every slice in the record points into `capture`. The driver wires it into
//! the pipe that settles a test, so this module composes with nothing yet by design.
//!
//! Scans use `memchr` (`memchr`, `memchr2`, `memrchr`, `memmem`). Nothing here allocates.

use memchr::{memchr, memchr2, memmem, memrchr};

const PANIC_MARK: &[u8] = b"panicked at ";
const THREAD_LINE: &[u8] = b"thread '";
const BACKTRACE_MARK: &[u8] = b"stack backtrace:";
const RELAY_MARK: &[u8] = b" (at ";
const FRAME_MARK: &[u8] = b"at ";
const HARNESS_BODY_TIMEOUT: &[u8] = b"proxima::test: body did not complete within the timeout";
const HARNESS_DISPATCH_TIMEOUT: &[u8] =
    b"proxima::test: worker never began polling the test body within the dispatch timeout";
const MESSAGE_ENDS: [&[u8]; 5] = [
    b"\nnote: run with `RUST_BACKTRACE",
    b"\nstack backtrace:",
    b"\nfailures:",
    b"\n---- ",
    b"\nthread '",
];
const BLOCK_ENDS: [&[u8]; 2] = [b"\n---- ", b"\nfailures:"];

/// How the test process ended, as the driver knows it. `DeadlineKilled` is val's own deadline,
/// which is not a signal the test chose, so it is a variant of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    DeadlineKilled,
}

impl Exit {
    /// Reads the driver's session status: on unix a negative status is the negated number of the signal that ended
    /// the child (`session_exit_code` in `run.rs`); on windows a status with the NTSTATUS error bits set (`0xC000_0000`,
    /// as `abort` and an access violation exit with) is a crash and keeps the raw status as its number; every other status is an exit code.
    #[must_use]
    pub const fn from_status(status: i32) -> Self {
        if cfg!(unix) && status < 0 {
            Self::Signal(status.saturating_neg())
        } else if cfg!(windows) && status.cast_unsigned() & 0xC000_0000 == 0xC000_0000 {
            Self::Signal(status)
        } else {
            Self::Code(status)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enforcer {
    Val,
    Harness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Assert,
    Panic,
    Timeout { enforced_by: Enforcer },
    Hang,
    Crash { signal: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location<'a> {
    pub file: &'a [u8],
    pub line: u32,
    pub col: u32,
}

/// Why a record has no user location. `External` keeps the first library site so a renderer can
/// print `external:<path>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absent<'a> {
    NoPanicSite,
    External(Location<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    pub class: Class,
    pub location: Result<Location<'a>, Absent<'a>>,
    pub message: &'a [u8],
    pub sides: Option<(&'a [u8], &'a [u8])>,
    pub dropped: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Succeeded,
    NoEvidence,
}

struct Panic<'a> {
    site: Location<'a>,
    message: &'a [u8],
    resume: usize,
}

/// Classifies one settled test. `dropped` is the count of capture bytes the driver discarded.
///
/// # Errors
///
/// `Reject::Succeeded` when the process exited 0; `Reject::NoEvidence` when it failed but the
/// capture holds no panic and the exit carries no signal.
pub fn classify(capture: &[u8], exit: Exit, dropped: u64) -> Result<Record<'_>, Reject> {
    match exit {
        Exit::Code(0) => Err(Reject::Succeeded),
        Exit::DeadlineKilled => Ok(deadline_record(capture, dropped)),
        Exit::Code(_) | Exit::Signal(_) => match find_panic(capture) {
            Some(panic) => Ok(panic_record(capture, &panic, dropped)),
            None => silent_record(exit, dropped),
        },
    }
}

fn deadline_record(capture: &[u8], dropped: u64) -> Record<'_> {
    let class = if capture.is_empty() {
        Class::Hang
    } else {
        Class::Timeout { enforced_by: Enforcer::Val }
    };
    Record { class, location: Err(Absent::NoPanicSite), message: &capture[..0], sides: None, dropped }
}

fn silent_record(exit: Exit, dropped: u64) -> Result<Record<'static>, Reject> {
    match exit {
        Exit::Signal(signal) => Ok(Record {
            class: Class::Crash { signal },
            location: Err(Absent::NoPanicSite),
            message: &[],
            sides: None,
            dropped,
        }),
        Exit::Code(_) | Exit::DeadlineKilled => Err(Reject::NoEvidence),
    }
}

fn panic_record<'a>(capture: &'a [u8], panic: &Panic<'a>, dropped: u64) -> Record<'a> {
    let message = panic.message;
    let class = message_class(message);
    let sides = match class {
        Class::Assert => assert_sides(message),
        Class::Panic | Class::Timeout { .. } | Class::Hang | Class::Crash { .. } => None,
    };
    Record { class, location: user_location(capture, panic), message, sides, dropped }
}

fn message_class(message: &[u8]) -> Class {
    if message == HARNESS_BODY_TIMEOUT || message == HARNESS_DISPATCH_TIMEOUT {
        Class::Timeout { enforced_by: Enforcer::Harness }
    } else if message.starts_with(b"assertion") {
        Class::Assert
    } else {
        Class::Panic
    }
}

fn find_panic(capture: &[u8]) -> Option<Panic<'_>> {
    let mut from = 0;
    while let Some(relative) = memmem::find(&capture[from..], PANIC_MARK) {
        let mark = from + relative;
        let line_start = memrchr(b'\n', &capture[..mark]).map_or(0, |index| index + 1);
        let start = mark + PANIC_MARK.len();
        if capture[line_start..].starts_with(THREAD_LINE) {
            return if capture.get(start) == Some(&b'\'') {
                old_panic(capture, start)
            } else {
                current_panic(capture, start)
            };
        }
        from = start;
    }
    None
}

fn current_panic(capture: &[u8], start: usize) -> Option<Panic<'_>> {
    let line_stop = line_end(capture, start);
    let site_text = trim_end(&capture[start..line_stop]);
    let site = parse_site(site_text.strip_suffix(b":").unwrap_or(site_text))?;
    let message_start = (line_stop + 1).min(capture.len());
    let message_stop = message_end(capture, message_start);
    let message = trim_end(&capture[message_start..message_stop]);
    Some(Panic { site, message, resume: message_stop })
}

// the pre-1.73 header is `panicked at 'message', file:line:col`; the message may span lines, so
// the location is the first line whose tail after `', ` parses as a site
fn old_panic(capture: &[u8], quote: usize) -> Option<Panic<'_>> {
    let message_start = quote + 1;
    let mut line_start = message_start;
    while line_start < capture.len() {
        let line_stop = line_end(capture, line_start);
        let line = &capture[line_start..line_stop];
        if let Some(index) = memmem::rfind(line, b"', ")
            && let Some(site) = parse_site(trim_end(&line[index + 3..]))
        {
            let message = &capture[message_start..line_start + index];
            return Some(Panic { site, message, resume: line_stop });
        }
        line_start = line_stop + 1;
    }
    None
}

fn message_end(capture: &[u8], from: usize) -> usize {
    let tail = &capture[from..];
    let stop = MESSAGE_ENDS.iter().filter_map(|mark| memmem::find(tail, mark)).min();
    from + stop.unwrap_or(tail.len())
}

fn user_location<'a>(capture: &'a [u8], panic: &Panic<'a>) -> Result<Location<'a>, Absent<'a>> {
    if !is_library(panic.site.file) {
        return Ok(panic.site);
    }
    if let Some(relayed) = relayed_site(panic.message).filter(|site| !is_library(site.file)) {
        return Ok(relayed);
    }
    backtrace_user_frame(capture, panic.resume).ok_or(Absent::External(panic.site))
}

// proxima-test re-panics with `<message> (at file:line:col)`, so the original site rides the message
fn relayed_site(message: &[u8]) -> Option<Location<'_>> {
    let body = message.strip_suffix(b")")?;
    let index = memmem::rfind(body, RELAY_MARK)?;
    parse_site(&body[index + RELAY_MARK.len()..])
}

fn backtrace_user_frame(capture: &[u8], from: usize) -> Option<Location<'_>> {
    let tail = capture.get(from..)?;
    let begin = memmem::find(tail, BACKTRACE_MARK)?;
    let block = &tail[begin..];
    let block = &block[..BLOCK_ENDS.iter().filter_map(|mark| memmem::find(block, mark)).min().unwrap_or(block.len())];
    let mut cursor = 0;
    while cursor < block.len() {
        let stop = line_end(block, cursor);
        let line = trim_end(trim_start(&block[cursor..stop]));
        let site = line.strip_prefix(FRAME_MARK).and_then(parse_site);
        if let Some(site) = site.filter(|site| !is_library(site.file)) {
            return Some(site);
        }
        cursor = stop + 1;
    }
    None
}

/// True for the paths a user never wrote: the cargo registry, `<crate>-<semver>/` source
/// directories, the rustc sysroot library, and proxima-test's harness.
#[must_use]
pub fn is_library(path: &[u8]) -> bool {
    let mut previous: &[u8] = &[];
    let mut first = true;
    let mut harness_crate = false;
    let mut last: &[u8] = &[];
    for component in components(path).filter(|component| !component.is_empty()) {
        let sysroot = first && (component == b"rustc" || component == b"library");
        let registry = previous == b".cargo" && component == b"registry";
        if sysroot || registry || component == b"rustlib" || is_versioned(component) {
            return true;
        }
        harness_crate |= component.starts_with(b"proxima-test");
        (previous, last, first) = (component, component, false);
    }
    harness_crate && last == b"harness.rs"
}

fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = Some(path);
    core::iter::from_fn(move || {
        let current = rest?;
        let split = memchr2(b'/', b'\\', current);
        rest = split.map(|index| &current[index + 1..]);
        Some(split.map_or(current, |index| &current[..index]))
    })
}

// `name-1.2.3`: a dash, then three dot-separated numbers
fn is_versioned(component: &[u8]) -> bool {
    let Some(dash) = memrchr(b'-', component).filter(|&index| index > 0) else {
        return false;
    };
    let mut parts = component[dash + 1..].splitn(3, |&byte| byte == b'.');
    let major = parts.next().is_some_and(is_number);
    let minor = parts.next().is_some_and(is_number);
    let patch = parts.next().is_some_and(|part| part.first().is_some_and(u8::is_ascii_digit));
    major && minor && patch
}

fn is_number(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

// windows backtrace frames carry `file:line` with no column; the column is then 0
fn parse_site(text: &[u8]) -> Option<Location<'_>> {
    let (rest, last) = split_number(text)?;
    match split_number(rest) {
        Some((file, line)) if !file.is_empty() => Some(Location { file, line, col: last }),
        Some(_) | None => (!rest.is_empty()).then_some(Location { file: rest, line: last, col: 0 }),
    }
}

fn split_number(text: &[u8]) -> Option<(&[u8], u32)> {
    let colon = memrchr(b':', text)?;
    Some((&text[..colon], parse_number(&text[colon + 1..])?))
}

fn parse_number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0_u32, |total, &byte| {
        let digit = byte.is_ascii_digit().then(|| u32::from(byte - b'0'))?;
        total.checked_mul(10)?.checked_add(digit)
    })
}

// libtest prints `  left: 1` / ` right: 2`; before 1.73 the values were wrapped as `` `1`, ``
fn assert_sides(message: &[u8]) -> Option<(&[u8], &[u8])> {
    let legacy = memmem::find(message, b"`(left").is_some();
    let (mut left, mut right) = (None, None);
    let mut cursor = 0;
    while cursor < message.len() {
        let stop = line_end(message, cursor);
        let line = trim_start(&message[cursor..stop]);
        left = left.or_else(|| line.strip_prefix(b"left: ").map(|value| side_value(value, legacy)));
        right = right.or_else(|| line.strip_prefix(b"right: ").map(|value| side_value(value, legacy)));
        cursor = stop + 1;
    }
    Some((left?, right?))
}

fn side_value(value: &[u8], legacy: bool) -> &[u8] {
    let value = trim_end(value);
    if !legacy {
        return value;
    }
    let value = value.strip_suffix(b",").unwrap_or(value);
    value.strip_prefix(b"`").and_then(|inner| inner.strip_suffix(b"`")).unwrap_or(value)
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    memchr(b'\n', &bytes[from..]).map_or(bytes.len(), |index| from + index)
}

fn trim_start(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = bytes {
        bytes = rest;
    }
    bytes
}

fn trim_end(mut bytes: &[u8]) -> &[u8] {
    while let [rest @ .., b'\n' | b'\r'] = bytes {
        bytes = rest;
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_ASSERT: &[u8] = b"running 1 test\ntest checksum ... FAILED\n\nfailures:\n\n---- checksum stdout ----\nthread 'checksum' panicked at 'assertion failed: `(left == right)`\n  left: `[1, 2, 3]`,\n right: `[1, 2, 4]`', src/lib.rs:9:9\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    checksum\n";

    const WINDOWS_HARNESS_TIMEOUT: &[u8] = br"
running 1 test
test failing::body_never_completes ... FAILED

failures:

---- failing::body_never_completes stdout ----

thread 'failing::body_never_completes' (1400) panicked at C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\proxima-test-0.1.0\src\harness.rs:174:21:
proxima::test: body did not complete within the timeout
stack backtrace:
   0: std::panicking::panic_handler
             at /rustc/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library\std\src\panicking.rs:679
   3: proxima_test::harness::finish
             at C:\Users\runneradmin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\proxima-test-0.1.0\src\harness.rs:174
   5: cargo_val_failure_harness_fixture::failing::body_never_completes
             at .\tests\fixtures\failure_harness\src\lib.rs:3
note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.


failures:
    failing::body_never_completes

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.30s
";

    #[test]
    fn windows_backtrace_frames_without_a_column_still_yield_the_user_frame() {
        let record = classify(WINDOWS_HARNESS_TIMEOUT, Exit::Code(101), 0).expect("a windows timeout classifies");

        assert_eq!(record.class, Class::Timeout { enforced_by: Enforcer::Harness });
        assert_eq!(
            record.location,
            Ok(Location { file: br".\tests\fixtures\failure_harness\src\lib.rs", line: 3, col: 0 })
        );
    }

    #[test]
    fn legacy_header_and_backticked_sides_parse() {
        let record = classify(LEGACY_ASSERT, Exit::Code(101), 0).expect("legacy assert is a failure");

        assert_eq!(record.class, Class::Assert);
        assert_eq!(record.location, Ok(Location { file: b"src/lib.rs", line: 9, col: 9 }));
        assert_eq!(record.sides, Some((&b"[1, 2, 3]"[..], &b"[1, 2, 4]"[..])));
    }

    #[test]
    fn garbage_is_rejected_and_never_guessed_at() {
        let noise: &[u8] = b"\x00\xff panicked at \n\n thread ' panicked at :::\n";

        assert_eq!(classify(noise, Exit::Code(101), 0), Err(Reject::NoEvidence));
        assert_eq!(classify(b"", Exit::Code(1), 0), Err(Reject::NoEvidence));
        assert_eq!(classify(noise, Exit::Code(0), 0), Err(Reject::Succeeded));
    }

    #[test]
    fn library_paths_are_recognised_and_user_paths_are_not() {
        assert!(is_library(b"/Users/dev/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/bytes-1.12.1/src/bytes.rs"));
        assert!(is_library(b"/rustc/b940084d7/library/core/src/panicking.rs"));
        assert!(is_library(b"C:\\Users\\dev\\.cargo\\registry\\src\\x\\serde-1.0.200\\src\\lib.rs"));
        assert!(is_library(b"/work/vendor/proxima-test/src/harness.rs"));
        assert!(!is_library(b"./src/lib.rs"));
        assert!(!is_library(b"/work/proxima-test/tests/timeout.rs"));
        assert!(!is_library(b"/work/cargo-val/src/driver.rs"));
    }
}
