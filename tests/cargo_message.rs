use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::hint::black_box;
use std::alloc::System;

use cargo_val::ParseError;
use cargo_val::cargo_message::{CargoMessage, parse_cargo_message};
use cargo_val::test_environment::LinkedDirectories;

static TEST_ARTIFACT: &[u8] = include_bytes!("fixtures/compiler_artifact_test.jsonl");
static BUILD_SCRIPT: &[u8] = include_bytes!("fixtures/build_script_executed.jsonl");
const RECORD_LIMIT: usize = 4096;
const DEPTH_LIMIT: usize = 8;

struct CountingAllocator;

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACKING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATION_COUNT.try_with(|count| count.set(count.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}

fn count_allocations<T>(action: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATION_COUNT.with(|count| count.set(0));
    TRACKING.with(|tracking| tracking.set(true));
    let output = action();
    TRACKING.with(|tracking| tracking.set(false));
    (output, ALLOCATION_COUNT.with(Cell::get))
}

fn parse<'text>(
    line: &'text [u8],
    scratch: &'text mut [u8],
) -> Result<CargoMessage<'text>, ParseError> {
    parse_cargo_message(line, scratch, RECORD_LIMIT, DEPTH_LIMIT)
}

#[proxima::test]
async fn test_artifact_carries_executable_package_and_manifest_borrowed_from_the_record() {
    let mut scratch = [0_u8; 64];

    let (message, allocations) = count_allocations(|| parse(TEST_ARTIFACT, &mut scratch));

    let Ok(CargoMessage::TestArtifact(artifact)) = message else {
        panic!("real compiler-artifact test record was not selected: {message:?}");
    };
    assert_eq!(
        artifact.executable,
        "/work/probe_target/debug/deps/buildscript_probe-3724f85eaba568c3"
    );
    assert_eq!(
        artifact.package_id,
        "path+file:///work/buildscript_probe#2.7.1-rc.2"
    );
    assert_eq!(artifact.manifest_path, "/work/buildscript_probe/Cargo.toml");
    let record = TEST_ARTIFACT.as_ptr_range();
    assert!(record.contains(&artifact.executable.as_ptr()));
    assert_eq!(allocations, 0);
}

#[proxima::test]
async fn build_script_reports_search_paths_output_directory_and_environment_in_emitted_order() {
    let mut scratch = [0_u8; 1024];
    let mut storage = [0_u8; 512];

    let (observed, allocations) = count_allocations(|| {
        let message = parse(BUILD_SCRIPT, &mut scratch);
        let mut directories = LinkedDirectories::new(&mut storage);
        if let Ok(CargoMessage::BuildScript(script)) = message {
            for reported in script.linked_paths.iter() {
                directories.add(reported).expect("reported path fits");
            }
        }
        (message, directories.as_str().len())
    });

    let (Ok(CargoMessage::BuildScript(script)), joined_length) = observed else {
        panic!("real build-script-executed record was not selected: {observed:?}");
    };
    assert_eq!(
        script.package_id,
        "path+file:///work/buildscript_probe#2.7.1-rc.2"
    );
    assert_eq!(
        script.out_dir,
        "/work/probe_target/debug/build/buildscript_probe-b6efe4fe62985510/out"
    );
    let reported: Vec<&str> = script.linked_paths.iter().collect();
    assert_eq!(
        reported,
        [
            "native=/work/probe_target/debug/build/buildscript_probe-b6efe4fe62985510/out",
            "dependency=/opt/homebrew/lib",
        ]
    );
    let environment: Vec<(&str, &str)> = script.environment.iter().collect();
    assert_eq!(
        environment,
        [
            ("PROBE_GREETING", "hello \"quoted\" world"),
            ("PROBE_ANSWER", "42"),
        ]
    );
    let expected_length = script.out_dir.len() + 1 + "/opt/homebrew/lib".len();
    assert_eq!(joined_length, expected_length);
    assert_eq!(allocations, 0);
}

#[proxima::test]
async fn build_script_without_search_paths_or_variables_still_reports_its_output_directory() {
    let record = br#"{"reason":"build-script-executed","package_id":"path+file:///work/a#0.1.0","linked_libs":[],"linked_paths":[],"cfgs":[],"env":[],"out_dir":"/work/a/out"}"#;
    let mut scratch = [0_u8; 8];

    let message = parse(record, &mut scratch);

    let Ok(CargoMessage::BuildScript(script)) = message else {
        panic!("silent build script was not selected: {message:?}");
    };
    assert_eq!(script.out_dir, "/work/a/out");
    assert_eq!(script.linked_paths.iter().count(), 0);
    assert_eq!(script.environment.iter().count(), 0);
}

#[proxima::test]
async fn escaped_windows_paths_are_decoded_into_scratch() {
    let manifest = "C:\\work\\buildscript_probe\\Cargo.toml";
    let executable = "C:\\work\\target\\debug\\deps\\buildscript_probe-3724f85eaba568c3.exe";
    let record = serde_json::json!({
        "reason": "compiler-artifact",
        "package_id": "path+file:///C:/work/buildscript_probe#2.7.1-rc.2",
        "manifest_path": manifest,
        "profile": { "test": true },
        "executable": executable,
    })
    .to_string();
    let mut scratch = [0_u8; 512];

    let message = parse(record.as_bytes(), &mut scratch);

    let Ok(CargoMessage::TestArtifact(artifact)) = message else {
        panic!("escaped record was not selected: {message:?}");
    };
    assert_eq!(artifact.executable, executable);
    assert_eq!(artifact.manifest_path, manifest);
    assert!(
        !record
            .as_bytes()
            .as_ptr_range()
            .contains(&artifact.executable.as_ptr())
    );
}

#[proxima::test]
async fn records_without_a_test_executable_are_ignored() {
    let not_test_profile = br#"{"reason":"compiler-artifact","package_id":"path+file:///work/a#0.1.0","manifest_path":"/work/a/Cargo.toml","profile":{"test":false},"executable":"/work/a/target/debug/a"}"#;
    let no_executable = br#"{"reason":"compiler-artifact","package_id":"path+file:///work/a#0.1.0","manifest_path":"/work/a/Cargo.toml","profile":{"test":true},"executable":null}"#;
    let build_finished = br#"{"reason":"build-finished","success":true}"#;
    let mut scratch = [0_u8; 64];

    for record in [&not_test_profile[..], &no_executable[..]] {
        assert_eq!(parse(record, &mut scratch), Ok(CargoMessage::Ignored));
    }
    assert_eq!(
        parse(build_finished, &mut scratch),
        Ok(CargoMessage::BuildFinished { success: true })
    );
}

#[proxima::test]
async fn malformed_artifacts_are_rejected_with_their_reason() {
    let missing_package = br#"{"reason":"compiler-artifact","manifest_path":"/work/a/Cargo.toml","profile":{"test":true},"executable":"/work/a/t"}"#;
    let missing_manifest = br#"{"reason":"compiler-artifact","package_id":"path+file:///work/a#0.1.0","profile":{"test":true},"executable":"/work/a/t"}"#;
    let build_script_without_output = br#"{"reason":"build-script-executed","package_id":"path+file:///work/a#0.1.0","linked_libs":[],"linked_paths":[],"cfgs":[],"env":[]}"#;
    let duplicate_package = br#"{"reason":"compiler-artifact","package_id":"x","package_id":"y","manifest_path":"/work/a/Cargo.toml","profile":{"test":true},"executable":"/work/a/t"}"#;
    let mut scratch = [0_u8; 8];

    assert_eq!(
        parse(missing_package, &mut scratch),
        Err(ParseError::MissingArtifactField)
    );
    assert_eq!(
        parse(missing_manifest, &mut scratch),
        Err(ParseError::MissingArtifactField)
    );
    assert_eq!(
        parse(build_script_without_output, &mut scratch),
        Err(ParseError::MissingArtifactField)
    );
    assert_eq!(
        parse(duplicate_package, &mut scratch),
        Err(ParseError::InvalidJson)
    );
    assert_eq!(
        parse(BUILD_SCRIPT, &mut scratch),
        Err(ParseError::ScratchTooSmall)
    );
}

#[proxima::test]
async fn allocation_probe_detects_a_heap_allocation() {
    let (_, allocations) = count_allocations(|| black_box(Vec::<u8>::with_capacity(1)));

    assert_eq!(allocations, 1);
}

#[proxima::test]
async fn a_built_bin_target_is_reported_by_its_package_and_a_bin_unit_test_is_not() {
    let bin = br#"{"reason":"compiler-artifact","package_id":"path+file:///work/a#0.1.0","manifest_path":"/work/a/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"a"},"profile":{"test":false},"executable":"/work/a/target/debug/a"}"#;
    let library = br#"{"reason":"compiler-artifact","package_id":"path+file:///work/a#0.1.0","manifest_path":"/work/a/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"a"},"profile":{"test":false},"executable":null}"#;
    let bin_unit_test = br#"{"reason":"compiler-artifact","package_id":"path+file:///work/a#0.1.0","manifest_path":"/work/a/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"a"},"profile":{"test":true},"executable":"/work/a/target/debug/deps/a-0123456789abcdef"}"#;
    let mut scratch = [0_u8; 512];

    assert_eq!(
        parse(bin, &mut scratch),
        Ok(CargoMessage::BinArtifact {
            package_id: "path+file:///work/a#0.1.0",
            executable: "/work/a/target/debug/a",
        })
    );
    assert_eq!(parse(library, &mut scratch), Ok(CargoMessage::Ignored));
    assert!(matches!(
        parse(bin_unit_test, &mut scratch),
        Ok(CargoMessage::TestArtifact(_))
    ));
}

#[proxima::test]
async fn crlf_terminated_artifact_record_selects_the_same_executable_as_lf() {
    let mut lf_record = TEST_ARTIFACT.to_vec();
    while matches!(lf_record.last(), Some(b'\n' | b'\r')) {
        lf_record.pop();
    }
    let mut crlf_record = lf_record.clone();
    crlf_record.extend_from_slice(b"\r\n");
    lf_record.push(b'\n');
    let mut lf_scratch = [0_u8; 64];
    let mut crlf_scratch = [0_u8; 64];

    let lf = parse(&lf_record, &mut lf_scratch).expect("lf record parses");
    let crlf = parse(&crlf_record, &mut crlf_scratch).expect("crlf record parses");

    assert_eq!(lf, crlf);
}
