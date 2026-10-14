//! The agent projection of the render state: counts, one block per failure, the shared closing block, and an EXIT line.
//!
//! Composes [`Line`] and [`crate::failure_class::classify`]; it reads the same [`State`] plain reads and holds nothing of
//! its own. Every field of a failure block is a borrowed view of the captured bytes the driver already owns.

use super::{Event, Identity, State, Stream, label};
use crate::failure_class::{Absent, Class, Enforcer, Exit, Record, classify};
use crate::line::Line;
use crate::view_defaults::{OUTPUT_BYTES, OUTPUT_HEAD_BYTES};

const ESCAPE: u8 = 0x1b;
const CARRIAGE_RETURN: u8 = b'\r';
const CONTINUATION: &[u8] = b"         ";
const TRUNCATION_MARK: &[u8] = b"...\n";

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
    let (capture, truncated) = if stdout.0.is_empty() { stderr } else { stdout };
    let record = classify(capture, exit, u64::from(truncated)).or_else(|_| classify(stderr.0, exit, u64::from(stderr.1)));
    label(frame, b"FAIL");
    frame.text(id.0);
    frame.text(b"  ");
    frame.text(id.1);
    frame.text(b"\n");
    match record {
        Ok(found) => classified(frame, &found),
        Err(_) => unclassified(frame),
    }
    output(frame, capture, truncated);
    label(frame, b"END");
    frame.text(b"rerun: cargo val --no-fail-fast -E '(binary(=");
    frame.text(id.0);
    frame.text(b") & test(=");
    frame.text(id.1);
    frame.text(b"))'");
    frame.text(if truncated { b"  full: cargo test -- --exact --nocapture\n" } else { b"\n" });
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

fn output<const FRAME: usize>(frame: &mut Line<FRAME>, capture: &[u8], truncated: bool) {
    let shown_cut = capture.len() > OUTPUT_BYTES;
    let (head, tail) = if shown_cut {
        (&capture[..OUTPUT_HEAD_BYTES], &capture[capture.len() - (OUTPUT_BYTES - OUTPUT_HEAD_BYTES)..])
    } else {
        (capture, &capture[capture.len()..])
    };
    label(frame, b"OUTPUT");
    frame.text(b"bytes=");
    frame.number(capture.len());
    frame.text(b" shown=");
    frame.number(if shown_cut { OUTPUT_BYTES } else { capture.len() });
    frame.text(b" dropped=");
    frame.number(capture.len().saturating_sub(OUTPUT_BYTES));
    frame.text(b" esc_stripped=");
    frame.number(count_escapes(head) + count_escapes(tail));
    if truncated {
        frame.text(b" capture_truncated=yes");
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
