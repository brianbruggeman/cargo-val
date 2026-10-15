#[test]
fn doubling_zero_is_zero() {
    pipeline_quick::held_until_released();
    assert_eq!(pipeline_quick::doubled(0), 0);
}
