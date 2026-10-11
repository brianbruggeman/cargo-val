const TWO_POW_32: f64 = 4_294_967_296.0;

#[derive(Debug, Clone)]
pub struct SplitMix {
    state: u64,
}

impl SplitMix {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    }

    /// uniform in the open interval (0, 1); never 0 so `ln` is finite
    pub fn next_unit(&mut self) -> f64 {
        let high = u32::try_from(self.next_u64() >> 32).unwrap_or(u32::MAX);
        (f64::from(high) + 0.5) / TWO_POW_32
    }

    /// standard normal via box-muller
    pub fn next_normal(&mut self) -> f64 {
        let radius = (-2.0 * self.next_unit().ln()).sqrt();
        radius * (std::f64::consts::TAU * self.next_unit()).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut left = SplitMix::new(42);
        let mut right = SplitMix::new(42);
        assert!((0..100).all(|_| left.next_u64() == right.next_u64()));
    }

    #[test]
    fn unit_stays_in_open_interval() {
        let mut rng = SplitMix::new(7);
        assert!((0..100_000).all(|_| {
            let value = rng.next_unit();
            value > 0.0 && value < 1.0
        }));
    }
}
