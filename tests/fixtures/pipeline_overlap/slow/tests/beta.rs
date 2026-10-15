#[test]
fn tripling_a_request_count_is_exact() {
    assert_eq!(pipeline_slow::tripled(10), 30);
}
