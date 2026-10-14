use core::str::{self, Lines, Utf8Error};

/// Which text a stack capture holds: macOS `sample` call-graph output, or Linux `/proc/<pid>/task/<tid>/stack` blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `sample` output: a `Call graph:` section of indented `count symbol (in image) + offset [address] file:line` lines.
    Sample,
    /// `Thread_<tid>` header lines each followed by `[<0>] symbol+0xoffset/0xsize` kernel frames.
    KernelStack,
}

/// Where a frame lives, from the reader's point of view.
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

/// One output row of [`Rows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    /// A thread header; the frames after it belong to it.
    Thread(&'a str),
    Frame(Frame<'a>),
    /// This many consecutive runtime frames were folded away.
    Folded(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Preamble,
    Graph,
    Done,
}

const RUNTIME_CRATES: [&str; 8] = ["std", "core", "alloc", "test", "panic_unwind", "panic_abort", "proc_macro", "backtrace"];

/// Sans-IO iterator over a captured stack: thread headers, classified frames, and optionally runs of runtime frames folded.
///
/// Zero allocation: every row borrows from the input, and the iterator holds only a cursor.
/// Frame classes come from the crate named in the symbol (`workspace` lists the user crates, in
/// underscore form), falling back to the image for symbols that name no Rust crate.
#[derive(Debug, Clone)]
pub struct Rows<'a> {
    lines: Lines<'a>,
    format: Format,
    workspace: &'a [&'a str],
    fold: bool,
    section: Section,
    thread: &'a str,
    folded: usize,
    pending: Option<Row<'a>>,
}

impl<'a> Rows<'a> {
    /// Every frame, none folded.
    ///
    /// # Errors
    ///
    /// Returns the UTF-8 error when the captured bytes are not text.
    pub fn raw(bytes: &'a [u8], format: Format, workspace: &'a [&'a str]) -> Result<Self, Utf8Error> {
        Self::new(bytes, format, workspace, false)
    }

    /// Runs of runtime frames collapse into one [`Row::Folded`]; user and other frames stay.
    ///
    /// # Errors
    ///
    /// Returns the UTF-8 error when the captured bytes are not text.
    pub fn folded(bytes: &'a [u8], format: Format, workspace: &'a [&'a str]) -> Result<Self, Utf8Error> {
        Self::new(bytes, format, workspace, true)
    }

    fn new(bytes: &'a [u8], format: Format, workspace: &'a [&'a str], fold: bool) -> Result<Self, Utf8Error> {
        let section = match format {
            Format::Sample => Section::Preamble,
            Format::KernelStack => Section::Graph,
        };
        Ok(Self {
            lines: str::from_utf8(bytes)?.lines(),
            format,
            workspace,
            fold,
            section,
            thread: "",
            folded: 0,
            pending: None,
        })
    }

    fn read_line(&mut self, line: &'a str) -> Option<Row<'a>> {
        match (self.format, self.section) {
            (_, Section::Done) => None,
            (Format::Sample, Section::Preamble) => {
                if line.trim() == "Call graph:" {
                    self.section = Section::Graph;
                }
                None
            }
            (Format::Sample, Section::Graph) => self.sample_line(line),
            (Format::KernelStack, _) => self.kernel_line(line),
        }
    }

    fn sample_line(&mut self, line: &'a str) -> Option<Row<'a>> {
        if line.trim().is_empty() {
            self.section = Section::Done;
            return None;
        }
        let trimmed = line.trim_start_matches([' ', '+', '!', ':', '|']);
        let depth = line.len() - trimmed.len();
        let (count, body) = trimmed.split_once(' ')?;
        let samples = count.parse::<u32>().ok()?;
        let body = body.trim_start();
        if body.starts_with("Thread_") {
            self.thread = body.trim_end();
            return Some(Row::Thread(self.thread));
        }
        let (symbol, rest) = body.split_once("  (in ")?;
        let (image, tail) = rest.split_once(')')?;
        let (file, number) = source_location(tail);
        Some(Row::Frame(self.frame(depth, samples, symbol, image, file, number)))
    }

    fn kernel_line(&mut self, line: &'a str) -> Option<Row<'a>> {
        let trimmed = line.trim();
        if trimmed.starts_with("Thread_") {
            self.thread = trimmed;
            return Some(Row::Thread(trimmed));
        }
        let (_, after) = trimmed.split_once("] ")?;
        let symbol = after.split_once('+').map_or(after, |(name, _)| name);
        Some(Row::Frame(self.frame(0, 1, symbol, "kernel", None, None)))
    }

    fn frame(&self, depth: usize, samples: u32, symbol: &'a str, image: &'a str, file: Option<&'a str>, line: Option<u32>) -> Frame<'a> {
        Frame {
            thread: self.thread,
            depth,
            samples,
            symbol,
            image,
            file,
            line,
            class: classify(symbol, image, self.workspace),
        }
    }
}

impl<'a> Iterator for Rows<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Row<'a>> {
        if let Some(row) = self.pending.take() {
            return Some(row);
        }
        loop {
            let Some(line) = self.lines.next().filter(|_| self.section != Section::Done) else {
                return self.flush_folded();
            };
            match self.read_line(line) {
                Some(Row::Frame(frame)) if self.fold && frame.class == Class::Runtime => self.folded += 1,
                Some(row) if self.folded == 0 => return Some(row),
                Some(row) => {
                    self.pending = Some(row);
                    return self.flush_folded();
                }
                None => {}
            }
        }
    }
}

impl Rows<'_> {
    fn flush_folded(&mut self) -> Option<Row<'static>> {
        let count = core::mem::take(&mut self.folded);
        (count > 0).then_some(Row::Folded(count))
    }
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

    use super::{Class, Format, Frame, Row, Rows};
    use crate::test_alloc::measure_allocations;

    /// Real `sample` output: `/usr/bin/sample <pid> 1 -mayDie -file <path>` against the `tests/fixtures/blocked_test`
    /// lib test binary while its `unit_blocks_when_requested` test was blocked in `File::open` on a fifo with no
    /// writer. macOS 15.8 (24H20), ARM64, 2026-10-13. Committed unedited.
    const SAMPLE: &[u8] = include_bytes!("../tests/fixtures/stack/sample_blocked_fixture.txt");
    const WORKSPACE: [&str; 1] = ["cargo_val_blocked_fixture"];

    fn frames(rows: Rows<'_>) -> Vec<Frame<'_>> {
        rows.filter_map(|row| if let Row::Frame(frame) = row { Some(frame) } else { None }).collect()
    }

    #[test]
    fn real_sample_names_the_user_frame_where_the_test_blocks() {
        let rows = Rows::raw(SAMPLE, Format::Sample, &WORKSPACE).expect("sample output is text");

        let frames = frames(rows);

        let blocked = frames
            .iter()
            .rfind(|frame| frame.class == Class::User && frame.thread.contains("unit_blocks_when_requested"))
            .expect("the blocked test thread has a user frame");
        assert!(blocked.symbol.contains("unit_blocks_when_requested"), "{}", blocked.symbol);
        assert_eq!((blocked.file, blocked.line), (Some("lib.rs"), Some(26)));
        assert_eq!(frames.len(), 32, "every frame line of the call graph");
    }

    #[test]
    fn real_sample_folds_runtime_frames_into_counted_rows() {
        let rows = Rows::folded(SAMPLE, Format::Sample, &WORKSPACE).expect("sample output is text");

        let all: Vec<Row<'_>> = rows.collect();

        let folded: usize = all.iter().filter_map(|row| if let Row::Folded(count) = row { Some(*count) } else { None }).sum();
        let kept = all.iter().filter(|row| matches!(row, Row::Frame(_))).count();
        assert_eq!(kept + folded, 32, "folding loses no frame");
        assert!(folded > 0 && kept < 32, "folded={folded} kept={kept}");
        assert_eq!(all.iter().filter(|row| matches!(row, Row::Thread(_))).count(), 2);
        assert!(all.iter().filter_map(|row| if let Row::Frame(frame) = row { Some(frame) } else { None }).all(|frame| frame.class != Class::Runtime));
    }

    #[test]
    fn parsing_the_real_sample_100k_times_allocates_nothing() {
        let (parsed, allocations) = measure_allocations(|| {
            let mut total = 0_usize;
            for _ in 0..100_000 {
                total += Rows::folded(SAMPLE, Format::Sample, &WORKSPACE).expect("sample output is text").count();
            }
            total
        });

        assert!(parsed > 100_000, "parsed rows: {parsed}");
        assert_eq!(allocations, 0);
    }

    #[test]
    fn kernel_stack_blocks_parse_as_runtime_frames_per_thread() {
        // layout documented in proc(5) for /proc/<pid>/task/<tid>/stack; no Linux host was available to capture one
        let text = b"Thread_4242\n[<0>] futex_wait_queue+0x4d/0x90\n[<0>] __x64_sys_futex+0x14e/0x200\n[<0>] do_syscall_64+0x5b/0x1a0\nThread_4243\n[<0>] do_epoll_wait+0x5ae/0x7c0\n";

        let rows: Vec<Row<'_>> = Rows::raw(text, Format::KernelStack, &WORKSPACE).expect("text").collect();

        assert_eq!(rows.len(), 6);
        let Row::Frame(first) = rows[1] else { panic!("second row is a frame: {:?}", rows[1]) };
        assert_eq!((first.symbol, first.thread, first.class), ("futex_wait_queue", "Thread_4242", Class::Runtime));
    }

    #[test]
    fn rows_are_empty_for_text_without_a_call_graph() {
        let rows = Rows::raw(b"sample: process 99999 does not exist\n", Format::Sample, &WORKSPACE).expect("text");

        assert_eq!(rows.count(), 0);
    }

    #[test]
    fn rows_refuse_bytes_that_are_not_text() {
        assert!(Rows::raw(&[0xff, 0xfe, b'\n'], Format::Sample, &WORKSPACE).is_err());
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
