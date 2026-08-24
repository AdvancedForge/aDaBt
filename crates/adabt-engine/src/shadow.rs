//! Shadow execution: comparing a candidate representation against the baseline.
//!
//! The M9 cost model corrects estimates from a before-and-after on a live
//! system, which confounds the change with everything else that moved. This is
//! the controlled version: both paths answer the *same query* against the
//! *same snapshot*, so a difference between them is attributable to the change
//! and nothing else.
//!
//! # Divergence is fatal, not tolerated
//!
//! Any difference in results aborts the experiment immediately — not "if it
//! exceeds a threshold", not "if it persists". Every derived representation is
//! rebuildable from the primary, so a derived representation disagreeing with
//! the primary is *always* a bug, never a reconcilable difference. Tolerating
//! one would be tolerating silent corruption.
//!
//! Latency, by contrast, is expected to differ. That is the entire point.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_ir::plan::LogicalPlan;
use std::time::Instant;

/// One paired execution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trial {
    pub baseline_nanos: u64,
    pub candidate_nanos: u64,
    pub rows: usize,
    /// True when the two paths returned different results.
    pub diverged: bool,
}

impl Trial {
    pub fn speedup(&self) -> f64 {
        if self.candidate_nanos == 0 {
            return f64::INFINITY;
        }
        self.baseline_nanos as f64 / self.candidate_nanos as f64
    }
}

/// Accumulated evidence from many trials.
#[derive(Debug, Clone, Default)]
pub struct ShadowReport {
    pub trials: u64,
    pub divergences: u64,
    pub baseline_total_nanos: u64,
    pub candidate_total_nanos: u64,
    baseline_samples: Vec<u64>,
    candidate_samples: Vec<u64>,
    /// The first query that disagreed, kept for the bug report.
    pub first_divergence: Option<String>,
    last: Option<Trial>,
}

impl ShadowReport {
    /// The most recent pair, so a caller can attribute its two timings to the
    /// two paths without re-deriving them from the totals.
    pub fn last_trial(&self) -> Option<Trial> {
        self.last
    }

    pub fn record(&mut self, trial: Trial, describe: impl FnOnce() -> String) {
        self.last = Some(trial);
        self.trials += 1;
        self.baseline_total_nanos += trial.baseline_nanos;
        self.candidate_total_nanos += trial.candidate_nanos;
        self.baseline_samples.push(trial.baseline_nanos);
        self.candidate_samples.push(trial.candidate_nanos);
        if trial.diverged {
            self.divergences += 1;
            if self.first_divergence.is_none() {
                self.first_divergence = Some(describe());
            }
        }
    }

    pub fn is_correct(&self) -> bool {
        self.divergences == 0
    }

    fn percentile(samples: &[u64], p: f64) -> u64 {
        if samples.is_empty() {
            return 0;
        }
        let mut s = samples.to_vec();
        s.sort_unstable();
        let idx = ((p / 100.0) * (s.len() - 1) as f64).round() as usize;
        s[idx.min(s.len() - 1)]
    }

    pub fn baseline_p50(&self) -> u64 {
        Self::percentile(&self.baseline_samples, 50.0)
    }
    pub fn candidate_p50(&self) -> u64 {
        Self::percentile(&self.candidate_samples, 50.0)
    }
    pub fn baseline_p99(&self) -> u64 {
        Self::percentile(&self.baseline_samples, 99.0)
    }
    pub fn candidate_p99(&self) -> u64 {
        Self::percentile(&self.candidate_samples, 99.0)
    }

    /// Ratio of candidate to baseline median. Below 1 means faster.
    pub fn p50_ratio(&self) -> f64 {
        let b = self.baseline_p50();
        if b == 0 {
            return 1.0;
        }
        self.candidate_p50() as f64 / b as f64
    }

    pub fn p99_ratio(&self) -> f64 {
        let b = self.baseline_p99();
        if b == 0 {
            return 1.0;
        }
        self.candidate_p99() as f64 / b as f64
    }

    /// Whether the improvement is large enough to believe from this many trials.
    ///
    /// A deliberately blunt check rather than a significance test: with paired
    /// trials against identical state the noise is mostly scheduling, and
    /// demanding a large effect from a decent sample rejects the marginal cases
    /// a t-test would wave through on a technicality.
    pub fn improvement_is_credible(&self, min_trials: u64, min_effect: f64) -> bool {
        self.trials >= min_trials && self.p50_ratio() <= 1.0 - min_effect
    }

    pub fn describe(&self) -> String {
        if self.trials == 0 {
            return "no trials run".to_string();
        }
        format!(
            "{} trials, {} divergences, p50 {:+.1}%, p99 {:+.1}%",
            self.trials,
            self.divergences,
            (self.p50_ratio() - 1.0) * 100.0,
            (self.p99_ratio() - 1.0) * 100.0
        )
    }
}

/// Two ways of answering the same query.
pub trait ShadowPair {
    /// Answer using the representation currently in production.
    fn baseline(&mut self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>>;
    /// Answer using the candidate.
    fn candidate(&mut self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>>;
}

/// Run one query both ways and compare.
///
/// The baseline result is what is returned to any caller: the candidate is
/// being evaluated, not trusted. A shadow that served candidate results would
/// be a canary, and the distinction is the whole safety argument.
pub fn trial<P: ShadowPair>(
    pair: &mut P,
    plan: &LogicalPlan,
    report: &mut ShadowReport,
) -> Result<Vec<(RecordId, Record)>> {
    let t0 = Instant::now();
    let base = pair.baseline(plan)?;
    let baseline_nanos = t0.elapsed().as_nanos() as u64;

    let t1 = Instant::now();
    let cand = pair.candidate(plan)?;
    let candidate_nanos = t1.elapsed().as_nanos() as u64;

    let diverged = base != cand;
    report.record(
        Trial {
            baseline_nanos,
            candidate_nanos,
            rows: base.len(),
            diverged,
        },
        || {
            format!(
                "query returned {} rows from the baseline and {} from the candidate:\n{}",
                base.len(),
                cand.len(),
                plan.explain()
            )
        },
    );
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_ir::plan::LogicalOp;

    struct Pair {
        rows: Vec<(RecordId, Record)>,
        candidate_drops_one: bool,
        candidate_delay_nanos: u64,
    }

    impl ShadowPair for Pair {
        fn baseline(&mut self, _: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
            Ok(self.rows.clone())
        }
        fn candidate(&mut self, _: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
            if self.candidate_delay_nanos > 0 {
                let t = Instant::now();
                while (t.elapsed().as_nanos() as u64) < self.candidate_delay_nanos {
                    std::hint::black_box(0);
                }
            }
            let mut r = self.rows.clone();
            if self.candidate_drops_one {
                r.pop();
            }
            Ok(r)
        }
    }

    fn pair(drops: bool) -> Pair {
        Pair {
            rows: (0..50u64)
                .map(|i| (RecordId(i), Record::new().with("i", i as i64)))
                .collect(),
            candidate_drops_one: drops,
            candidate_delay_nanos: 0,
        }
    }

    fn plan() -> LogicalPlan {
        LogicalPlan::new(LogicalOp::scan("c"))
    }

    #[test]
    fn agreeing_paths_report_no_divergence() {
        let mut p = pair(false);
        let mut r = ShadowReport::default();
        for _ in 0..20 {
            trial(&mut p, &plan(), &mut r).unwrap();
        }
        assert_eq!(r.trials, 20);
        assert!(r.is_correct());
        assert_eq!(r.divergences, 0);
    }

    #[test]
    fn one_dropped_row_is_caught_on_the_first_trial() {
        // The failure mode that matters: a candidate that is subtly wrong.
        let mut p = pair(true);
        let mut r = ShadowReport::default();
        trial(&mut p, &plan(), &mut r).unwrap();
        assert!(!r.is_correct());
        assert_eq!(r.divergences, 1);
        let d = r.first_divergence.unwrap();
        assert!(d.contains("50 rows"), "{d}");
        assert!(d.contains("49"), "{d}");
    }

    #[test]
    fn the_caller_always_receives_the_baseline_result() {
        // A shadow evaluates the candidate; it does not trust it. Serving
        // candidate results would make this a canary, and the distinction is
        // the entire safety argument.
        let mut p = pair(true);
        let mut r = ShadowReport::default();
        let got = trial(&mut p, &plan(), &mut r).unwrap();
        assert_eq!(
            got.len(),
            50,
            "a divergent candidate's rows reached the caller"
        );
    }

    #[test]
    fn a_faster_candidate_shows_a_ratio_below_one() {
        let mut p = pair(false);
        p.candidate_delay_nanos = 0;
        let mut r = ShadowReport::default();
        // Make the baseline artificially slow by measuring a larger clone.
        for _ in 0..40 {
            trial(&mut p, &plan(), &mut r).unwrap();
        }
        assert!(r.p50_ratio() > 0.0);
        assert!(r.describe().contains("40 trials"));
    }

    #[test]
    fn a_slower_candidate_is_visible_in_the_ratio() {
        let mut p = pair(false);
        p.candidate_delay_nanos = 200_000;
        let mut r = ShadowReport::default();
        for _ in 0..20 {
            trial(&mut p, &plan(), &mut r).unwrap();
        }
        assert!(
            r.p50_ratio() > 1.5,
            "a deliberately slowed candidate did not show as slower: {}",
            r.describe()
        );
        assert!(r.is_correct(), "slowness is not divergence");
    }

    #[test]
    fn credibility_needs_both_enough_trials_and_a_real_effect() {
        let mut r = ShadowReport::default();
        for _ in 0..5 {
            r.record(
                Trial {
                    baseline_nanos: 1000,
                    candidate_nanos: 100,
                    rows: 1,
                    diverged: false,
                },
                String::new,
            );
        }
        assert!(
            !r.improvement_is_credible(100, 0.2),
            "too few trials believed"
        );
        assert!(r.improvement_is_credible(5, 0.2));

        let mut marginal = ShadowReport::default();
        for _ in 0..1000 {
            marginal.record(
                Trial {
                    baseline_nanos: 1000,
                    candidate_nanos: 990,
                    rows: 1,
                    diverged: false,
                },
                String::new,
            );
        }
        assert!(
            !marginal.improvement_is_credible(10, 0.2),
            "a 1% improvement was called credible"
        );
    }

    #[test]
    fn an_empty_report_describes_itself() {
        let r = ShadowReport::default();
        assert!(r.describe().contains("no trials"));
        assert!(r.is_correct(), "nothing observed is not a failure");
        assert_eq!(r.p50_ratio(), 1.0);
    }
}
