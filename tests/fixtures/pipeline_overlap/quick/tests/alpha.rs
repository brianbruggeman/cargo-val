#[test]
fn doubling_a_request_count_is_exact() {
    pipeline_quick::held_until_released();
    assert_eq!(pipeline_quick::doubled(10), 20);
}
