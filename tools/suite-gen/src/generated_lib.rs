#[must_use]
pub fn mix(seed: u64, rounds: u64) -> u64 {
    let mut state = seed | 1;
    for round in 0..rounds {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(round);
    }
    state
}

#[must_use]
pub fn checksum(values: &[u64]) -> u64 {
    values
        .iter()
        .fold(0xCBF2_9CE4_8422_2325, |hash, value| (hash ^ value).wrapping_mul(0x0100_0000_01B3))
}

#[must_use]
pub fn clamp_add(left: u32, right: u32, ceiling: u32) -> u32 {
    left.saturating_add(right).min(ceiling)
}
