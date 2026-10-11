fn ordered_fold(seed: u64) -> u64 {
    (0..1_000_000_u64).fold(seed, |state, value| {
        std::hint::black_box(state.wrapping_add(value ^ (value >> 3)).rotate_left(9))
    })
}

#[test]
fn order_seed_thirty_one() {
    assert_ne!(std::hint::black_box(ordered_fold(31)), 0);
}

#[test]
fn order_seed_thirty_seven() {
    assert_ne!(std::hint::black_box(ordered_fold(37)), 0);
}

#[test]
fn order_seed_forty_one() {
    assert_ne!(std::hint::black_box(ordered_fold(41)), 0);
}

#[test]
fn order_seed_forty_three() {
    assert_ne!(std::hint::black_box(ordered_fold(43)), 0);
}
