//! A cost model calibrated from observed outcomes.
//!
//! M8 ended with the driver arguing with itself: it dropped an index on
//! evidence that the planner never chose it, then re-added it three cycles
//! later on an estimate claiming a 70% latency improvement, forever. A
//! fifty-cycle cooldown stopped the flapping without addressing the cause,
//! which is that the estimate never learned it was wrong.
//!
//! This is the repair. Every applied change is measured, and the measurement
//! corrects the prior for the next time that optimization is considered.
//!
//! # What this measurement is and is not
//!
//! Attribution here is **crude on purpose**. Comparing the workload before and
//! after a change, on a live system, confounds the change with whatever else
//! moved — traffic shape, data growth, another optimization applied in the same
//! window. The model compensates by trusting a single observation very little
//! and by widening confidence only as observations accumulate and agree.
//!
//! Controlled attribution needs the candidate and the baseline running against
//! the same state at the same time, which is shadow execution — M11. Until
//! then this is a bias correction, not an experiment, and it is deliberately
//! weighted as one.

use std::collections::HashMap;

use crate::cost::{CostEstimate, Ratio};

/// Snapshot of what an optimization was supposed to affect.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metrics {
    pub p50_nanos: u64,
    pub p99_nanos: u64,
    pub bytes: u64,
    pub operations: u64,
}

impl Metrics {
    /// Whether there is enough here to compare against anything.
    pub fn is_usable(&self) -> bool {
        self.operations > 0 && self.p50_nanos > 0
    }
}

/// One before-and-after pair for one optimization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub p50_ratio: f64,
    pub p99_ratio: f64,
    pub byte_delta: i64,
}

impl Observation {
    pub fn between(before: &Metrics, after: &Metrics) -> Option<Observation> {
        if !before.is_usable() || !after.is_usable() {
            return None;
        }
        Some(Observation {
            p50_ratio: after.p50_nanos as f64 / before.p50_nanos as f64,
            p99_ratio: after.p99_nanos as f64 / before.p99_nanos as f64,
            byte_delta: after.bytes as i64 - before.bytes as i64,
        })
    }
}

/// Observations required before the model trusts itself over the prior.
const FULL_TRUST_AT: f64 = 8.0;

/// Ceiling on learned confidence. Never 1.0: this is inferred from a confounded
/// before-and-after, not from a controlled comparison.
const MAX_LEARNED_CONFIDENCE: f64 = 0.85;

#[derive(Default)]
pub struct CostModel {
    by_optimization: HashMap<String, Vec<Observation>>,
}

impl CostModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, optimization: &str, observation: Observation) {
        self.by_optimization
            .entry(optimization.to_string())
            .or_default()
            .push(observation);
    }

    pub fn observation_count(&self, optimization: &str) -> usize {
        self.by_optimization
            .get(optimization)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    pub fn observations(&self, optimization: &str) -> &[Observation] {
        self.by_optimization
            .get(optimization)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// How much the observations disagree with each other, 0 meaning perfect
    /// agreement. High spread means the change's effect depends on something
    /// the model cannot see, and confidence should not grow.
    pub fn spread(&self, optimization: &str) -> f64 {
        let obs = self.observations(optimization);
        if obs.len() < 2 {
            return 0.0;
        }
        let mean = obs.iter().map(|o| o.p50_ratio).sum::<f64>() / obs.len() as f64;
        let var = obs
            .iter()
            .map(|o| (o.p50_ratio - mean).powi(2))
            .sum::<f64>()
            / obs.len() as f64;
        var.sqrt()
    }

    /// Correct a hand-written prior with whatever has been measured.
    ///
    /// With no observations the prior is returned untouched. As they accumulate
    /// the estimate moves toward what was actually seen, and confidence rises —
    /// but only while the observations agree with each other.
    pub fn calibrate(&self, optimization: &str, prior: CostEstimate) -> CostEstimate {
        let obs = self.observations(optimization);
        if obs.is_empty() {
            return prior;
        }
        let n = obs.len() as f64;
        // Weight on the measurements, rising with count and saturating.
        let w = (n / FULL_TRUST_AT).min(1.0);

        let mean_p50 = obs.iter().map(|o| o.p50_ratio).sum::<f64>() / n;
        let mean_p99 = obs.iter().map(|o| o.p99_ratio).sum::<f64>() / n;
        let mean_bytes = obs.iter().map(|o| o.byte_delta as f64).sum::<f64>() / n;

        let blend = |prior: f64, seen: f64| prior * (1.0 - w) + seen * w;

        let mut out = prior;
        out.p50_delta = Ratio(blend(prior.p50_delta.0, mean_p50));
        out.p99_delta = Ratio(blend(prior.p99_delta.0, mean_p99));
        out.throughput_delta = Ratio(1.0 / out.p50_delta.0.max(1e-9));
        // Bytes are attributed to the change that was applied; with several
        // optimizations enabled in a window this is approximate, which is part
        // of why confidence is capped.
        out.ram_bytes = blend(prior.ram_bytes as f64, mean_bytes) as i64;

        // Confidence grows with agreement, not merely with count. Observations
        // that scatter mean the effect depends on something unmodelled, and
        // averaging them harder does not make the average more true.
        let disagreement = self.spread(optimization);
        let agreement = (1.0 - disagreement * 2.0).clamp(0.0, 1.0);
        let learned = MAX_LEARNED_CONFIDENCE * w * agreement;
        out.confidence = prior.confidence.max(learned);
        out
    }

    pub fn is_empty(&self) -> bool {
        self.by_optimization.is_empty()
    }

    /// Human-readable summary, for the decision log.
    pub fn describe(&self, optimization: &str) -> String {
        let obs = self.observations(optimization);
        if obs.is_empty() {
            return format!("{optimization}: no measurements yet");
        }
        let n = obs.len() as f64;
        let mean_p50 = obs.iter().map(|o| o.p50_ratio).sum::<f64>() / n;
        format!(
            "{optimization}: {} measurement(s), mean p50 {:+.1}%, spread {:.3}",
            obs.len(),
            (mean_p50 - 1.0) * 100.0,
            self.spread(optimization)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(p50: u64, p99: u64, bytes: u64) -> Metrics {
        Metrics {
            p50_nanos: p50,
            p99_nanos: p99,
            bytes,
            operations: 10_000,
        }
    }

    fn optimistic() -> CostEstimate {
        // The estimate that caused the M8 oscillation: claims a large win.
        CostEstimate::faster(0.3, 0.25).with_confidence(0.6)
    }

    #[test]
    fn with_no_measurements_the_prior_survives_unchanged() {
        let m = CostModel::new();
        let p = optimistic();
        let c = m.calibrate("auto_index", p);
        assert_eq!(c.p50_delta.0, p.p50_delta.0);
        assert_eq!(c.confidence, p.confidence);
    }

    #[test]
    fn measurements_pull_an_optimistic_estimate_toward_what_happened() {
        // The M8 failure, repaired: an index that measurably did nothing must
        // stop claiming it will halve latency.
        let mut m = CostModel::new();
        for _ in 0..8 {
            m.record(
                "auto_index",
                Observation::between(&metrics(1000, 5000, 0), &metrics(1000, 5000, 0)).unwrap(),
            );
        }
        let c = m.calibrate("auto_index", optimistic());
        assert!(
            (c.p50_delta.0 - 1.0).abs() < 0.05,
            "estimate still claims {:.2}x after measuring no change",
            c.p50_delta.0
        );
        assert!(!c.helps_latency(), "a measured no-op still claims to help");
    }

    #[test]
    fn a_single_measurement_barely_moves_the_prior() {
        // One before-and-after on a live system is confounded with everything
        // else that moved; it should nudge, not overrule.
        let mut m = CostModel::new();
        m.record(
            "auto_index",
            Observation::between(&metrics(1000, 5000, 0), &metrics(1000, 5000, 0)).unwrap(),
        );
        let c = m.calibrate("auto_index", optimistic());
        assert!(c.p50_delta.0 < 0.5, "one observation overruled the prior");
        assert!(c.p50_delta.0 > 0.3, "one observation had no effect at all");
    }

    #[test]
    fn confidence_rises_with_agreeing_measurements() {
        let mut m = CostModel::new();
        let before = optimistic().confidence;
        for _ in 0..8 {
            m.record(
                "cache",
                Observation::between(&metrics(1000, 5000, 0), &metrics(500, 2500, 0)).unwrap(),
            );
        }
        let c = m.calibrate("cache", optimistic());
        assert!(c.confidence > before);
        assert!(c.confidence <= MAX_LEARNED_CONFIDENCE);
    }

    #[test]
    fn confidence_does_not_rise_when_measurements_disagree() {
        // Scattered results mean the effect depends on something unmodelled.
        // Averaging harder does not make the average more true.
        let mut m = CostModel::new();
        for (a, b) in [(1000, 100), (1000, 3000), (1000, 200), (1000, 4000)] {
            m.record(
                "flaky",
                Observation::between(&metrics(a, 5000, 0), &metrics(b, 5000, 0)).unwrap(),
            );
        }
        let c = m.calibrate("flaky", optimistic());
        assert_eq!(
            c.confidence,
            optimistic().confidence,
            "confidence rose despite disagreement (spread {:.2})",
            m.spread("flaky")
        );
    }

    #[test]
    fn confidence_never_reaches_certainty() {
        let mut m = CostModel::new();
        for _ in 0..200 {
            m.record(
                "cache",
                Observation::between(&metrics(1000, 5000, 0), &metrics(500, 2500, 0)).unwrap(),
            );
        }
        let c = m.calibrate("cache", optimistic());
        assert!(
            c.confidence <= MAX_LEARNED_CONFIDENCE,
            "a confounded before-and-after claimed near-certainty"
        );
    }

    #[test]
    fn a_measured_regression_is_learned_too() {
        let mut m = CostModel::new();
        for _ in 0..8 {
            m.record(
                "bad_idea",
                Observation::between(&metrics(1000, 5000, 0), &metrics(3000, 15000, 0)).unwrap(),
            );
        }
        let c = m.calibrate("bad_idea", optimistic());
        assert!(
            c.p50_delta.0 > 1.5,
            "a measured 3x slowdown was not learned"
        );
    }

    #[test]
    fn unusable_metrics_produce_no_observation() {
        let empty = Metrics::default();
        assert!(Observation::between(&empty, &metrics(1, 1, 0)).is_none());
        assert!(Observation::between(&metrics(1, 1, 0), &empty).is_none());
    }

    #[test]
    fn byte_changes_are_learned() {
        let mut m = CostModel::new();
        for _ in 0..8 {
            m.record(
                "compression",
                Observation::between(
                    &metrics(1000, 5000, 1_000_000),
                    &metrics(1000, 5000, 400_000),
                )
                .unwrap(),
            );
        }
        let c = m.calibrate("compression", CostEstimate::neutral());
        assert!(
            c.ram_bytes < -500_000,
            "measured saving not learned: {}",
            c.ram_bytes
        );
    }

    #[test]
    fn the_model_describes_what_it_has_seen() {
        let mut m = CostModel::new();
        assert!(m.describe("x").contains("no measurements"));
        m.record(
            "x",
            Observation::between(&metrics(1000, 5000, 0), &metrics(500, 2500, 0)).unwrap(),
        );
        let d = m.describe("x");
        assert!(d.contains("1 measurement"), "{d}");
        assert!(d.contains("-50.0%"), "{d}");
    }

    #[test]
    fn calibration_is_per_optimization() {
        let mut m = CostModel::new();
        for _ in 0..8 {
            m.record(
                "good",
                Observation::between(&metrics(1000, 5000, 0), &metrics(200, 1000, 0)).unwrap(),
            );
        }
        let good = m.calibrate("good", CostEstimate::neutral());
        let other = m.calibrate("untouched", CostEstimate::neutral());
        assert!(good.helps_latency());
        assert!(
            !other.helps_latency(),
            "one optimization taught the model about another"
        );
    }
}
