fn branch_count(seed: u64) -> u64 {
    (0..1_000_000_u64).fold(0, |count, value| {
        count + u64::from((value.wrapping_mul(seed) & 31) < 7)
    })
}

#[test]
fn branch_seed_five() {
    assert!(std::hint::black_box(branch_count(5)) > 0);
}

#[test]
fn branch_seed_seven() {
    assert!(std::hint::black_box(branch_count(7)) > 0);
}

#[test]
fn branch_seed_eleven() {
    assert!(std::hint::black_box(branch_count(11)) > 0);
}

#[test]
fn branch_seed_thirteen() {
    assert!(std::hint::black_box(branch_count(13)) > 0);
}
