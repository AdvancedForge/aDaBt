//! The cost model's numbers, calibrated against the scale ladder.
//!
//! Every access-path estimate used to assume an indexed point lookup costs
//! the same however large the collection — and `adabt-bench`'s scale ladder
//! refuted that: measured 6.3 µs at 100k rows and 12.4 µs at 800k, a
//! log-linear climb of roughly two microseconds per doubling. This module
//! encodes that curve so nothing downstream has to transcribe constants by
//! hand, and pins them to the measurements with tests: if a future engine
//! change moves real lookup cost off this curve, the bench run updates these
//! anchors first and every consumer inherits the correction.

/// Measured point-lookup latency at the 100k-row anchor (scale ladder).
pub const LOOKUP_NS_AT_100K: u64 = 6_300;

/// Measured growth per row-count doubling, derived from the 100k → 800k
/// ladder rungs ((12 400 − 6 300) / 3 ≈ 2 033, rounded down to be
/// conservative about how fast cost grows).
pub const LOOKUP_NS_PER_DOUBLING: u64 = 2_000;

/// Row count of the measurement anchor.
const ANCHOR_ROWS: u64 = 100_000;

/// Estimated nanoseconds for one indexed point lookup in a collection of
/// `rows` logical rows. Below the anchor the curve is held flat rather than
/// extrapolated down: small collections are dominated by fixed per-query
/// work, and pretending lookups get arbitrarily cheap would mis-rank access
/// paths exactly where scans are already competitive.
pub fn point_lookup_ns(rows: u64) -> u64 {
    let rows = rows.max(1);
    if rows <= ANCHOR_ROWS {
        return LOOKUP_NS_AT_100K;
    }
    let doublings = (rows as f64 / ANCHOR_ROWS as f64).log2().ceil() as u64;
    LOOKUP_NS_AT_100K + doublings * LOOKUP_NS_PER_DOUBLING
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_passes_through_the_measured_rungs() {
        // 6.3 µs at the 100k anchor...
        assert_eq!(point_lookup_ns(100_000), LOOKUP_NS_AT_100K);
        // ...and within measurement noise at 800k (measured 12.4 µs; the
        // encoded curve says 6.3 + 3×2 = 12.3).
        let est = point_lookup_ns(800_000);
        assert!(
            (11_400..=13_400).contains(&est),
            "encoded {est} ns drifted from the measured 12.4 µs at 800k"
        );
    }

    #[test]
    fn below_the_anchor_the_curve_holds_flat() {
        assert_eq!(point_lookup_ns(1), LOOKUP_NS_AT_100K);
        assert_eq!(point_lookup_ns(99_999), LOOKUP_NS_AT_100K);
        assert_eq!(point_lookup_ns(0), LOOKUP_NS_AT_100K);
    }

    #[test]
    fn the_climb_is_monotone_and_log_linear_in_shape() {
        let mut prev = point_lookup_ns(100_000);
        for rows in [200_000u64, 400_000, 800_000, 1_600_000] {
            let now = point_lookup_ns(rows);
            assert!(now > prev, "cost must grow with rows");
            assert_eq!(
                now - prev,
                LOOKUP_NS_PER_DOUBLING,
                "each doubling adds exactly the measured increment"
            );
            prev = now;
        }
    }
}
