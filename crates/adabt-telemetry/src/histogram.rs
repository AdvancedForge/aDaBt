//! Log-linear latency histogram.
//!
//! Records values across the full `u64` range in bounded memory with a fixed
//! relative error, so p99/p999 stay meaningful without retaining samples. The
//! layout is the usual HDR arrangement: an exact linear region for small
//! values, then `SUB` linearly-spaced buckets per power of two.

const SUB_BITS: u32 = 4;
const SUB: u64 = 1 << SUB_BITS;
/// One row per octave above the linear region, plus the linear region itself.
const BUCKETS: usize = ((64 - SUB_BITS + 1) * SUB as u32) as usize;

#[derive(Clone)]
pub struct Histogram {
    counts: Box<[u64; BUCKETS]>,
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histogram")
            .field("count", &self.count)
            .field("min", &self.min())
            .field("p50", &self.percentile(50.0))
            .field("p99", &self.percentile(99.0))
            .field("max", &self.max)
            .finish()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            counts: Box::new([0; BUCKETS]),
            count: 0,
            sum: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    #[inline]
    fn bucket(v: u64) -> usize {
        if v < SUB {
            return v as usize;
        }
        let octave = 63 - v.leading_zeros();
        let shift = octave - SUB_BITS;
        let sub = (v >> shift) - SUB;
        ((octave - SUB_BITS + 1) as u64 * SUB + sub) as usize
    }

    /// Lower bound of the values falling in `index`.
    #[inline]
    fn value_at(index: usize) -> u64 {
        let i = index as u64;
        if i < SUB {
            return i;
        }
        let row = i / SUB;
        let sub = i % SUB;
        (SUB + sub) << (row - 1)
    }

    #[inline]
    pub fn record(&mut self, v: u64) {
        self.counts[Self::bucket(v)] += 1;
        self.count += 1;
        self.sum = self.sum.saturating_add(v);
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a += *b;
        }
        self.count += other.count;
        self.sum = self.sum.saturating_add(other.sum);
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    pub fn count(&self) -> u64 {
        self.count
    }
    pub fn sum(&self) -> u64 {
        self.sum
    }
    pub fn min(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.min
        }
    }
    pub fn max(&self) -> u64 {
        self.max
    }
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum as f64 / self.count as f64
        }
    }

    /// Value at `p` (0-100), as the lower bound of the containing bucket.
    /// Relative error is at most `1/SUB` (6.25%).
    pub fn percentile(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((p / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            seen += c;
            if seen >= target {
                // Never report below the smallest observed value: for a single
                // sample the bucket floor would understate it.
                return Self::value_at(i).max(self.min);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_are_exact() {
        let mut h = Histogram::new();
        for v in 0..SUB {
            h.record(v);
        }
        assert_eq!(h.percentile(0.0), 0);
        assert_eq!(h.max(), SUB - 1);
    }

    #[test]
    fn bucket_and_value_at_are_inverse_at_boundaries() {
        for v in [
            0u64,
            1,
            15,
            16,
            31,
            32,
            63,
            64,
            1_000,
            1_000_000,
            u64::MAX / 2,
        ] {
            let b = Histogram::bucket(v);
            assert!(b < BUCKETS, "v={v} produced out-of-range bucket {b}");
            assert!(
                Histogram::value_at(b) <= v,
                "value_at({b})={} must not exceed v={v}",
                Histogram::value_at(b)
            );
        }
    }

    #[test]
    fn relative_error_stays_within_one_sub_bucket() {
        for v in [100u64, 1_000, 12_345, 999_999, 1 << 40] {
            let b = Histogram::bucket(v);
            let lo = Histogram::value_at(b);
            let err = (v - lo) as f64 / v as f64;
            assert!(err <= 1.0 / SUB as f64, "v={v} error={err}");
        }
    }

    #[test]
    fn percentiles_track_a_known_distribution() {
        let mut h = Histogram::new();
        for v in 1..=1000u64 {
            h.record(v);
        }
        let p50 = h.percentile(50.0);
        let p99 = h.percentile(99.0);
        // Within one sub-bucket of the true 500 and 990.
        assert!((470..=500).contains(&p50), "p50={p50}");
        assert!((930..=990).contains(&p99), "p99={p99}");
        assert!(p50 < p99);
    }

    #[test]
    fn single_sample_percentile_is_not_understated() {
        let mut h = Histogram::new();
        h.record(1_000_000);
        assert!(h.percentile(50.0) >= 937_500);
    }

    #[test]
    fn merge_is_equivalent_to_recording_into_one() {
        let (mut a, mut b, mut both) = (Histogram::new(), Histogram::new(), Histogram::new());
        for v in 1..500u64 {
            a.record(v);
            both.record(v);
        }
        for v in 500..1000u64 {
            b.record(v);
            both.record(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), both.count());
        assert_eq!(a.percentile(99.0), both.percentile(99.0));
        assert_eq!(a.max(), both.max());
        assert_eq!(a.min(), both.min());
    }

    #[test]
    fn empty_histogram_reports_zeroes_rather_than_panicking() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.percentile(99.0), 0);
        assert_eq!(h.min(), 0);
        assert_eq!(h.mean(), 0.0);
    }
}
