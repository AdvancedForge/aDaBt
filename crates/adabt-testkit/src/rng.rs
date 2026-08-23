//! SplitMix64.
//!
//! Deliberately dependency-free and fully specified, so a failing differential
//! run reproduces bit-for-bit from its seed alone, on any platform, forever. A
//! third-party RNG could change its stream across a version bump and silently
//! invalidate every recorded reproduction.

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`. Panics if `n == 0`.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0) is undefined");
        // Rejection sampling; unbiased, unlike a bare modulo.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % n;
            }
        }
    }

    #[inline]
    pub fn below_usize(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }

    /// True with probability `p`.
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below_usize(items.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_yields_same_stream() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let (mut a, mut b) = (Rng::new(1), Rng::new(2));
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut r = Rng::new(7);
        let mut seen = [false; 10];
        for _ in 0..10_000 {
            let v = r.below(10);
            assert!(v < 10);
            seen[v as usize] = true;
        }
        assert!(
            seen.iter().all(|&s| s),
            "below(10) never produced some values"
        );
    }

    #[test]
    fn chance_respects_its_probability() {
        let mut r = Rng::new(99);
        let hits = (0..100_000).filter(|_| r.chance(0.25)).count();
        assert!((20_000..30_000).contains(&hits), "hits={hits}");
        assert!(!r.chance(0.0));
        assert!(r.chance(1.0));
    }
}
