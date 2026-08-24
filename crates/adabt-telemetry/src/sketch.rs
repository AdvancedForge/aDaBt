//! Count-min sketch with exponential decay, for data temperature.
//!
//! The adaptive driver needs to know which records are hot. Counting per record
//! would cost memory proportional to the dataset — telemetry as expensive as
//! the data it describes — so this trades exactness for a fixed footprint: a
//! few tens of kilobytes regardless of whether the collection holds a thousand
//! rows or a billion.
//!
//! Two properties make it safe to act on. It never *under*-counts, so a record
//! the sketch calls cold is genuinely cold and evicting it cannot be a surprise.
//! And counts decay, so "hot" means hot *lately* rather than hot once — which
//! matters because the whole point of adaptation is that workloads change.

/// Independent hash seeds. One per row of the sketch.
const SEEDS: [u64; 4] = [
    0x9E37_79B9_7F4A_7C15,
    0xBF58_476D_1CE4_E5B9,
    0x94D0_49BB_1331_11EB,
    0xC2B2_AE3D_27D4_EB4F,
];

#[derive(Debug, Clone)]
pub struct TemperatureSketch {
    width: usize,
    counts: Vec<u32>,
    observations: u64,
    /// Halve every counter after this many observations.
    decay_interval: u64,
    decays: u64,
}

impl Default for TemperatureSketch {
    fn default() -> Self {
        Self::new(4096, 100_000)
    }
}

impl TemperatureSketch {
    /// `width` counters per row; four rows. Wider means less collision error.
    pub fn new(width: usize, decay_interval: u64) -> Self {
        assert!(width.is_power_of_two(), "width must be a power of two");
        assert!(decay_interval > 0);
        Self {
            width,
            counts: vec![0; width * SEEDS.len()],
            observations: 0,
            decay_interval,
            decays: 0,
        }
    }

    #[inline]
    fn slot(&self, key: u64, row: usize) -> usize {
        let mut h = key ^ SEEDS[row];
        h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
        row * self.width + (h as usize & (self.width - 1))
    }

    pub fn observe(&mut self, key: u64) {
        for row in 0..SEEDS.len() {
            let s = self.slot(key, row);
            self.counts[s] = self.counts[s].saturating_add(1);
        }
        self.observations += 1;
        if self.observations % self.decay_interval == 0 {
            self.decay();
        }
    }

    /// Halve every counter, so old activity fades rather than accumulating
    /// forever. A record that was hot last week must not look hot today.
    pub fn decay(&mut self) {
        for c in &mut self.counts {
            *c /= 2;
        }
        self.decays += 1;
    }

    /// Estimated recent access count. Never below the true decayed count.
    pub fn estimate(&self, key: u64) -> u32 {
        (0..SEEDS.len())
            .map(|row| self.counts[self.slot(key, row)])
            .min()
            .unwrap_or(0)
    }

    /// Whether `key` is at least `factor` times as active as the average slot.
    pub fn is_hot(&self, key: u64, factor: f64) -> bool {
        let avg = self.average_count();
        avg > 0.0 && self.estimate(key) as f64 >= avg * factor
    }

    fn average_count(&self) -> f64 {
        if self.counts.is_empty() {
            return 0.0;
        }
        let row_len = self.width;
        let row_sum: u64 = self.counts[..row_len].iter().map(|c| *c as u64).sum();
        row_sum as f64 / row_len as f64
    }

    pub fn observations(&self) -> u64 {
        self.observations
    }
    pub fn decays(&self) -> u64 {
        self.decays
    }
    pub fn memory_bytes(&self) -> usize {
        self.counts.len() * 4 + std::mem::size_of::<Self>()
    }

    pub fn merge(&mut self, other: &TemperatureSketch) {
        if self.width != other.width {
            return;
        }
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a = a.saturating_add(*b);
        }
        self.observations += other.observations;
    }

    pub fn clear(&mut self) {
        self.counts.iter_mut().for_each(|c| *c = 0);
        self.observations = 0;
        self.decays = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hot_key_estimates_higher_than_a_cold_one() {
        let mut s = TemperatureSketch::new(4096, u64::MAX);
        for _ in 0..10_000 {
            s.observe(42);
        }
        s.observe(99);
        assert!(s.estimate(42) > s.estimate(99) * 100);
    }

    #[test]
    fn it_never_underestimates() {
        // The property that makes eviction safe: a key the sketch calls cold is
        // genuinely cold, so acting on that judgment cannot surprise anyone.
        let mut s = TemperatureSketch::new(1024, u64::MAX);
        let mut truth = std::collections::HashMap::new();
        for i in 0..5_000u64 {
            let key = i % 300;
            s.observe(key);
            *truth.entry(key).or_insert(0u32) += 1;
        }
        for (key, count) in truth {
            assert!(
                s.estimate(key) >= count,
                "underestimated key {key}: {} < {count}",
                s.estimate(key)
            );
        }
    }

    #[test]
    fn memory_is_bounded_regardless_of_key_count() {
        let mut s = TemperatureSketch::new(4096, u64::MAX);
        let before = s.memory_bytes();
        for i in 0..1_000_000u64 {
            s.observe(i);
        }
        assert_eq!(
            s.memory_bytes(),
            before,
            "the sketch grew with the key space"
        );
        assert!(before < 100_000, "sketch is larger than expected: {before}");
    }

    #[test]
    fn decay_cools_a_key_that_stops_being_used() {
        // What makes this a *temperature* rather than a total: adaptation only
        // works if the measure reflects the workload that exists now.
        let mut s = TemperatureSketch::new(4096, u64::MAX);
        for _ in 0..10_000 {
            s.observe(7);
        }
        let hot = s.estimate(7);
        for _ in 0..8 {
            s.decay();
        }
        let cooled = s.estimate(7);
        assert!(cooled * 100 < hot, "{cooled} did not cool from {hot}");
    }

    #[test]
    fn decay_happens_automatically_at_the_configured_interval() {
        let mut s = TemperatureSketch::new(64, 100);
        for i in 0..1_000u64 {
            s.observe(i);
        }
        assert_eq!(s.decays(), 10);
        assert_eq!(s.observations(), 1_000);
    }

    #[test]
    fn a_recently_hot_key_beats_a_formerly_hot_one() {
        let mut s = TemperatureSketch::new(4096, u64::MAX);
        for _ in 0..5_000 {
            s.observe(1); // hot in the past
        }
        for _ in 0..6 {
            s.decay();
        }
        for _ in 0..500 {
            s.observe(2); // hot now
        }
        assert!(
            s.estimate(2) > s.estimate(1),
            "the formerly hot key still looks hotter: {} vs {}",
            s.estimate(1),
            s.estimate(2)
        );
    }

    #[test]
    fn is_hot_separates_a_skewed_key_from_the_background() {
        let mut s = TemperatureSketch::new(4096, u64::MAX);
        for i in 0..20_000u64 {
            s.observe(i % 2_000);
        }
        for _ in 0..20_000 {
            s.observe(999_999);
        }
        assert!(s.is_hot(999_999, 4.0));
        assert!(!s.is_hot(5, 4.0));
    }

    #[test]
    fn an_empty_sketch_reports_nothing_hot() {
        let s = TemperatureSketch::new(64, u64::MAX);
        assert_eq!(s.estimate(1), 0);
        assert!(!s.is_hot(1, 2.0));
    }

    #[test]
    fn merging_combines_two_sketches() {
        let (mut a, mut b) = (
            TemperatureSketch::new(1024, u64::MAX),
            TemperatureSketch::new(1024, u64::MAX),
        );
        for _ in 0..500 {
            a.observe(3);
            b.observe(3);
        }
        let solo = a.estimate(3);
        a.merge(&b);
        assert_eq!(a.estimate(3), solo * 2);
        assert_eq!(a.observations(), 1_000);
    }

    #[test]
    fn merging_a_differently_sized_sketch_is_ignored_rather_than_corrupting() {
        let mut a = TemperatureSketch::new(1024, u64::MAX);
        let mut b = TemperatureSketch::new(64, u64::MAX);
        b.observe(1);
        a.merge(&b);
        assert_eq!(a.observations(), 0);
    }

    #[test]
    fn counters_saturate_rather_than_wrapping() {
        let mut s = TemperatureSketch::new(4, u64::MAX);
        s.counts.fill(u32::MAX);
        s.observe(1);
        assert_eq!(s.estimate(1), u32::MAX);
    }
}
