use std::path::Path;

#[test]
fn the_bin_target_exists_when_the_test_starts() {
    let bin = env!("CARGO_BIN_EXE_gate-bin");
    assert!(Path::new(bin).is_file(), "{bin} was not built before this test launched");
}
