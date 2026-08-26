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

/// Measured cost of a columnar scan per row touched — the denominator of
/// every scan-vs-index comparison.
///
/// From the comparison benches: a full filtered scan over 1M rows runs ~4 ms,
/// i.e. ~4 ns per row. Held flat deliberately: per-row scan work is the one
/// thing in this engine that genuinely does not depend on collection size.
pub const SCAN_NS_PER_ROW: u64 = 4;

/// Which access path an estimate is for. Deliberately its own small enum
/// rather than a reference to [`crate::physical::PhysicalOp`]: the estimator
/// should stay usable from the optimizer, benchmarks and tooling without
/// dragging plan nodes along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPath {
    /// Read every row.
    Scan,
    /// One index descent answering one predicate.
    IndexLookup,
    /// N index descents for N ids.
    GetByIds(usize),
}

/// Estimated nanoseconds for `AccessPath` against `rows` logical rows.
///
/// The point of this function is that the flat-lookup assumption now lives
/// here and nowhere else: anything ranking access paths — planner, adaptive
/// optimizer, benchmark harness — calls this instead of assuming a lookup
/// costs what it cost at 100k rows forever.
pub fn access_ns(path: AccessPath, rows: u64) -> u64 {
    match path {
        AccessPath::Scan => rows.saturating_mul(SCAN_NS_PER_ROW),
        AccessPath::IndexLookup => point_lookup_ns(rows),
        // Each id pays one descent; ids repeat rarely enough that dedup
        // credit is not worth modelling yet.
        AccessPath::GetByIds(n) => (n as u64).saturating_mul(point_lookup_ns(rows)),
    }
}

/// The number of matching rows at which reading the collection once beats
/// answering through index descents — the boundary a planner should compare
/// its match-count estimate against when it knows both sizes.
pub fn scan_wins_over_lookups(rows: u64) -> u64 {
    let per_lookup = point_lookup_ns(rows);
    (rows.saturating_mul(SCAN_NS_PER_ROW)) / per_lookup.max(1)
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

    #[test]
    fn access_estimates_rank_paths_sensibly() {
        // Small collection: a scan is cheaper than even one calibrated
        // lookup — which is exactly why tiny tables need no indexes.
        assert!(
            access_ns(AccessPath::Scan, 500) < access_ns(AccessPath::IndexLookup, 500),
            "a 500-row scan should estimate under a fixed-cost lookup"
        );
        // Large collection with a selective predicate: the index wins big.
        assert!(
            access_ns(AccessPath::IndexLookup, 800_000)
                < access_ns(AccessPath::Scan, 800_000) / 100,
            "one lookup must beat a full scan by orders of magnitude"
        );
        // Batched identity reads grow linearly in how many you want.
        assert_eq!(
            access_ns(AccessPath::GetByIds(10), 100_000),
            10 * access_ns(AccessPath::IndexLookup, 100_000)
        );
    }

    #[test]
    fn the_crossover_is_where_the_curves_meet() {
        // At 800k rows: a scan costs 3.2 ms, one lookup ~12.4 µs, so roughly
        // 260 matches is the break-even — and the estimates must actually
        // agree with that on both sides of the line.
        let rows = 800_000u64;
        let threshold = scan_wins_over_lookups(rows);
        let lookup = access_ns(AccessPath::IndexLookup, rows);
        let scan = access_ns(AccessPath::Scan, rows);
        assert!(
            threshold * lookup < scan && (threshold + 1) * lookup >= scan,
            "threshold {threshold} must be the exact break-even between \
             {lookup} ns lookups and a {scan} ns scan"
        );
    }
}
