use cargo_val_binary_count_fixture::checksum;

#[test]
fn checksum_differs_for_different_paths() {
    assert_ne!(checksum(b"/orders/17"), checksum(b"/orders/18"));
}
