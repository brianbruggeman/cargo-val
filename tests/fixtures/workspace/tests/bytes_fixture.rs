fn byte_mix(seed: u8) -> u64 {
    (0..1_000_000_u64).fold(u64::from(seed), |state, value| {
        let byte = (value as u8).wrapping_mul(seed).rotate_left((value & 7) as u32);
        std::hint::black_box(state.rotate_left(5) ^ u64::from(byte))
    })
}

#[test]
fn bytes_seed_seventeen() {
    assert_ne!(std::hint::black_box(byte_mix(17)), 0);
}

#[test]
fn bytes_seed_nineteen() {
    assert_ne!(std::hint::black_box(byte_mix(19)), 0);
}

#[test]
fn bytes_seed_twenty_three() {
    assert_ne!(std::hint::black_box(byte_mix(23)), 0);
}

#[test]
fn bytes_seed_twenty_nine() {
    assert_ne!(std::hint::black_box(byte_mix(29)), 0);
}
