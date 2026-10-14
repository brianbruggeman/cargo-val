//! The json projection of the render state: JSON Lines, one record per event, `"v":1` and `"t"` first.
//!
//! A sibling of [`super::agent`]: it reads the same [`State`], reuses the same classifier, libtest-body cut and escape
//! strip, and appends into the same [`Line`] frame, so nothing about "what failed and why" is held twice. Strings are
//! escaped as bytes are copied: `"` and `\` get a backslash, control bytes become `\u00XX`, valid UTF-8 passes through,
//! and each invalid run becomes U+FFFD. No record allocates.

use super::agent::{CARRIAGE_RETURN, ESCAPE, TRUNCATION_MARK, after_sequence, count_escapes, exit_word, libtest_body};
use super::{Event, ExcerptRows, Identity, Outcome, State, Stream, located};
use crate::failure_class::{Absent, Class, Enforcer, Exit, Location, Record};
use crate::line::Line;
use crate::view_defaults::{OUTPUT_BYTES, OUTPUT_HEAD_BYTES};

const HEX: &[u8; 16] = b"0123456789abcdef";
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();
const RECOVER: &[u8] = b"cargo test -- --exact --nocapture";

impl<const RERUN: usize> State<'_, RERUN> {
    pub(super) fn project_json<const FRAME: usize>(&self, event: &Event<'_>, frame: &mut Line<FRAME>) {
        match *event {
            Event::Begin { version, shard, jobs, why: Some(why), .. } => run(frame, version, shard, jobs, why),
            Event::Listed { binaries, tests, .. } => listed(frame, binaries, tests),
            Event::Started { id, at_ms, limit_ms, .. } => started(frame, id, at_ms, limit_ms),
            Event::Settled { id, outcome, solo, .. } => settled(frame, id, outcome, self.settled_ms, solo),
            Event::Captured { id, stdout, stderr, exit, excerpt } => failure(frame, id, [stdout, stderr], exit, excerpt),
            Event::BuildFailed { diagnostic, exit } => build_failed(frame, diagnostic, exit),
            Event::Interrupted { signal, groups } => interrupted(frame, signal, groups),
            Event::Running { id, .. } => running(frame, id),
            Event::Exited { code } => exit(frame, code),
            Event::Finished { passed, failed, ignored, not_run, wall_ms, manifest, discovered, filtered, exit: code } => {
                summary(frame, [discovered, passed, failed, ignored, filtered, not_run], wall_ms);
                self.rerun_record(frame, manifest);
                exit(frame, code);
            }
            Event::Begin { .. } | Event::Waiting { .. } | Event::Previous { .. } | Event::Built { .. } | Event::Resize { .. } | Event::Tick { .. } | Event::Warned { .. } | Event::BlockOpened { .. } | Event::BlockClosed | Event::Key(_) | Event::Notice { .. } | Event::Retried { .. } | Event::Reran { .. } => {}
        }
    }

    fn rerun_record<const FRAME: usize>(&self, frame: &mut Line<FRAME>, manifest: Option<&[u8]>) {
        let included = self.clauses_within(RERUN);
        if included == 0 {
            return;
        }
        open(frame, b"rerun");
        frame.text(b",\"command\":\"cargo val");
        if let Some(path) = manifest {
            frame.text(b" --manifest-path '");
            escaped(frame, path);
            frame.text(b"'");
        }
        frame.text(b" --no-fail-fast -E '");
        for (position, (binary, name)) in self.failures().take(included).enumerate() {
            if position != 0 {
                frame.text(b" | ");
            }
            filter_clause(frame, (binary, name));
        }
        frame.text(b"'\"");
        number(frame, b"covers", included);
        number(frame, b"unlisted", self.stored - included + self.unstored);
        close(frame);
    }
}

fn open<const FRAME: usize>(frame: &mut Line<FRAME>, tag: &[u8]) {
    frame.text(b"{\"v\":1,\"t\":\"");
    frame.text(tag);
    frame.text(b"\"");
}

fn close<const FRAME: usize>(frame: &mut Line<FRAME>) {
    frame.text(b"}\n");
}

fn key<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8]) {
    frame.text(b",\"");
    frame.text(name);
    frame.text(b"\":");
}

fn number<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], value: usize) {
    key(frame, name);
    frame.number(value);
}

fn flag<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], value: bool) {
    key(frame, name);
    frame.text(if value { b"true" } else { b"false" });
}

fn null<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8]) {
    key(frame, name);
    frame.text(b"null");
}

fn text<const FRAME: usize>(frame: &mut Line<FRAME>, name: &[u8], value: &[u8]) {
    key(frame, name);
    frame.text(b"\"");
    escaped(frame, value);
    frame.text(b"\"");
}

fn id_field<const FRAME: usize>(frame: &mut Line<FRAME>, (binary, name): Identity<'_>) {
    key(frame, b"id");
    frame.text(b"\"");
    escaped(frame, binary);
    frame.text(b"::");
    escaped(frame, name);
    frame.text(b"\"");
    text(frame, b"binary", binary);
    text(frame, b"test", name);
}

fn filter_clause<const FRAME: usize>(frame: &mut Line<FRAME>, (binary, name): Identity<'_>) {
    frame.text(b"(binary(=");
    escaped(frame, binary);
    frame.text(b") & test(=");
    escaped(frame, name);
    frame.text(b"))");
}

fn escaped<const FRAME: usize>(frame: &mut Line<FRAME>, bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        valid(frame, chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            frame.text(REPLACEMENT);
        }
    }
}

fn valid<const FRAME: usize>(frame: &mut Line<FRAME>, bytes: &[u8]) {
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(*byte, b'"' | b'\\' | 0..=0x1f) {
            continue;
        }
        frame.text(&bytes[start..index]);
        start = index + 1;
        match *byte {
            b'"' => frame.text(b"\\\""),
            b'\\' => frame.text(b"\\\\"),
            control => frame.text(&[b'\\', b'u', b'0', b'0', HEX[usize::from(control >> 4)], HEX[usize::from(control & 0xf)]]),
        }
    }
    frame.text(&bytes[start..]);
}

fn clean<const FRAME: usize>(frame: &mut Line<FRAME>, bytes: &[u8]) {
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            ESCAPE => position = after_sequence(bytes, position),
            CARRIAGE_RETURN => position += 1,
            _ => {
                let run = bytes[position..].iter().take_while(|byte| !matches!(**byte, ESCAPE | CARRIAGE_RETURN)).count();
                escaped(frame, &bytes[position..position + run]);
                position += run;
            }
        }
    }
}

fn run<const FRAME: usize>(frame: &mut Line<FRAME>, version: &[u8], shard: bool, jobs: usize, why: &[u8]) {
    open(frame, b"run");
    text(frame, b"tool", b"cargo-val");
    text(frame, b"version", version);
    null(frame, b"run_id");
    text(frame, b"mode", b"json");
    text(frame, b"mode_why", why);
    text(frame, b"process", if shard { b"shard" } else { b"test" });
    number(frame, b"jobs", jobs);
    null(frame, b"test_timeout_ms");
    null(frame, b"records");
    number(frame, b"schema", 1);
    close(frame);
}

fn listed<const FRAME: usize>(frame: &mut Line<FRAME>, binaries: usize, tests: usize) {
    open(frame, b"listed");
    number(frame, b"binaries", binaries);
    number(frame, b"discovered", tests);
    null(frame, b"filtered");
    null(frame, b"ignored");
    close(frame);
}

fn started<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, at_ms: u64, limit_ms: Option<u32>) {
    open(frame, b"start");
    id_field(frame, id);
    null(frame, b"slot");
    number(frame, b"at_ms", usize::try_from(at_ms).unwrap_or(usize::MAX));
    match limit_ms {
        Some(limit) => number(frame, b"limit_ms", usize::try_from(limit).unwrap_or(usize::MAX)),
        None => null(frame, b"limit_ms"),
    }
    close(frame);
}

fn settled<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, outcome: Outcome, millis: Option<u64>, solo: bool) {
    open(frame, b"end");
    id_field(frame, id);
    text(frame, b"outcome", match outcome {
        Outcome::Passed => b"passed",
        Outcome::Failed => b"failed",
        Outcome::Ignored => b"ignored",
    });
    match millis {
        Some(value) => number(frame, b"ms", usize::try_from(value).unwrap_or(usize::MAX)),
        None => null(frame, b"ms"),
    }
    flag(frame, b"solo_rerun", solo);
    close(frame);
}

fn failure<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, [stdout, stderr]: [Stream<'_>; 2], exit: Exit, excerpt: ExcerptRows<'_>) {
    let (capture, dropped) = if stdout.0.is_empty() { stderr } else { stdout };
    let record = located(stdout, stderr, exit);
    open(frame, b"fail");
    id_field(frame, id);
    match &record {
        Ok(found) => classified(frame, found),
        Err(_) => unclassified(frame),
    }
    excerpt_fields(frame, excerpt);
    key(frame, b"rerun");
    frame.text(b"\"cargo val --no-fail-fast -E '");
    filter_clause(frame, id);
    frame.text(b"'\"");
    close(frame);
    output(frame, id, (capture, dropped), if stdout.0.is_empty() { b"stderr" } else { b"stdout" });
}

fn excerpt_fields<const FRAME: usize>(frame: &mut Line<FRAME>, excerpt: ExcerptRows<'_>) {
    match excerpt {
        Ok(rows) => {
            text(frame, b"excerpt", rows);
            null(frame, b"excerpt_absent");
        }
        Err(reason) => {
            null(frame, b"excerpt");
            text(frame, b"excerpt_absent", reason);
        }
    }
}

fn classified<const FRAME: usize>(frame: &mut Line<FRAME>, record: &Record<'_>) {
    let (word, enforcer, signal): (&[u8], Option<Enforcer>, Option<i32>) = match record.class {
        Class::Assert => (b"ASSERT", None, None),
        Class::Panic => (b"PANIC", None, None),
        Class::Hang => (b"HANG", None, None),
        Class::Timeout { enforced_by } => (b"TIMEOUT", Some(enforced_by), None),
        Class::Crash { signal } => (b"CRASH", None, Some(signal)),
    };
    text(frame, b"class", word);
    match enforcer {
        Some(Enforcer::Val) => text(frame, b"enforced_by", b"val"),
        Some(Enforcer::Harness) => text(frame, b"enforced_by", b"harness"),
        None => null(frame, b"enforced_by"),
    }
    match signal {
        Some(value) => number(frame, b"signal", usize::try_from(value).unwrap_or(0)),
        None => null(frame, b"signal"),
    }
    match record.location {
        Ok(site) => location(frame, site, false),
        Err(Absent::External(site)) => location(frame, site, true),
        Err(Absent::NoPanicSite) => absent_location(frame, b"no_panic_site"),
    }
    text(frame, b"message", record.message);
}

fn unclassified<const FRAME: usize>(frame: &mut Line<FRAME>) {
    text(frame, b"class", b"UNCLASSIFIED");
    null(frame, b"enforced_by");
    null(frame, b"signal");
    absent_location(frame, b"no_panic_in_capture");
    null(frame, b"message");
}

fn location<const FRAME: usize>(frame: &mut Line<FRAME>, site: Location<'_>, external: bool) {
    text(frame, b"file", site.file);
    number(frame, b"line", site.line as usize);
    number(frame, b"col", site.col as usize);
    flag(frame, b"external", external);
    null(frame, b"absent");
}

fn absent_location<const FRAME: usize>(frame: &mut Line<FRAME>, reason: &[u8]) {
    null(frame, b"file");
    null(frame, b"line");
    null(frame, b"col");
    flag(frame, b"external", false);
    text(frame, b"absent", reason);
}

fn output<const FRAME: usize>(frame: &mut Line<FRAME>, id: Identity<'_>, (capture, dropped): Stream<'_>, stream: &[u8]) {
    let body = libtest_body(capture);
    let cut = body.len() > OUTPUT_BYTES;
    let (head, tail) = if cut {
        (&body[..OUTPUT_HEAD_BYTES], &body[body.len() - (OUTPUT_BYTES - OUTPUT_HEAD_BYTES)..])
    } else {
        (body, &body[body.len()..])
    };
    let shown = if cut { OUTPUT_BYTES } else { body.len() };
    let total = capture.len().saturating_add(usize::try_from(dropped).unwrap_or(usize::MAX));
    open(frame, b"output");
    id_field(frame, id);
    text(frame, b"stream", stream);
    number(frame, b"bytes", total);
    number(frame, b"shown", shown);
    number(frame, b"dropped", total - shown);
    number(frame, b"esc_stripped", count_escapes(head) + count_escapes(tail));
    key(frame, b"text");
    frame.text(b"\"");
    clean(frame, head);
    if cut {
        escaped(frame, b"\n");
        escaped(frame, TRUNCATION_MARK);
    }
    clean(frame, tail);
    frame.text(b"\"");
    close(frame);
    if cut || dropped != 0 {
        truncation(frame, total, total - shown);
    }
}

fn truncation<const FRAME: usize>(frame: &mut Line<FRAME>, total: usize, dropped: usize) {
    open(frame, b"trunc");
    text(frame, b"what", b"output");
    number(frame, b"cap", OUTPUT_BYTES);
    number(frame, b"total", total);
    number(frame, b"dropped", dropped);
    text(frame, b"recover", RECOVER);
    close(frame);
}

fn build_failed<const FRAME: usize>(frame: &mut Line<FRAME>, diagnostic: &[u8], exit: u8) {
    open(frame, b"build");
    flag(frame, b"ok", false);
    null(frame, b"errors");
    null(frame, b"warnings");
    null(frame, b"binaries_built");
    null(frame, b"binaries_total");
    number(frame, b"exit", usize::from(exit));
    text(frame, b"message", diagnostic);
    close(frame);
}

fn interrupted<const FRAME: usize>(frame: &mut Line<FRAME>, signal: u8, groups: usize) {
    open(frame, b"interrupted");
    number(frame, b"signal", usize::from(signal));
    number(frame, b"forwarded_groups", groups);
    key(frame, b"in_flight");
    frame.text(b"[]");
    close(frame);
}

fn running<const FRAME: usize>(frame: &mut Line<FRAME>, (binary, name): Identity<'_>) {
    open(frame, b"running");
    key(frame, b"in_flight");
    frame.text(b"[{\"id\":\"");
    escaped(frame, binary);
    frame.text(b"::");
    escaped(frame, name);
    frame.text(b"\",\"age_ms\":null,\"limit_ms\":null}]");
    close(frame);
}

fn summary<const FRAME: usize>(frame: &mut Line<FRAME>, [discovered, passed, failed, ignored, filtered, not_run]: [usize; 6], wall_ms: u64) {
    open(frame, b"summary");
    number(frame, b"discovered", discovered);
    number(frame, b"run", passed + failed);
    number(frame, b"passed", passed);
    number(frame, b"failed", failed);
    number(frame, b"ignored", ignored);
    number(frame, b"filtered", filtered);
    number(frame, b"not_run", not_run);
    number(frame, b"wall_ms", usize::try_from(wall_ms).unwrap_or(usize::MAX));
    close(frame);
}

fn exit<const FRAME: usize>(frame: &mut Line<FRAME>, code: u8) {
    open(frame, b"exit");
    number(frame, b"code", usize::from(code));
    text(frame, b"outcome", exit_word(code));
    close(frame);
}

