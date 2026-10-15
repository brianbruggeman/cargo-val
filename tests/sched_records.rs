use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_val::build_defaults::SCHED_RECORD_BYTES;
use cargo_val::sched_binary::{Absent, RECORD_BYTES, decode};
use cargo_val::sched_file::{Loaded, load};

mod common;
#[path = "common/isolation.rs"]
mod isolation;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sched/Cargo.toml");

async fn built_test_binary() -> String {
    let output = fixture_output(
        Command::new("cargo")
            .args(["test", "--lib", "--no-run", "--message-format=json", "--manifest-path", FIXTURE])
            .envs(isolation::environment("sched-fixture")),
    )
    .await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("\"profile\":{") && line.contains("\"test\":true"))
        .find_map(|line| {
            let after = line.split("\"executable\":\"").nth(1)?;
            Some(after.split('"').next()?.replace("\\\\", "\\"))
        })
        .expect("cargo reports the test executable")
}

fn declared(binary: &str) -> Vec<(String, u8, String)> {
    let mut storage = vec![0_u8; SCHED_RECORD_BYTES];
    let Loaded::Records { count, little_endian } = load(binary, &mut storage).expect("binary is readable") else {
        panic!("the fixture binary declares scheduling records");
    };
    let (records, _) = storage[..count * RECORD_BYTES].as_chunks::<RECORD_BYTES>();
    let mut entries: Vec<_> = records
        .iter()
        .map(|record| {
            let entry = decode(record, little_endian).expect("record decodes");
            (
                String::from_utf8(entry.path.to_vec()).expect("path is UTF-8"),
                entry.flags,
                String::from_utf8(entry.group.to_vec()).expect("group is UTF-8"),
            )
        })
        .collect();
    entries.sort();
    entries
}

#[proxima::test(timeout = "310s")]
async fn the_host_binary_format_yields_every_declared_record_with_flags_and_group() {
    let entries = declared(&built_test_binary().await);

    let expected: Vec<(&str, u8, &str)> = vec![
        ("declared::declares_nothing", 0b000, ""),
        ("declared::decodes_each_size::large", 0b100, "real-checkpoint"),
        ("declared::decodes_each_size::small", 0b100, "real-checkpoint"),
        ("declared::also_shares_the_gpu_group_process", 0b110, "gpu"),
        ("declared::loads_whole_checkpoint", 0b001, ""),
        ("declared::runs_alone_in_the_shared_process", 0b011, ""),
        ("declared::shares_the_binary_process", 0b010, ""),
        ("declared::shares_the_gpu_group_process", 0b110, "gpu"),
    ];
    let mut expected: Vec<(String, u8, String)> = expected
        .into_iter()
        .map(|(path, flags, group)| (path.to_string(), flags, group.to_string()))
        .collect();
    expected.sort();
    assert_eq!(entries, expected);
}

#[proxima::test]
async fn a_binary_without_the_section_declares_nothing_and_a_non_object_is_unsupported() {
    let program = binary::command();
    let plain = Path::new(program.get_program()).to_str().expect("path is UTF-8").to_string();
    let garbage = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let mut storage = vec![0_u8; 4096];

    let from_binary = load(&plain, &mut storage).expect("cargo-val binary is readable");
    let from_text = load(garbage.to_str().expect("path is UTF-8"), &mut storage).expect("manifest is readable");

    assert_eq!(from_binary, Loaded::Absent(Absent::NoSection));
    assert_eq!(from_text, Loaded::Absent(Absent::Unsupported));
}
