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
/// Structures an experiment has built but not yet proven, hidden from the
/// planner until they are.
///
/// `column_store` and `direct` are per-collection lists rather than global
/// flags. They were flags, and that was a real defect: `Action::SetColumnStore`
/// and `Action::SetDirectLookup` are engine-wide actions, so an experiment
/// trialling a column store for one collection masked the column store for
/// *every* collection — including ones whose column store was built and
/// promoted long ago. That silently slowed unrelated queries for the duration
/// and, worse, moved the baseline those queries were being measured against
/// while an experiment elsewhere was running. Scoping the mask to the
/// collection the experiment is actually about is both the fix and the
/// precondition for ever running two experiments at once.
///
/// Every entry is tagged with the id of the experiment that built it.
/// Without that, retiring one experiment cleared the whole mask and
/// unmasked another experiment's unproven structure into live traffic —
/// which is the difference between "concurrency is unimplemented" and
/// "concurrency is unsafe".
#[derive(Debug, Default, Clone)]
pub struct Candidates {
    indexes: Vec<(u64, String, String, IndexKind)>,
    column_store: Vec<(u64, String)>,
    direct: Vec<(u64, String)>,
}

impl Candidates {
    /// Note a structure experiment `id` just built.
    ///
    /// `for_collection` is that experiment's own collection, needed because
    /// `SetColumnStore`/`SetDirectLookup` are engine-wide actions that carry
    /// no collection of their own — the experiment knows which collection it
    /// is about, the action does not.
    pub(crate) fn record(&mut self, id: u64, action: &Action, for_collection: &str) {
        match action {
            Action::CreateIndex {
                collection,
                field,
                kind,
            } => self
                .indexes
                .push((id, collection.clone(), field.clone(), *kind)),
            // Both are engine-wide actions carrying no collection of their
            // own, so the experiment's collection is what scopes them.
            Action::SetColumnStore(true) | Action::SetDirectLookup(true) => {
                if for_collection.is_empty() {
                    return;
                }
                let list = if matches!(action, Action::SetColumnStore(true)) {
                    &mut self.column_store
                } else {
                    &mut self.direct
                };
                if !list.iter().any(|(_, c)| c == for_collection) {
                    list.push((id, for_collection.to_string()));
                }
            }
            _ => {}
        }
    }

    /// Drop everything experiment `id` contributed, leaving every other
    /// experiment's mask intact.
    ///
    /// This is what makes concurrent experiments safe rather than merely
    /// possible: clearing the whole mask on retirement would expose another
    /// experiment's unproven structure to live traffic the instant an
    /// unrelated experiment finished.
    pub(crate) fn forget(&mut self, id: u64) {
        self.indexes.retain(|(e, _, _, _)| *e != id);
        self.column_store.retain(|(e, _)| *e != id);
        self.direct.retain(|(e, _)| *e != id);
    }

    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty() && self.column_store.is_empty() && self.direct.is_empty()
    }

    /// Whether anything at all is masked for experiment `id`.
    pub(crate) fn is_empty_for(&self, id: u64) -> bool {
        !self.indexes.iter().any(|(e, _, _, _)| *e == id)
            && !self.column_store.iter().any(|(e, _)| *e == id)
            && !self.direct.iter().any(|(e, _)| *e == id)
    }

    /// Whether this index is an unproven candidate that must stay hidden.
    ///
    /// `revealed` names the one experiment whose candidate the current query
    /// is allowed to see — the canary path sets it while routing a query to
    /// the candidate side. Every *other* experiment's structures stay hidden
    /// regardless.
    ///
    /// That parameter is what makes concurrent experiments safe. A single
    /// global "candidates are visible now" flag revealed every running
    /// experiment's unproven structure at once, so one experiment's canary
    /// query would be served using another experiment's untested index — and
    /// each would be measuring the other.
    pub fn hides_index(
        &self,
        revealed: Option<u64>,
        collection: &str,
        field: &str,
        kind: IndexKind,
    ) -> bool {
        self.indexes
            .iter()
            .any(|(e, c, f, k)| Some(*e) != revealed && c == collection && f == field && *k == kind)
    }

    /// Whether this collection's column store is an unproven candidate.
    pub fn hides_column_store(&self, revealed: Option<u64>, collection: &str) -> bool {
        self.column_store
            .iter()
            .any(|(e, c)| Some(*e) != revealed && c == collection)
    }

    /// Whether this collection's direct array is an unproven candidate.
    pub fn hides_direct(&self, revealed: Option<u64>, collection: &str) -> bool {
        self.direct
            .iter()
            .any(|(e, c)| Some(*e) != revealed && c == collection)
    }

    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = self
            .indexes
            .iter()
            .map(|(_, c, f, k)| format!("{} index on {c}.{f}", k.as_str()))
            .collect();
        for (_, c) in &self.column_store {
            parts.push(format!("column store on {c}"));
        }
        for (_, c) in &self.direct {
            parts.push(format!("direct lookup on {c}"));
        }
        if parts.is_empty() {
            "nothing".into()
        } else {
            parts.join(", ")
        }
    }
}

/// Whether two experiment scopes could see each other's traffic.
///
/// An experiment named by a collection takes only that collection's queries as
/// evidence; one scoped globally (the empty string) takes every query. So two
/// experiments are safe to run together exactly when their collections differ
/// and neither is global.
///
/// Overlapping scopes would put both candidates in front of the same queries,
/// and each would then be measuring the other's change as if it were noise in
/// its own. That is the reason experiments used to be limited to one at a
/// time; with per-experiment masking in place, this is the whole of what
/// remains of that restriction.
pub(crate) fn scopes_overlap(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b
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
        c.record(
            1,
            &Action::CreateIndex {
                collection: "users".into(),
                field: "country".into(),
                kind: IndexKind::Hash,
            },
            "users",
        );
        assert!(c.hides_index(None, "users", "country", IndexKind::Hash));
        assert!(
            !c.hides_index(None, "users", "country", IndexKind::BTree),
            "a hash candidate masked a pre-existing btree on the same field"
        );
        assert!(!c.hides_index(None, "orders", "country", IndexKind::Hash));
    }

    #[test]
    fn a_candidate_on_one_collection_does_not_mask_another_collections_structures() {
        // The defect this scoping fixed. `SetColumnStore` and
        // `SetDirectLookup` are engine-wide actions, so when the mask held a
        // single global flag, an experiment trialling a column store for
        // `users` hid the column store for *every* collection — including
        // `orders`, whose column store had been built and promoted long
        // before. Unrelated queries got slower for the duration, and the
        // baseline they were measured against moved while an experiment
        // somewhere else was running.
        let mut c = Candidates::default();
        c.record(1, &Action::SetColumnStore(true), "users");
        c.record(1, &Action::SetDirectLookup(true), "users");

        assert!(c.hides_column_store(None, "users"));
        assert!(c.hides_direct(None, "users"));
        assert!(
            !c.hides_column_store(None, "orders"),
            "an experiment on users masked orders' column store"
        );
        assert!(
            !c.hides_direct(None, "orders"),
            "an experiment on users masked orders' direct array"
        );
    }

    #[test]
    fn retiring_one_experiment_leaves_anothers_mask_intact() {
        // The blocker that made concurrent experiments *unsafe* rather than
        // merely unimplemented: retirement used to clear the whole mask, so
        // finishing experiment 1 would unmask experiment 2's unproven
        // structure into live traffic the instant it happened.
        let mut c = Candidates::default();
        c.record(1, &Action::SetColumnStore(true), "users");
        c.record(
            2,
            &Action::CreateIndex {
                collection: "orders".into(),
                field: "total".into(),
                kind: IndexKind::Hash,
            },
            "orders",
        );
        assert!(c.hides_column_store(None, "users"));
        assert!(c.hides_index(None, "orders", "total", IndexKind::Hash));

        c.forget(1);
        assert!(
            !c.hides_column_store(None, "users"),
            "experiment 1 was not unmasked"
        );
        assert!(
            c.hides_index(None, "orders", "total", IndexKind::Hash),
            "retiring experiment 1 exposed experiment 2's unproven index"
        );
        assert!(c.is_empty_for(1));
        assert!(!c.is_empty_for(2));
        assert!(!c.is_empty(), "the mask as a whole is not empty");
    }

    #[test]
    fn a_global_scoped_candidate_masks_nothing_by_collection() {
        // An experiment with no collection of its own has nothing to scope
        // the mask to, so it records nothing rather than masking everything —
        // the safe direction: an unmasked candidate is measured honestly as
        // part of the baseline, where masking the wrong thing would take the
        // "baseline" reading through the candidate itself.
        let mut c = Candidates::default();
        c.record(1, &Action::SetColumnStore(true), "");
        assert!(c.is_empty());
    }

    #[test]
    fn a_change_that_rewrites_the_primary_masks_nothing() {
        // Only derived builds are maskable. A compression toggle passing through
        // the sink must not leave the engine believing it has something to hide,
        // because there is no second copy to hide it from.
        let mut c = Candidates::default();
        c.record(1, &Action::SetRecordCompression(true), "users");
        c.record(1, &Action::SetBufferPoolPages(4096), "users");
        c.record(
            1,
            &Action::FreezeSchema {
                collection: "users".into(),
            },
            "users",
        );
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

#[cfg(test)]
mod scope_overlap {
    use super::scopes_overlap;

    #[test]
    fn the_same_collection_overlaps_with_itself() {
        assert!(scopes_overlap("users", "users"));
    }

    #[test]
    fn different_collections_do_not_overlap() {
        assert!(!scopes_overlap("users", "orders"));
    }

    /// A global experiment takes every query as evidence, so it overlaps with
    /// everything — in both directions, which is the part an asymmetric
    /// implementation would get wrong.
    #[test]
    fn a_global_scope_overlaps_with_everything() {
        assert!(scopes_overlap("", "users"));
        assert!(scopes_overlap("users", ""));
        assert!(scopes_overlap("", ""));
    }
}
