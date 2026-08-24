//! Driving an experiment against live traffic.
//!
//! [`adabt_opt::experiment`] holds the state machine — the phases, the
//! guardrails, the rule that one divergence is fatal. This is the half that
//! makes it real: it builds the candidate structure where the planner cannot
//! see it, routes queries at the percentage the phase names, keeps the two
//! latency populations apart, and hands the evidence back for judgement.
//!
//! # Why the candidate is hidden rather than simply absent
//!
//! An index that exists is an index the planner will use. Building one and
//! leaving it visible would end the experiment before it started: every query
//! would take the new path, there would be nothing to compare against, and the
//! first wrong answer would be served rather than caught. So the structure is
//! built, and a single flag decides whether this particular query is allowed to
//! know about it. That flag is the experiment.
//!
//! # What shadow proves and what canary proves
//!
//! They are not the same evidence and neither substitutes for the other.
//!
//! Shadow answers the same query both ways against the same state, so a
//! difference in *results* is attributable to the change and nothing else. That
//! is the only phase in which correctness can be established, and it is why no
//! traffic moves until it has passed.
//!
//! Canary sends a fraction of real queries down the new path and returns what it
//! finds. Only one path runs per query, so it cannot compare results at all —
//! its evidence is *latency under production conditions*, which shadow cannot
//! produce because running both paths back to back perturbs the cache state each
//! is being measured on.

use adabt_core::index_kind::IndexKind;
use adabt_opt::action::Action;
use adabt_opt::decision::Decision;
use adabt_opt::experiment::{Experiment, Guardrails, Measurement, Phase};
use adabt_telemetry::histogram::Histogram;

use crate::shadow::ShadowReport;

/// Structures an experiment built, and which the planner is not yet allowed to
/// use.
///
/// Recorded as the actions are carried out rather than predicted from the plan,
/// so what is hidden is exactly what was built. A prediction that drifted from
/// reality would hide the wrong thing, and hiding the wrong thing means the
/// "baseline" measurement is quietly taken through the candidate.
#[derive(Debug, Default, Clone)]
pub struct Candidates {
    indexes: Vec<(String, String, IndexKind)>,
    column_store: bool,
    direct: bool,
}

impl Candidates {
    pub(crate) fn record(&mut self, action: &Action) {
        match action {
            Action::CreateIndex {
                collection,
                field,
                kind,
            } => self
                .indexes
                .push((collection.clone(), field.clone(), *kind)),
            Action::SetColumnStore(true) => self.column_store = true,
            Action::SetDirectLookup(true) => self.direct = true,
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty() && !self.column_store && !self.direct
    }

    pub fn hides_index(&self, collection: &str, field: &str, kind: IndexKind) -> bool {
        self.indexes
            .iter()
            .any(|(c, f, k)| c == collection && f == field && *k == kind)
    }

    pub fn hides_column_store(&self) -> bool {
        self.column_store
    }

    pub fn hides_direct(&self) -> bool {
        self.direct
    }

    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = self
            .indexes
            .iter()
            .map(|(c, f, k)| format!("{} index on {c}.{f}", k.as_str()))
            .collect();
        if self.column_store {
            parts.push("column store".into());
        }
        if self.direct {
            parts.push("direct lookup".into());
        }
        if parts.is_empty() {
            "nothing".into()
        } else {
            parts.join(", ")
        }
    }
}

/// An experiment with its evidence, mid-flight.
pub struct LiveExperiment {
    pub experiment: Experiment,
    /// Only queries against this collection take part. A query elsewhere is not
    /// evidence about this change, and counting it would dilute both
    /// populations towards each other.
    pub collection: String,
    /// What the change actually built. Filled in when the experiment ends, so a
    /// finished experiment still says what it was about.
    pub candidates: Candidates,
    pub shadow: ShadowReport,
    baseline: Histogram,
    candidate: Histogram,
    baseline_errors: u64,
    candidate_errors: u64,
    /// Queries considered for routing so far. Routing is a function of this
    /// counter rather than of a random draw: an experiment that reached a
    /// different verdict on a rerun would be untestable, and a deterministic
    /// stride also spreads a 1% sample evenly instead of in clumps.
    routed: u64,
    /// Derived-representation footprint before the build, so the candidate's own
    /// cost can be measured as a difference rather than guessed at.
    ram_before: u64,
}

impl LiveExperiment {
    pub fn new(id: u64, decision: Decision, collection: String, guardrails: Guardrails) -> Self {
        Self {
            experiment: Experiment::new(id, decision, guardrails),
            collection,
            candidates: Candidates::default(),
            shadow: ShadowReport::default(),
            baseline: Histogram::new(),
            candidate: Histogram::new(),
            baseline_errors: 0,
            candidate_errors: 0,
            routed: 0,
            ram_before: 0,
        }
    }

    pub fn phase(&self) -> Phase {
        self.experiment.phase
    }

    pub(crate) fn set_ram_before(&mut self, bytes: u64) {
        self.ram_before = bytes;
    }

    /// Forget the latency evidence on entering a new measuring phase.
    ///
    /// **Each phase earns its own promotion.** Carrying samples forward would
    /// let a shadow's evidence satisfy the minimum for every canary step in
    /// turn, and the ramp — the whole mechanism for discovering that a change
    /// which looked good in a paired trial behaves differently under real
    /// traffic — would run to completion without ever measuring real traffic.
    ///
    /// Divergences are not forgotten. A wrong answer seen once does not stop
    /// being a wrong answer because the phase moved on.
    pub(crate) fn reset_measurements(&mut self) {
        self.baseline = Histogram::new();
        self.candidate = Histogram::new();
        self.baseline_errors = 0;
        self.candidate_errors = 0;
    }

    /// Whether the next query should be served by the candidate.
    ///
    /// `floor(n·p/100)` stepping: exactly `p` of every hundred, spread evenly,
    /// and identical on every rerun.
    pub(crate) fn route(&mut self, percent: u8) -> bool {
        let n = self.routed;
        self.routed += 1;
        let p = percent as u64;
        (n + 1) * p / 100 > n * p / 100
    }

    pub(crate) fn record_baseline(&mut self, nanos: u64, ok: bool) {
        self.baseline.record(nanos);
        if !ok {
            self.baseline_errors += 1;
        }
    }

    pub(crate) fn record_candidate(&mut self, nanos: u64, ok: bool) {
        self.candidate.record(nanos);
        if !ok {
            self.candidate_errors += 1;
        }
    }

    /// Fold the accumulated evidence into the form the state machine judges.
    pub(crate) fn fold(&mut self, ram_now: u64) {
        self.experiment.baseline = Measurement {
            samples: self.baseline.count(),
            p50_nanos: self.baseline.percentile(50.0),
            p99_nanos: self.baseline.percentile(99.0),
            errors: self.baseline_errors,
            ram_bytes: 0,
        };
        self.experiment.candidate = Measurement {
            samples: self.candidate.count(),
            p50_nanos: self.candidate.percentile(50.0),
            p99_nanos: self.candidate.percentile(99.0),
            errors: self.candidate_errors,
            ram_bytes: ram_now.saturating_sub(self.ram_before),
        };
        // Divergence is observable only in shadow, where both paths answered the
        // same query. It is carried forward unchanged through every later phase:
        // a wrong answer seen once does not stop being a wrong answer because
        // the phase moved on.
        self.experiment.divergences = self.shadow.divergences;
    }

    pub fn explain(&self) -> String {
        let mut s = self.experiment.explain();
        s.push_str(&format!("  candidate: {}\n", self.candidates.describe()));
        s.push_str(&format!("  shadow: {}\n", self.shadow.describe()));
        s.push_str(&format!(
            "  served: {} baseline, {} candidate\n",
            self.baseline.count(),
            self.candidate.count()
        ));
        if let Some(d) = &self.shadow.first_divergence {
            s.push_str(&format!("  first divergence: {d}\n"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_opt::decision::DecisionAction;

    fn live() -> LiveExperiment {
        LiveExperiment::new(
            1,
            Decision::new(
                "auto_index",
                "users.country",
                DecisionAction::Enable,
                "test",
            ),
            "users".into(),
            Guardrails::default(),
        )
    }

    #[test]
    fn one_percent_routing_sends_exactly_one_query_in_a_hundred() {
        let mut e = live();
        let taken = (0..1000).filter(|_| e.route(1)).count();
        assert_eq!(taken, 10);
    }

    #[test]
    fn routing_is_spread_rather_than_clumped() {
        // A random draw would sometimes put ten candidate queries in a row,
        // which measures a warmed cache rather than the change.
        let mut e = live();
        let picks: Vec<usize> = (0..1000).filter(|_| e.route(10)).collect();
        assert_eq!(picks.len(), 100);
        let gaps: Vec<usize> = picks.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.iter().all(|g| *g == 10),
            "uneven spacing: {:?}",
            &gaps[..5.min(gaps.len())]
        );
    }

    #[test]
    fn routing_is_identical_on_a_rerun() {
        let a: Vec<bool> = {
            let mut e = live();
            (0..300).map(|_| e.route(50)).collect()
        };
        let b: Vec<bool> = {
            let mut e = live();
            (0..300).map(|_| e.route(50)).collect()
        };
        assert_eq!(a, b, "the same experiment routed differently twice");
    }

    #[test]
    fn a_hundred_percent_routes_everything_and_zero_routes_nothing() {
        let mut e = live();
        assert!((0..50).all(|_| e.route(100)));
        let mut e = live();
        assert!(!(0..50).any(|_| e.route(0)));
    }

    #[test]
    fn a_mask_matches_only_the_exact_structure_it_recorded() {
        let mut c = Candidates::default();
        c.record(&Action::CreateIndex {
            collection: "users".into(),
            field: "country".into(),
            kind: IndexKind::Hash,
        });
        assert!(c.hides_index("users", "country", IndexKind::Hash));
        assert!(
            !c.hides_index("users", "country", IndexKind::BTree),
            "a hash candidate masked a pre-existing btree on the same field"
        );
        assert!(!c.hides_index("orders", "country", IndexKind::Hash));
    }

    #[test]
    fn a_change_that_rewrites_the_primary_masks_nothing() {
        // Only derived builds are maskable. A compression toggle passing through
        // the sink must not leave the engine believing it has something to hide,
        // because there is no second copy to hide it from.
        let mut c = Candidates::default();
        c.record(&Action::SetRecordCompression(true));
        c.record(&Action::SetBufferPoolPages(4096));
        c.record(&Action::FreezeSchema {
            collection: "users".into(),
        });
        assert!(c.is_empty());
    }

    #[test]
    fn folding_reports_the_candidates_own_memory_not_the_databases() {
        let mut e = live();
        e.set_ram_before(10_000_000);
        e.fold(12_500_000);
        assert_eq!(e.experiment.candidate.ram_bytes, 2_500_000);
    }

    #[test]
    fn folding_carries_shadow_divergences_into_the_verdict() {
        let mut e = live();
        e.shadow.divergences = 3;
        e.fold(0);
        assert_eq!(e.experiment.divergences, 3);
        assert!(matches!(
            e.experiment.assess(),
            adabt_opt::experiment::Assessment::Abort(_)
        ));
    }

    #[test]
    fn the_two_latency_populations_stay_separate() {
        let mut e = live();
        for _ in 0..100 {
            e.record_baseline(1_000, true);
            e.record_candidate(100, true);
        }
        e.fold(0);
        assert_eq!(e.experiment.baseline.samples, 100);
        assert_eq!(e.experiment.candidate.samples, 100);
        assert!(e.experiment.candidate.p50_nanos < e.experiment.baseline.p50_nanos / 2);
    }

    #[test]
    fn errors_are_attributed_to_the_path_that_produced_them() {
        let mut e = live();
        e.record_baseline(1_000, false);
        e.record_candidate(1_000, true);
        e.fold(0);
        assert_eq!(e.experiment.baseline.errors, 1);
        assert_eq!(
            e.experiment.candidate.errors, 0,
            "a baseline error was blamed on the candidate"
        );
    }
}
