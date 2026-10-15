#[test]
fn doubling_a_large_count_is_exact() {
    pipeline_quick::held_until_released();
    assert_eq!(pipeline_quick::doubled(1000), 2000);
}
