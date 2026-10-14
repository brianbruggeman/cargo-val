//! The agent projection of the render state: counts, one block per failure, the shared closing block, and an EXIT line.
//!
//! Composes [`Line`] and [`crate::failure_class::classify`]; it reads the same [`State`] plain reads and holds nothing of
//! its own. Every field of a failure block is a borrowed view of the captured bytes the driver already owns.

use memchr::{memchr, memmem};

use super::{Event, Identity, State, Stream, label};
use crate::failure_class::{Absent, Class, Enforcer, Exit, Record, classify};
use crate::line::Line;
use crate::view_defaults::{OUTPUT_BYTES, OUTPUT_HEAD_BYTES};

const ESCAPE: u8 = 0x1b;
const CARRIAGE_RETURN: u8 = b'\r';
const CONTINUATION: &[u8] = b"         ";
const TRUNCATION_MARK: &[u8] = b"...\n";
const SECTION_OPEN: &[u8] = b"---- ";
const SECTION_CLOSE: &[u8] = b" stdout ----";
const FAILURES_LIST: &[u8] = b"\nfailures:\n";

impl<const RERUN: usize> State<'_, RERUN> {
    pub(super) fn project_agent<const FRAME: usize>(&self, event: &Event<'_>, frame: &mut Line<FRAME>) {
        match *event {
            Event::Begin { version, why: Some(why), .. } => header(frame, version, why),
            Event::Captured { id, stdout, stderr, exit } => failure_block(frame, id, [stdout, stderr], exit),
            Event::Interrupted { signal } => super::interrupted(frame, signal),
            Event::Finished { passed, failed, ignored, not_run, wall_ms, manifest, discovered, filtered, exit } => {
                if passed + failed == 0 {
                    no_tests_note(frame, [discovered, filtered, ignored]);
                }
                self.closing(frame, [passed, failed, ignored, not_run], wall_ms, manifest, Some([discovered, filtered]));
                exit_line(frame, exit);
            }
            _ => {}
        }
    }
}

fn header<const FRAME: usize>(frame: &mut Line<FRAME>, version: &[u8], why: &[u8]) {
    frame.text(b"cargo-val ");
    frame.text(version);
    frame.text(b"  output=agent why=");
    frame.text(why);
    frame.text(b"  schema=1\n");
}

pub(super) fn exit_word(code: u8) -> &'static [u8] {
    match code {
        0 => b"green",
        1 => b"failed",
        4 => b"no_tests",
        _ => b"exited",
    }
}

pub(super) fn exit_line<const FRAME: usize>(frame: &mut Line<FRAME>, code: u8) {
    label(frame, b"EXIT");
    frame.number(usize::from(code));
    frame.text(b"  ");
    frame.text(exit_word(code));
    frame.text(b"\n");
}

pub(super) fn no_tests_note<const FRAME: usize>(frame: &mut Line<FRAME>, [discovered, filtered, ignored]: [usize; 3]) {
    label(frame, b"NOTE");
    frame.text(b"no tests ran: discovered=");
    frame.number(discovered);
    frame.text(b" filtered=");
    frame.number(filtered);
    frame.text(b" ignored=");
    frame.number(ignored);
    frame.text(b"\n");
}

fn failure_block<const FRAME: usize>(
    frame: &mut Line<FRAME>,
    id: Identity<'_>,
    [stdout, stderr]: [Stream<'_>; 2],
    exit: Exit,
) {
    let (capture, dropped) = if stdout.0.is_empty() { stderr } else { stdout };
    let record = classify(capture, exit, dropped).or_else(|_| classify(stderr.0, exit, stderr.1));
    label(frame, b"FAIL");
    frame.text(id.0);
    frame.text(b"  ");
    frame.text(id.1);
    frame.text(b"\n");
    match record {
        Ok(found) => classified(frame, &found),
        Err(_) => unclassified(frame),
    }
    output(frame, capture, dropped);
    label(frame, b"END");
    frame.text(b"rerun: cargo val --no-fail-fast -E '(binary(=");
    frame.text(id.0);
    frame.text(b") & test(=");
    frame.text(id.1);
    frame.text(b"))'");
    frame.text(if dropped != 0 { b"  full: cargo test -- --exact --nocapture\n" } else { b"\n" });
}

fn classified<const FRAME: usize>(frame: &mut Line<FRAME>, record: &Record<'_>) {
    frame.text(CONTINUATION);
    class_words(frame, record.class);
    frame.text(b"  ");
    match record.location {
        Ok(site) => location(frame, b"", site.file, site.line, site.col),
        Err(Absent::External(site)) => location(frame, b"external:", site.file, site.line, site.col),
        Err(Absent::NoPanicSite) => frame.text(b"location=absent:no_panic_site"),
    }
    frame.text(b"\n");
    for (position, line) in record.message.split(|byte| *byte == b'\n').enumerate() {
        if position != 0 {
            frame.text(b"\n");
        }
        frame.text(CONTINUATION);
        frame.text(line);
    }
    frame.text(b"\n");
}

fn unclassified<const FRAME: usize>(frame: &mut Line<FRAME>) {
    frame.text(CONTINUATION);
    frame.text(b"UNCLASSIFIED  location=absent:no_panic_in_capture\n");
    frame.text(CONTINUATION);
    frame.text(b"message=absent:no_panic_in_capture\n");
}

fn class_words<const FRAME: usize>(frame: &mut Line<FRAME>, class: Class) {
    match class {
        Class::Assert => frame.text(b"ASSERT"),
        Class::Panic => frame.text(b"PANIC"),
        Class::Hang => frame.text(b"HANG"),
        Class::Timeout { enforced_by } => {
            frame.text(b"TIMEOUT enforced_by=");
            frame.text(match enforced_by {
                Enforcer::Val => b"val",
                Enforcer::Harness => b"harness",
            });
        }
        Class::Crash { signal } => {
            frame.text(b"CRASH signal=");
            frame.number(usize::try_from(signal).unwrap_or(0));
        }
    }
}

fn location<const FRAME: usize>(frame: &mut Line<FRAME>, prefix: &[u8], file: &[u8], line: u32, col: u32) {
    frame.text(prefix);
    frame.text(file);
    frame.text(b":");
    frame.number(line as usize);
    frame.text(b":");
    frame.number(col as usize);
}

fn output<const FRAME: usize>(frame: &mut Line<FRAME>, capture: &[u8], dropped: u64) {
    let body = libtest_body(capture);
    let shown_cut = body.len() > OUTPUT_BYTES;
    let (head, tail) = if shown_cut {
        (&body[..OUTPUT_HEAD_BYTES], &body[body.len() - (OUTPUT_BYTES - OUTPUT_HEAD_BYTES)..])
    } else {
        (body, &body[body.len()..])
    };
    label(frame, b"OUTPUT");
    let shown = if shown_cut { OUTPUT_BYTES } else { body.len() };
    let total = capture.len().saturating_add(usize::try_from(dropped).unwrap_or(usize::MAX));
    frame.text(b"bytes=");
    frame.number(total);
    frame.text(b" shown=");
    frame.number(shown);
    frame.text(b" dropped=");
    frame.number(total - shown);
    frame.text(b" esc_stripped=");
    frame.number(count_escapes(head) + count_escapes(tail));
    if dropped != 0 {
        frame.text(b" capture_dropped=");
        frame.number(usize::try_from(dropped).unwrap_or(usize::MAX));
    }
    frame.text(b"\n");
    clean(frame, head);
    if shown_cut {
        frame.text(b"\n");
        frame.text(TRUNCATION_MARK);
    }
    clean(frame, tail);
    if frame_open(frame) {
        frame.text(b"\n");
    }
}

// the verbatim test output sits between libtest's `---- name stdout ----` header and the closing `failures:` list; a run
// that died before printing either keeps only what follows the leading `running` and `test ... ` lines
fn libtest_body(capture: &[u8]) -> &[u8] {
    let Some(start) = section_start(capture) else {
        return unframed(capture);
    };
    let body = &capture[start..];
    let end = memmem::rfind(body, FAILURES_LIST).unwrap_or(body.len());
    body[..end].trim_ascii_end()
}

fn section_start(capture: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(relative) = memmem::find(&capture[from..], SECTION_OPEN) {
        let at = from + relative;
        let line_end = memchr(b'\n', &capture[at..]).map(|index| at + index)?;
        if (at == 0 || capture[at - 1] == b'\n') && capture[at..line_end].ends_with(SECTION_CLOSE) {
            return Some(line_end + 1);
        }
        from = at + SECTION_OPEN.len();
    }
    None
}

fn unframed(capture: &[u8]) -> &[u8] {
    let mut rest = capture;
    loop {
        let line_end = memchr(b'\n', rest).unwrap_or(rest.len());
        if !is_framing(&rest[..line_end]) || rest.is_empty() {
            return rest;
        }
        rest = &rest[(line_end + 1).min(rest.len())..];
    }
}

fn is_framing(line: &[u8]) -> bool {
    let counted = line.strip_prefix(b"running ").is_some_and(|tail| tail.ends_with(b" test") || tail.ends_with(b" tests"));
    let progress = line.starts_with(b"test ") && memmem::find(line, b" ... ").is_some();
    line.trim_ascii().is_empty() || counted || progress || line.starts_with(b"test result: ")
}

fn frame_open<const FRAME: usize>(frame: &Line<FRAME>) -> bool {
    frame.bytes().last().is_some_and(|byte| *byte != b'\n')
}

fn count_escapes(bytes: &[u8]) -> usize {
    memchr::memchr_iter(ESCAPE, bytes).count()
}

fn clean<const FRAME: usize>(frame: &mut Line<FRAME>, bytes: &[u8]) {
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            ESCAPE => position = after_sequence(bytes, position),
            CARRIAGE_RETURN => position += 1,
            _ => {
                let run = bytes[position..].iter().take_while(|byte| !matches!(**byte, ESCAPE | CARRIAGE_RETURN)).count();
                frame.text(&bytes[position..position + run]);
                position += run;
            }
        }
    }
}

fn after_sequence(bytes: &[u8], escape: usize) -> usize {
    let mut position = escape + 1;
    if bytes.get(position) == Some(&b'[') {
        position += 1;
        while position < bytes.len() && !bytes[position].is_ascii_alphabetic() {
            position += 1;
        }
    }
    (position + 1).min(bytes.len())
}
