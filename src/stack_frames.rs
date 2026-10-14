use core::str::{self, Utf8Error};

/// Which text a stack capture holds: macOS `sample` call-graph output, or Linux `/proc/<pid>/task/<tid>/stack` blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `sample` output: a `Call graph:` section of indented `count symbol (in image) + offset [address] file:line` lines.
    Sample,
    /// `Thread_<tid>` header lines each followed by `[<0>] symbol+0xoffset/0xsize` kernel frames; the offset is absent for a `wchan` frame.
    KernelStack,
}

/// Where a frame lives, from the reader's point of view.
///
/// This is frame ownership, not a failure kind: `failure_class::Class` names why a test failed, and no variant of it
/// describes a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A crate the caller named as the workspace.
    User,
    /// std, core, alloc, test, proxima, prime, or a system library or kernel frame.
    Runtime,
    /// Anything else: a dependency, or a frame whose owner the symbol does not reveal.
    Other,
}

/// One stack frame as borrowed views into the captured bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The thread header text this frame belongs to, e.g. `Thread_160249294: tests::unit_blocks_when_requested`.
    pub thread: &'a str,
    /// Indent of the line in the call graph; deeper is closer to the blocked leaf.
    pub depth: usize,
    /// How many samples saw this frame (1 for kernel stacks).
    pub samples: u32,
    /// The symbol exactly as the tool printed it; Rust symbols stay mangled.
    pub symbol: &'a str,
    /// The image the symbol lives in, e.g. `libsystem_kernel.dylib`; `kernel` for kernel stacks.
    pub image: &'a str,
    /// Source file name when the tool resolved one, without directory (`lib.rs`).
    pub file: Option<&'a str>,
    /// Source line when the tool resolved one.
    pub line: Option<u32>,
    pub class: Class,
}

const RUNTIME_CRATES: [&str; 8] = ["std", "core", "alloc", "test", "panic_unwind", "panic_abort", "proc_macro", "backtrace"];

/// Sans-IO parse of a captured stack into classified frames, in input order.
///
/// A source pipe over `str::lines`: `skip_while`/`take_while` cut the `Call graph:` section out of `sample` output, and
/// `scan` carries the current thread header, so the iterator holds only a cursor and allocates nothing. Every frame
/// borrows from `bytes`. Frame classes come from the crate named in the symbol (`workspace` lists the user crates, in
/// underscore form), falling back to the image for symbols that name no Rust crate. To drop runtime frames, filter on
/// [`Class::Runtime`]; the count dropped is the total minus the count kept.
///
/// # Errors
///
/// Returns the UTF-8 error when the captured bytes are not text.
pub fn frames<'a>(bytes: &'a [u8], format: Format, workspace: &'a [&'a str]) -> Result<impl Iterator<Item = Frame<'a>>, Utf8Error> {
    let graph = format == Format::Sample;
    Ok(str::from_utf8(bytes)?
        .lines()
        .skip_while(move |line| graph && line.trim() != "Call graph:")
        .skip(usize::from(graph))
        .take_while(move |line| !graph || !line.trim().is_empty())
        .scan("", move |thread, line| Some(parse_line(format, line, thread, workspace)))
        .flatten())
}

fn parse_line<'a>(format: Format, line: &'a str, thread: &mut &'a str, workspace: &[&str]) -> Option<Frame<'a>> {
    match format {
        Format::Sample => sample_line(line, thread, workspace),
        Format::KernelStack => kernel_line(line, thread, workspace),
    }
}

fn sample_line<'a>(line: &'a str, thread: &mut &'a str, workspace: &[&str]) -> Option<Frame<'a>> {
    let trimmed = line.trim_start_matches([' ', '+', '!', ':', '|']);
    let depth = line.len() - trimmed.len();
    let (count, body) = trimmed.split_once(' ')?;
    let samples = count.parse::<u32>().ok()?;
    let body = body.trim_start();
    if body.starts_with("Thread_") {
        *thread = body.trim_end();
        return None;
    }
    let (symbol, rest) = body.split_once("  (in ")?;
    let (image, tail) = rest.split_once(')')?;
    let (file, number) = source_location(tail);
    Some(Frame { thread, depth, samples, symbol, image, file, line: number, class: classify(symbol, image, workspace) })
}

fn kernel_line<'a>(line: &'a str, thread: &mut &'a str, workspace: &[&str]) -> Option<Frame<'a>> {
    let trimmed = line.trim();
    if trimmed.starts_with("Thread_") {
        *thread = trimmed;
        return None;
    }
    let (_, after) = trimmed.split_once("] ")?;
    let symbol = after.split_once('+').map_or(after, |(name, _)| name);
    Some(Frame { thread, depth: 0, samples: 1, symbol, image: "kernel", file: None, line: None, class: classify(symbol, "kernel", workspace) })
}

fn source_location(tail: &str) -> (Option<&str>, Option<u32>) {
    let Some((_, location)) = tail.rsplit_once(']') else {
        return (None, None);
    };
    match location.trim().rsplit_once(':') {
        Some((file, number)) => number.parse::<u32>().map_or((None, None), |line| (Some(file), Some(line))),
        None => (None, None),
    }
}

fn classify(symbol: &str, image: &str, workspace: &[&str]) -> Class {
    match rust_crate(symbol) {
        Some(name) if workspace.contains(&name) => Class::User,
        Some(name) if is_runtime_crate(name) => Class::Runtime,
        None if is_system_image(image) => Class::Runtime,
        Some(_) | None => Class::Other,
    }
}

fn is_runtime_crate(name: &str) -> bool {
    RUNTIME_CRATES.contains(&name) || name == "prime" || name.starts_with("prime_") || name.starts_with("proxima")
}

fn is_system_image(image: &str) -> bool {
    image == "dyld" || image == "kernel" || image.starts_with("lib")
}

/// The crate a Rust symbol belongs to: the first crate root in a v0 symbol, or the first path segment of a legacy one.
fn rust_crate(symbol: &str) -> Option<&str> {
    if let Some(v0) = symbol.strip_prefix("_R") {
        return v0_crate(v0);
    }
    let legacy = symbol.strip_prefix("_ZN")?;
    length_prefixed(legacy, 0)
}

fn v0_crate(path: &str) -> Option<&str> {
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'N' => index += 2,
            b's' | b'B' => index = skip_base62(bytes, index + 1),
            b'C' => return crate_root(path, index + 1),
            _ => index += 1,
        }
    }
    None
}

fn skip_base62(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < bytes.len() && bytes[index] != b'_' {
        index += 1;
    }
    index + 1
}

fn crate_root(path: &str, start: usize) -> Option<&str> {
    let bytes = path.as_bytes();
    let from = if bytes.get(start) == Some(&b's') { skip_base62(bytes, start + 1) } else { start };
    length_prefixed(path, from)
}

fn length_prefixed(text: &str, from: usize) -> Option<&str> {
    let tail = text.get(from..)?;
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    let length = tail.get(..digits)?.parse::<usize>().ok()?;
    let start = digits + usize::from(tail.as_bytes().get(digits) == Some(&b'_'));
    tail.get(start..start.checked_add(length)?)
}


#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::{Class, Format, Frame, frames};
    use crate::test_alloc::measure_allocations;

    /// Real `sample` output: `/usr/bin/sample <pid> 1 -mayDie -file <path>` against the `tests/fixtures/blocked_test`
    /// lib test binary while its `unit_blocks_when_requested` test was blocked in `block.read` on a read-write fifo
    /// no one writes. macOS 15.8 (24H20), ARM64, 2026-10-14. Committed unedited.
    const SAMPLE: &[u8] = include_bytes!("../tests/fixtures/stack/sample_blocked_fixture.txt");
    /// Real kernel stacks of the same fixture blocked the same way on versailles (Linux 7.2.9-zen1-1-zen `x86_64`,
    /// 2026-10-14): `sudo cat /proc/<pid>/task/<tid>/stack` for each of the two threads of
    /// `cargo_val_blocked_fixture-2e42461185e9115d --exact tests::unit_blocks_when_requested`, each file prefixed
    /// with the `Thread_<tid>` header line that `edge_stack_linux::capture` writes. Frame lines unedited.
    const KERNEL_STACK: &[u8] = include_bytes!("../tests/fixtures/stack/kernel_stack_blocked_fixture.txt");
    /// Real `/proc/<pid>/task/<tid>/wchan` of the same two threads, read unprivileged, with the same headers. The
    /// kernel prints the symbol alone: no offset, no size, no callers.
    const KERNEL_WCHAN: &[u8] = include_bytes!("../tests/fixtures/stack/kernel_wchan_blocked_fixture.txt");
    const WORKSPACE: [&str; 1] = ["cargo_val_blocked_fixture"];

    fn parse(bytes: &'static [u8], format: Format) -> Vec<Frame<'static>> {
        frames(bytes, format, &WORKSPACE).expect("captured stacks are text").collect()
    }

    #[test]
    fn real_sample_names_the_user_frame_where_the_test_blocks() {
        let all = parse(SAMPLE, Format::Sample);

        let blocked = all
            .iter()
            .rfind(|frame| frame.class == Class::User && frame.thread.contains("unit_blocks_when_requested"))
            .expect("the blocked test thread has a user frame");
        assert!(blocked.symbol.contains("unit_blocks_when_requested"), "{}", blocked.symbol);
        assert_eq!((blocked.file, blocked.line), (Some("lib.rs"), Some(32)));
        assert_eq!(all.len(), 28, "every frame line of the call graph");
    }

    #[test]
    fn real_sample_keeps_user_frames_when_runtime_frames_are_filtered_out() {
        let all = parse(SAMPLE, Format::Sample);

        let kept: Vec<&Frame<'_>> = all.iter().filter(|frame| frame.class != Class::Runtime).collect();

        assert!(!kept.is_empty() && kept.len() < all.len(), "kept={} of {}", kept.len(), all.len());
        assert!(kept.iter().any(|frame| frame.class == Class::User));
        let mut threads: Vec<&str> = all.iter().map(|frame| frame.thread).collect();
        threads.dedup();
        assert_eq!(threads.len(), 2, "{threads:?}");
    }

    #[test]
    fn parsing_the_real_sample_100k_times_allocates_nothing() {
        let (parsed, allocations) = measure_allocations(|| {
            let mut total = 0_usize;
            for _ in 0..100_000 {
                total += frames(SAMPLE, Format::Sample, &WORKSPACE).expect("sample output is text").count();
            }
            total
        });

        assert!(parsed > 100_000, "parsed frames: {parsed}");
        assert_eq!(allocations, 0);
    }

    #[test]
    fn real_kernel_stack_of_a_fifo_read_names_anon_pipe_read_under_its_thread() {
        let all = parse(KERNEL_STACK, Format::KernelStack);

        assert_eq!(all.len(), 11);
        let leaf = all.iter().find(|frame| frame.symbol == "anon_pipe_read").expect("the fifo read wait is a frame");
        assert_eq!((leaf.thread, leaf.class, leaf.image), ("Thread_3043700", Class::Runtime, "kernel"));
        assert_eq!(all[0].symbol, "__futex_wait");
        assert_eq!(all[0].thread, "Thread_3043699");
        assert_eq!(all[10].symbol, "entry_SYSCALL_64_after_hwframe");
    }

    #[test]
    fn real_wchan_frames_parse_without_an_offset() {
        let all = parse(KERNEL_WCHAN, Format::KernelStack);

        let symbols: Vec<(&str, &str)> = all.iter().map(|frame| (frame.thread, frame.symbol)).collect();
        assert_eq!(symbols, [("Thread_3043699", "__futex_wait"), ("Thread_3043700", "anon_pipe_read")]);
    }

    #[test]
    fn frames_are_empty_for_text_without_a_call_graph() {
        let all = frames(b"sample: process 99999 does not exist\n", Format::Sample, &WORKSPACE).expect("text");

        assert_eq!(all.count(), 0);
    }

    #[test]
    fn frames_refuse_bytes_that_are_not_text() {
        assert!(frames(&[0xff, 0xfe, b'\n'], Format::Sample, &WORKSPACE).is_err());
    }

    #[test]
    fn crate_names_come_from_v0_and_legacy_symbols() {
        let cases = [
            ("_RNvCs5cjG6yoUoUc_25cargo_val_blocked_fixture4main", Some("cargo_val_blocked_fixture")),
            ("_RNvNtCs1SSuMAdGM58_3std2rt19lang_start_internal", Some("std")),
            ("_RNCNvNtCs5cjG6yoUoUc_25cargo_val_blocked_fixture5tests26unit_blocks_when_requested0B5_", Some("cargo_val_blocked_fixture")),
            ("_RNvYNCNvNtCs5cjG6yoUoUc_25cargo_val_blocked_fixture5tests26unit_blocks_when_requested0INtNtNtCsjIpoRzUjyHQ_4core3ops8function6FnOnceuE9call_onceB8_", Some("cargo_val_blocked_fixture")),
            ("_ZN3std2rt10lang_start17h0123456789abcdefE", Some("std")),
            ("__open", None),
        ];

        for (symbol, expected) in cases {
            assert_eq!(super::rust_crate(symbol), expected, "{symbol}");
        }
    }
}
