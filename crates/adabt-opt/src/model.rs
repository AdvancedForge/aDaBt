//! A cost model calibrated from observed outcomes.
//!
//! An earlier driver argued with itself: it dropped an index on
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
//! the same state at the same time, which is shadow execution. Without it this
//! is a bias correction, not an experiment, and it is deliberately weighted as
//! one.

use std::collections::HashMap;

use crate::cost::{CostEstimate, Ratio};

/// Snapshot of what an optimization was supposed to affect.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metrics {
    /// The query shape the latencies are for.
    ///
    /// Recorded so a before and an after are only compared when they measure
    /// the same query. Without it a change applied as the workload moved on
    /// was credited, or blamed, for the difference between two unrelated
    /// queries: on the soak, whatever was enabled just before a phase ended
    /// was judged by the next phase's query against the last one's.
    pub shape: u64,
    pub p50_nanos: u64,
    pub p99_nanos: u64,
    pub bytes: u64,
    pub operations: u64,
}

/// Calls of the measured shape below which its percentiles are not a
/// measurement.
///
/// A p99 needs a hundred samples to exist at all, and a p50 from a handful is
/// whichever query happened to run. The soak's first baseline was a single
/// 20 us call, which then recorded every change applied beside it as an 80x
/// speedup.
const MIN_MEASURED_CALLS: u64 = 100;

impl Metrics {
    /// Whether there is enough here to compare against anything.
    pub fn is_usable(&self) -> bool {
        self.operations >= MIN_MEASURED_CALLS && self.p50_nanos > 0
    }
}

/// One before-and-after pair for one optimization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub p50_ratio: f64,
    pub p99_ratio: f64,
    pub byte_delta: i64,
    /// The p50 and p99 ratios the uncalibrated estimate predicted when the
    /// change was made, if known. See [`CostModel::calibrate`] for why it
    /// matters.
    pub predicted: Option<(f64, f64)>,
}

impl Observation {
    pub fn between(before: &Metrics, after: &Metrics) -> Option<Observation> {
        if !before.is_usable() || !after.is_usable() || before.shape != after.shape {
            return None;
        }
        Some(Observation {
            p50_ratio: after.p50_nanos as f64 / before.p50_nanos as f64,
            p99_ratio: after.p99_nanos as f64 / before.p99_nanos as f64,
            byte_delta: after.bytes as i64 - before.bytes as i64,
            predicted: None,
        })
    }

    /// Record what the estimate predicted for this change.
    pub fn predicting(mut self, prior: &CostEstimate) -> Self {
        self.predicted = Some((prior.p50_delta.0, prior.p99_delta.0));
        self
    }

    /// The latency ratios this observation implies for a change whose
    /// uncalibrated estimate is now `prior`.
    ///
    /// With no prediction on record, what was measured. With one, the
    /// measurement is read as a correction *to the prediction*: the gain the
    /// estimate claims now, scaled by how the measured gain compared with
    /// the gain it claimed then. That keeps a measurement taken on one
    /// workload from claiming a gain on another: an estimate grounded in the
    /// traffic it helps claims nothing once that traffic has gone, and nothing
    /// scaled is still nothing.
    fn implied(&self, prior: &CostEstimate) -> (f64, f64) {
        let Some((then_p50, then_p99)) = self.predicted else {
            return (self.p50_ratio, self.p99_ratio);
        };
        let scale = |now: f64, then: f64, seen: f64| {
            let claimed = 1.0 - then;
            if claimed.abs() < MIN_CLAIMED_GAIN {
                return seen;
            }
            let factor = ((1.0 - seen) / claimed).clamp(-MAX_FACTOR, MAX_FACTOR);
            (1.0 - (1.0 - now) * factor).max(0.0)
        };
        (
            scale(prior.p50_delta.0, then_p50, self.p50_ratio),
            scale(prior.p99_delta.0, then_p99, self.p99_ratio),
        )
    }
}

/// A predicted gain below which a measurement cannot be read as a correction
/// to it: a factor over a claim of nothing is noise divided by nothing.
const MIN_CLAIMED_GAIN: f64 = 0.01;

/// Bound on how far one measurement can scale a prediction.
///
/// Observations are confounded with whatever else changed in their window,
/// and an unbounded factor lets one of them dominate: a 2% claim measured
/// beside a change that halved latency would read as a factor of 25. Five
/// lets a measurement say "it was several times better or worse than
/// claimed", which is as much as one confounded before-and-after can say.
const MAX_FACTOR: f64 = 5.0;

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
    ///
    /// An observation that records what was predicted corrects the
    /// *prediction* rather than replacing it (see `Observation::implied`).
    /// Without that, a gain measured while a column store's scans were running
    /// went on being claimed after the scans stopped: the soak's last phase,
    /// pure identity lookups, kept a column store because one measurement from
    /// the aggregate phase said it was worth 12% there too.
    pub fn calibrate(&self, optimization: &str, prior: CostEstimate) -> CostEstimate {
        let obs = self.observations(optimization);
        if obs.is_empty() {
            return prior;
        }
        let n = obs.len() as f64;
        // Weight on the measurements, rising with count and saturating.
        let w = (n / FULL_TRUST_AT).min(1.0);

        let implied: Vec<(f64, f64)> = obs.iter().map(|o| o.implied(&prior)).collect();
        let mean_p50 = implied.iter().map(|(p50, _)| p50).sum::<f64>() / n;
        let mean_p99 = implied.iter().map(|(_, p99)| p99).sum::<f64>() / n;
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
            shape: 1,
            p50_nanos: p50,
            p99_nanos: p99,
            bytes,
            operations: 10_000,
        }
    }

    /// Eight measurements of a change predicted to halve latency that
    /// measured `seen` instead.
    fn measured_against_a_halving(seen: f64) -> CostModel {
        let predicted = CostEstimate::faster(0.5, 0.5);
        let mut m = CostModel::new();
        for _ in 0..8 {
            let after = (1000.0 * seen) as u64;
            m.record(
                "scan_helper",
                Observation::between(&metrics(1000, 1000, 0), &metrics(after, after, 0))
                    .unwrap()
                    .predicting(&predicted),
            );
        }
        m
    }

    #[test]
    fn a_measured_gain_does_not_outlive_the_traffic_it_was_measured_on() {
        // Measured as predicted on a scan-heavy workload; the estimate now
        // claims nothing, because the scans have stopped. The measurement
        // must not put the gain back.
        let m = measured_against_a_halving(0.5);
        let c = m.calibrate("scan_helper", CostEstimate::neutral());
        assert!(!c.helps_latency(), "{c:?}");
    }

    #[test]
    fn a_prediction_is_corrected_in_proportion() {
        // Predicted a halving, measured a quarter off: half the claimed gain.
        // A claim of 40% now should be calibrated to 20%.
        let m = measured_against_a_halving(0.75);
        let c = m.calibrate("scan_helper", CostEstimate::faster(0.6, 0.6));
        assert!((c.p50_delta.0 - 0.8).abs() < 1e-9, "{c:?}");
    }

    #[test]
    fn different_queries_are_not_compared() {
        // A change applied as the workload moved on must not be credited
        // with the difference between the old query and the new one.
        let before = metrics(1_000, 5_000, 0);
        let after = Metrics {
            shape: 2,
            ..metrics(100, 500, 0)
        };
        assert_eq!(Observation::between(&before, &after), None);
    }

    #[test]
    fn a_handful_of_calls_is_not_a_measurement() {
        let thin = Metrics {
            operations: MIN_MEASURED_CALLS - 1,
            ..metrics(20_000, 20_000, 0)
        };
        assert_eq!(Observation::between(&thin, &metrics(250, 800, 0)), None);
    }

    fn optimistic() -> CostEstimate {
        // The estimate that caused the add/drop oscillation: claims a large
        // win.
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
        // The oscillation, repaired: an index that measurably did nothing must
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
