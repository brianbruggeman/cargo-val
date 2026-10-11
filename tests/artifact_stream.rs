use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cargo_val::{ArtifactRecord, ExecutablePath, ParseError, parse_artifact};

struct CountAllocations;

static COUNT_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountAllocations {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNT_ALLOCATIONS.load(Ordering::Relaxed) {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountAllocations = CountAllocations;

fn counted_parse<'input, 'scratch>(
    line: &'input [u8],
    scratch: &'scratch mut [u8],
    max_record_bytes: usize,
    max_json_depth: usize,
) -> (Result<ArtifactRecord<'input, 'scratch>, ParseError>, usize) {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    COUNT_ALLOCATIONS.store(true, Ordering::Relaxed);
    let result = parse_artifact(line, scratch, max_record_bytes, max_json_depth);
    COUNT_ALLOCATIONS.store(false, Ordering::Relaxed);
    (result, ALLOCATION_COUNT.load(Ordering::Relaxed))
}

fn assert_escaped_and_bounded_parses(scratch: &mut [u8]) {
    let escaped = include_bytes!("../specs/cargo-val/oracles/c2-windows-escaped.jsonl");
    let escaped = &escaped[..escaped.len() - 1];
    let expected_escaped =
        include_str!("../specs/cargo-val/oracles/c2-windows-escaped.expected.jsonl");
    let expected_escaped: serde_json::Value = serde_json::from_str(expected_escaped)
        .expect("the escaped-path expected record must parse");
    let decoded_path = expected_escaped["decoded_executable"]
        .as_str()
        .expect("the expected executable must be text");
    let scratch_start = scratch.as_ptr();
    let scratch_end = scratch_start as usize + scratch.len();
    let (result, allocations) = counted_parse(escaped, scratch, 629, 4);
    assert_eq!(allocations, 0);
    match result.expect("escaped Windows Cargo path must parse") {
        ArtifactRecord::TestExecutable(ExecutablePath::Scratch(executable)) => {
            assert_eq!(executable, decoded_path);
            assert!((scratch_start as usize) <= executable.as_ptr() as usize);
            assert!((executable.as_ptr() as usize) < scratch_end);
            assert!(
                !escaped
                    .windows(executable.len())
                    .any(|window| { window.as_ptr() == executable.as_bytes().as_ptr() })
            );
        }
        _ => panic!("escaped Cargo path must use caller scratch"),
    }

    let (result, allocations) = counted_parse(escaped, &mut scratch[..48], 629, 4);
    assert_eq!(allocations, 0);
    assert_eq!(result, Err(ParseError::ScratchTooSmall));
    let mut over_byte_record = [0_u8; 630];
    over_byte_record[..escaped.len()].copy_from_slice(escaped);
    over_byte_record[escaped.len()] = b' ';
    let (result, allocations) = counted_parse(&over_byte_record, scratch, 629, 4);
    assert_eq!(allocations, 0);
    assert_eq!(result, Err(ParseError::RecordTooLarge));
    let too_deep = include_bytes!("../specs/cargo-val/oracles/c2-over-depth.jsonl");
    let (result, allocations) = counted_parse(too_deep, scratch, 629, 4);
    assert_eq!(allocations, 0);
    assert_eq!(result, Err(ParseError::DepthExceeded));
    let (result, allocations) = counted_parse(too_deep, scratch, 629, 17);
    assert_eq!(allocations, 0);
    assert_eq!(result, Err(ParseError::InvalidDepthLimit));
    let (result, allocations) = counted_parse(
            br#"{"reason":"compiler-artifact","target":{"kind":["lib"],"test":true},"profile":{"test":false},"executable":null}"#,
            scratch,
            629,
            4,
        );
    assert_eq!(allocations, 0);
    assert_eq!(
        result.expect("Cargo null-executable record must parse"),
        ArtifactRecord::Ignored
    );
}

fn assert_malformed_and_escaped_field_parses(scratch: &mut [u8]) {
    assert!(parse_artifact(b"{", scratch, 629, 4).is_err());
    assert_eq!(
        parse_artifact(br#"{"reason":"build-finished",}"#, scratch, 629, 4,),
        Err(ParseError::InvalidJson)
    );
    assert_eq!(
        parse_artifact(
            br#"{"reason":"build-finished","reason":"compiler-artifact"}"#,
            scratch,
            629,
            4,
        ),
        Err(ParseError::InvalidJson)
    );
    assert_eq!(
        parse_artifact(br#"{"future":{"field":1,}}"#, scratch, 629, 4,),
        Err(ParseError::InvalidJson)
    );
    assert_eq!(
        parse_artifact(br#"{"future":[1,]}"#, scratch, 629, 4,),
        Err(ParseError::InvalidJson)
    );
    assert_eq!(
        parse_artifact(
            br#"{"reason":"compiler-artifact","profile":{"test":true,},"executable":"x"}"#,
            scratch,
            629,
            4,
        ),
        Err(ParseError::InvalidJson)
    );
    let escaped_fields = include_bytes!("../specs/cargo-val/oracles/c2-escaped-fields.jsonl");
    let escaped_fields = &escaped_fields[..escaped_fields.len() - 1];
    let expected_escaped_fields =
        include_str!("../specs/cargo-val/oracles/c2-escaped-fields.expected.jsonl");
    let expected_escaped_fields: serde_json::Value = serde_json::from_str(expected_escaped_fields)
        .expect("the escaped-field expected record must parse");
    let (result, allocations) = counted_parse(escaped_fields, scratch, 629, 4);
    assert_eq!(allocations, 0);
    let expected_path = expected_escaped_fields["executable"]
        .as_str()
        .expect("the escaped-field path must be text");
    assert_eq!(
        result,
        Ok(ArtifactRecord::TestExecutable(ExecutablePath::Borrowed(
            expected_path
        )))
    );
    assert_eq!(
        parse_artifact(b"{\"future\":\"\\\\n\xff\"}", scratch, 629, 4,),
        Err(ParseError::InvalidUtf8)
    );
    assert_eq!(
        parse_artifact(
            br#"{"reason":"build-finished","success":true}"#,
            scratch,
            629,
            4
        )
        .expect("valid non-artifact Cargo message must parse"),
        ArtifactRecord::Ignored
    );
}

#[test]
fn artifact_records_borrow_decode_and_enforce_caller_bounds() {
    ALLOCATION_COUNT.store(0, Ordering::Relaxed);
    COUNT_ALLOCATIONS.store(true, Ordering::Relaxed);
    let allocation_control = vec![1_u8];
    COUNT_ALLOCATIONS.store(false, Ordering::Relaxed);
    assert_eq!(ALLOCATION_COUNT.load(Ordering::Relaxed), 1);
    assert_eq!(allocation_control, [1]);

    let input = include_str!("../specs/cargo-val/oracles/c2.jsonl");
    let expected = include_str!("../specs/cargo-val/oracles/c2.expected.jsonl");
    let expected: serde_json::Value =
        serde_json::from_str(expected).expect("the expected record must parse");
    let mut emitted_executables = Vec::new();
    let mut ignored_artifacts = 0;
    let mut scratch = [0; 512];

    for record in input.lines() {
        let record_bytes = record.as_bytes();
        let (result, allocations) = counted_parse(record_bytes, &mut scratch, 629, 4);
        assert_eq!(allocations, 0);
        match result.expect("literal Cargo artifact must parse") {
            ArtifactRecord::TestExecutable(ExecutablePath::Borrowed(executable)) => {
                assert!(
                    record_bytes
                        .windows(executable.len())
                        .any(|window| { window.as_ptr() == executable.as_bytes().as_ptr() })
                );
                emitted_executables.push(executable);
            }
            ArtifactRecord::TestExecutable(ExecutablePath::Scratch(_)) => {
                panic!("unescaped Cargo path must borrow the input")
            }
            ArtifactRecord::Ignored => ignored_artifacts += 1,
        }
    }

    assert_escaped_and_bounded_parses(&mut scratch);
    assert_eq!(
        emitted_executables.len(),
        expected["emitted_executables"].as_array().unwrap().len()
    );
    assert_eq!(
        ignored_artifacts,
        usize::try_from(expected["ignored_non_test_artifacts"].as_u64().unwrap())
            .expect("ignored artifact count fits usize")
    );
    assert_eq!(
        emitted_executables,
        expected["emitted_executables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_malformed_and_escaped_field_parses(&mut scratch);
}
