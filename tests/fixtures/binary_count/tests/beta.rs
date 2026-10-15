use cargo_val_binary_count_fixture::checksum;

#[test]
fn checksum_of_the_empty_body_is_zero() {
    assert_eq!(checksum(b""), 0);
}
