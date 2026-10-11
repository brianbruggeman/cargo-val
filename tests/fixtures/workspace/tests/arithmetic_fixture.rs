fn checksum(seed: u64) -> u64 {
    (0..1_000_000_u64).fold(seed, |state, value| {
        std::hint::black_box(state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(value))
    })
}

#[test]
fn arithmetic_seed_one() {
    assert_ne!(std::hint::black_box(checksum(1)), 0);
}

#[test]
fn arithmetic_seed_two() {
    assert_ne!(std::hint::black_box(checksum(2)), 0);
}

#[test]
fn arithmetic_seed_three() {
    assert_ne!(std::hint::black_box(checksum(3)), 0);
}

#[test]
fn arithmetic_seed_four() {
    assert_ne!(std::hint::black_box(checksum(4)), 0);
}
