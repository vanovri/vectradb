/// Deterministic, fast, zero-dependency 64-bit SplitMix64 PRNG.
/// Used across all VectraDB fuzzers, property tests, and synthetic data generators
/// to guarantee bit-identical cross-platform reproducibility from any `u64` seed.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    #[inline]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    #[inline]
    pub fn next_usize(&mut self, upper_exclusive: usize) -> usize {
        debug_assert!(upper_exclusive > 0);
        (self.next_u64() as usize) % upper_exclusive
    }

    #[inline]
    pub fn gen_range_i64(&mut self, low: i64, high_inclusive: i64) -> i64 {
        debug_assert!(low <= high_inclusive);
        let span = (high_inclusive as i128 - low as i128 + 1) as u128;
        if span > (u64::MAX as u128) {
            self.next_u64() as i64
        } else {
            let off = (self.next_u64() as u128 % span) as i128;
            (low as i128 + off) as i64
        }
    }

    #[inline]
    pub fn next_f64_unit(&mut self) -> f64 {
        // 53 bits of uniform precision in [0.0, 1.0)
        ((self.next_u64() >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
    }

    #[inline]
    pub fn gen_bool(&mut self, probability: f64) -> bool {
        self.next_f64_unit() < probability
    }

    #[inline]
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next_usize(items.len())]
    }

    pub fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let j = self.next_usize(i + 1);
            slice.swap(i, j);
        }
    }
}
