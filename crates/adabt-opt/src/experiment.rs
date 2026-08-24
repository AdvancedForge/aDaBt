//! Online experiments: the lifecycle, the guardrails, and the verdict.
//!
//! This half is pure judgement and holds no engine state, which is what lets it
//! live in a crate that depends on nothing physical. `adabt_engine::experiment`
//! is the half that builds the candidate, hides it from the planner, routes
//! traffic at the percentage a phase names, and feeds the evidence back here.
//!
//! Nothing here decides *when* to advance. A caller does, by calling `advance`,
//! and the machine holds where it is whenever the evidence is not yet enough —
//! so advancing on a timer is safe and advancing too eagerly merely returns
//! `Inconclusive` until the samples arrive.
//!
//! One rule here is not a stub and never will be: **any result divergence
//! aborts the experiment.** Not "if it exceeds a threshold" — any. The
//! rebuildability invariant means a derived representation disagreeing with the
//! primary is always a bug, never a reconcilable difference, so tolerating even
//! one would be tolerating silent corruption.

use crate::decision::Decision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Proposed but not yet acted on.
    Proposed,
    /// The candidate structure is being built. The workload is untouched.
    Building,
    /// Both paths run; results are compared; only the old path is trusted.
    Shadow,
    /// A fraction of traffic uses the new path.
    Canary(u8),
    Promoted,
    Reverted,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Proposed => "proposed",
            Phase::Building => "building",
            Phase::Shadow => "shadow",
            Phase::Canary(_) => "canary",
            Phase::Promoted => "promoted",
            Phase::Reverted => "reverted",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Phase::Promoted | Phase::Reverted)
    }

    /// Whether queries in this phase are evidence.
    ///
    /// Distinct from `candidate_traffic`: shadow serves no traffic yet is the
    /// phase that produces the most valuable evidence there is, because both
    /// paths answer the same query and a difference is attributable to the
    /// change rather than to whatever else the workload was doing.
    pub fn is_measuring(&self) -> bool {
        matches!(self, Phase::Shadow | Phase::Canary(_))
    }

    /// Fraction of traffic served by the candidate.
    pub fn candidate_traffic(&self) -> f64 {
        match self {
            Phase::Canary(p) => *p as f64 / 100.0,
            Phase::Promoted => 1.0,
            _ => 0.0,
        }
    }
}

/// The canary ramp. Small steps early, because the first percent is where a
/// mistake is cheapest to discover.
pub const CANARY_STEPS: [u8; 5] = [1, 10, 50, 90, 100];

/// Evidence never demanded below this, however small the traffic share.
const MIN_SAMPLES_FLOOR: u64 = 30;

/// Observations before a 99th percentile means anything.
///
/// A p99 estimated from thirty samples is the largest of thirty samples wearing
/// a percentile's name. Compared against a p99 drawn from three thousand it will
/// look like a regression whatever the candidate is actually doing, because the
/// small sample is dominated by its single worst observation and the large one
/// is not.
///
/// This is not a hypothetical either. The second soak run rejected two
/// candidates for "p99 regressed 11%" and "p99 regressed 14%" whose *shadow*
/// measurements — paired, same query, same state — had them 49% and 53% faster
/// at p99. The regression was in the arithmetic, not in the database.
const P99_MIN_SAMPLES: u64 = 100;

/// Advances producing no new evidence before an experiment is abandoned.
///
/// An experiment can be perfectly healthy and still never reach a verdict, if
/// the workload simply stops exercising the thing it was proving. Since only one
/// experiment runs at a time, one that waits forever does not merely fail to
/// finish — it blocks every later change from being proved at all, and the
/// optimizer goes back to applying things outright without anybody noticing.
/// Giving up is the lesser failure, and it says so in the log.
const MAX_STALLED_ADVANCES: u32 = 250;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Measurement {
    pub samples: u64,
    pub p50_nanos: u64,
    pub p99_nanos: u64,
    pub errors: u64,
    pub ram_bytes: u64,
}

/// Limits that abort an experiment when breached.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guardrails {
    /// Largest tolerable p99 regression, as a ratio. 1.2 means 20% worse.
    pub max_p99_ratio: f64,
    pub max_ram_bytes: Option<u64>,
    /// Evidence demanded of a path carrying the whole workload. Each side's
    /// actual requirement is this scaled by the share it carries.
    pub min_samples: u64,
    /// Median regression treated as catastrophic before there is enough evidence
    /// to judge a tail. Deliberately loose: this is the early canary's job — to
    /// catch a candidate that is plainly broken, on a blast radius of one
    /// percent, without waiting for the samples a percentile would need.
    pub max_early_p50_ratio: f64,
}

impl Default for Guardrails {
    fn default() -> Self {
        Self {
            max_p99_ratio: 1.10,
            max_ram_bytes: None,
            min_samples: 1_000,
            max_early_p50_ratio: 1.50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// Not enough evidence to judge.
    Inconclusive,
    Healthy,
    /// Abort. Carries what broke.
    Abort(String),
}

#[derive(Debug, Clone)]
pub struct Experiment {
    pub id: u64,
    pub decision: Decision,
    pub phase: Phase,
    pub guardrails: Guardrails,
    pub baseline: Measurement,
    pub candidate: Measurement,
    /// Results that differed between the two paths. Must stay zero.
    pub divergences: u64,
    pub history: Vec<Phase>,
    /// Candidate samples at the previous advance, and how many advances in a row
    /// have produced none.
    last_candidate_samples: u64,
    stalled: u32,
    /// Why it ended, if it ended badly.
    ///
    /// Recorded at the moment of the abort rather than re-derived afterwards.
    /// `assess()` describes the experiment *now*, and once it has reverted the
    /// conditions that condemned it are gone — the traffic share is zero, the
    /// stall counter has been reset — so asking after the fact produces a
    /// confident "inconclusive" and the actual reason is lost.
    pub aborted_because: Option<String>,
}

impl Experiment {
    pub fn new(id: u64, decision: Decision, guardrails: Guardrails) -> Self {
        Self {
            id,
            decision,
            phase: Phase::Proposed,
            guardrails,
            baseline: Measurement::default(),
            candidate: Measurement::default(),
            divergences: 0,
            history: vec![Phase::Proposed],
            last_candidate_samples: 0,
            stalled: 0,
            aborted_because: None,
        }
    }

    /// Evidence demanded before this phase's verdict is believed.
    ///
    /// **Proportional to what is at risk.** A candidate serving one percent of
    /// traffic can harm one percent of queries, and the first canary step exists
    /// to catch catastrophe — an error, a gross regression — not to discriminate
    /// finely between two nearly equal options.
    ///
    /// Demanding the same absolute count at every step makes the cheapest and
    /// safest step the slowest to clear, because at one percent each sample
    /// costs a hundred queries. A thousand samples at `Canary(1)` is a hundred
    /// thousand queries before the ramp can take its second step — so in
    /// practice the experiment never leaves the bottom, and since only one runs
    /// at a time, nothing else is ever proved either. That is not a theoretical
    /// concern: it is what the first soak run did, and the log showed every
    /// later optimization being applied outright while experiment #1 sat at one
    /// percent forever.
    /// Evidence demanded of each side, as `(baseline, candidate)`.
    ///
    /// **Each side's requirement is proportional to the traffic it carries.**
    /// At `Canary(1)` the candidate answers one query in a hundred, so demanding
    /// a thousand of its samples means a hundred thousand queries; at
    /// `Canary(90)` the *baseline* is the rare one and the same demand is
    /// backwards in the other direction. Scaling each side by its own share
    /// makes every step of the ramp cost about the same, which is what lets a
    /// ramp finish at all.
    ///
    /// The floor is what stops proportionality reaching zero. A step that
    /// demands no evidence is a step that promotes on none.
    pub fn required_samples(&self) -> (u64, u64) {
        let candidate_share = match self.phase {
            // Shadow runs both paths on every query, so both sides carry the
            // whole workload even though the candidate serves none of it.
            Phase::Shadow => 1.0,
            p => p.candidate_traffic(),
        };
        let baseline_share = match self.phase {
            Phase::Shadow => 1.0,
            p => 1.0 - p.candidate_traffic(),
        };
        // Rounded, not truncated: `1.0 - 0.9` is 0.0999… in binary floating
        // point, and truncating turns a requirement of 100 into one of 99.
        let scale = |share: f64| {
            ((self.guardrails.min_samples as f64 * share).round() as u64).max(MIN_SAMPLES_FLOOR)
        };
        (scale(baseline_share), scale(candidate_share))
    }

    /// Whether this experiment has given up waiting for evidence.
    pub fn is_stalled(&self) -> bool {
        self.stalled >= MAX_STALLED_ADVANCES
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.history.push(phase);
        // A new phase measures afresh, so the stall counter starts afresh too.
        self.stalled = 0;
        self.last_candidate_samples = 0;
    }

    /// Judge the candidate against the baseline.
    pub fn assess(&self) -> Assessment {
        // A divergence is fatal regardless of anything else, and is checked
        // before sample counts: one wrong answer is enough, and waiting for a
        // thousand more would mean serving nine hundred and ninety-nine.
        if self.divergences > 0 {
            return Assessment::Abort(format!(
                "{} result divergence(s): the candidate returned different rows",
                self.divergences
            ));
        }
        if self.candidate.errors > 0 {
            return Assessment::Abort(format!(
                "{} errors on the candidate path",
                self.candidate.errors
            ));
        }
        if let Some(max) = self.guardrails.max_ram_bytes {
            if self.candidate.ram_bytes > max {
                return Assessment::Abort(format!(
                    "candidate used {:.1} MB against a {:.1} MB limit",
                    self.candidate.ram_bytes as f64 / 1e6,
                    max as f64 / 1e6
                ));
            }
        }
        if self.is_stalled() {
            return Assessment::Abort(format!(
                "no new evidence after {MAX_STALLED_ADVANCES} attempts: the workload \
                 stopped exercising this change before it could be judged"
            ));
        }
        let (baseline_needed, candidate_needed) = self.required_samples();
        if self.candidate.samples < candidate_needed || self.baseline.samples < baseline_needed {
            return Assessment::Inconclusive;
        }

        // Which statistic is trusted depends on how much evidence there is.
        // A tail is only compared once both sides can support one; below that
        // the median is compared instead, against a much looser bound, because
        // the early ramp is looking for catastrophe rather than discriminating
        // between two nearly equal options.
        let tail_is_meaningful =
            self.candidate.samples >= P99_MIN_SAMPLES && self.baseline.samples >= P99_MIN_SAMPLES;
        if tail_is_meaningful {
            if self.baseline.p99_nanos > 0 {
                let ratio = self.candidate.p99_nanos as f64 / self.baseline.p99_nanos as f64;
                if ratio > self.guardrails.max_p99_ratio {
                    return Assessment::Abort(format!(
                        "p99 regressed {:.0}% over {} candidate samples (limit {:.0}%)",
                        (ratio - 1.0) * 100.0,
                        self.candidate.samples,
                        (self.guardrails.max_p99_ratio - 1.0) * 100.0
                    ));
                }
            }
        } else if self.baseline.p50_nanos > 0 {
            let ratio = self.candidate.p50_nanos as f64 / self.baseline.p50_nanos as f64;
            if ratio > self.guardrails.max_early_p50_ratio {
                return Assessment::Abort(format!(
                    "p50 regressed {:.0}% over {} candidate samples (early limit {:.0}%)",
                    (ratio - 1.0) * 100.0,
                    self.candidate.samples,
                    (self.guardrails.max_early_p50_ratio - 1.0) * 100.0
                ));
            }
        }
        Assessment::Healthy
    }

    /// Advance one step, or abort. Returns the phase now in effect.
    pub fn advance(&mut self) -> Phase {
        if self.phase.is_terminal() {
            return self.phase;
        }
        // Evidence arriving is progress even when the phase does not move; an
        // experiment ramping slowly is healthy, one receiving nothing is not.
        if self.phase.is_measuring() {
            if self.candidate.samples > self.last_candidate_samples {
                self.stalled = 0;
            } else {
                self.stalled += 1;
            }
            self.last_candidate_samples = self.candidate.samples;
        }
        if let Assessment::Abort(why) = self.assess() {
            self.abort(&why);
            return self.phase;
        }
        let next = match self.phase {
            Phase::Proposed => Phase::Building,
            Phase::Building => Phase::Shadow,
            // Shadow proves correctness before any traffic moves.
            Phase::Shadow => match self.assess() {
                Assessment::Healthy => Phase::Canary(CANARY_STEPS[0]),
                _ => return self.phase,
            },
            Phase::Canary(p) => match self.assess() {
                Assessment::Healthy => match CANARY_STEPS.iter().find(|s| **s > p) {
                    Some(next) => Phase::Canary(*next),
                    None => Phase::Promoted,
                },
                // Not enough evidence yet: hold at this percentage rather than
                // ramping on a guess.
                _ => return self.phase,
            },
            terminal => terminal,
        };
        if next == Phase::Canary(100) {
            self.enter(Phase::Promoted);
        } else {
            self.enter(next);
        }
        self.phase
    }

    pub fn abort(&mut self, why: &str) {
        if !self.phase.is_terminal() {
            self.aborted_because = Some(why.to_string());
            self.enter(Phase::Reverted);
        }
    }

    /// Why this experiment ended, in one line.
    pub fn outcome(&self) -> String {
        match (&self.aborted_because, self.phase) {
            (Some(why), _) => why.clone(),
            (None, Phase::Promoted) => "promoted".into(),
            (None, p) => format!("{} ({:?})", p.as_str(), self.assess()),
        }
    }

    pub fn record_divergence(&mut self) {
        self.divergences += 1;
    }

    pub fn explain(&self) -> String {
        let path: Vec<&str> = self.history.iter().map(|p| p.as_str()).collect();
        format!(
            "experiment #{} {} [{}]\n  phase: {} ({:.0}% traffic)\n  path: {}\n  \
             outcome: {}\n  evidence: baseline {}/{}, candidate {}/{}\n",
            self.id,
            self.decision.optimization,
            self.decision.scope,
            self.phase.as_str(),
            self.phase.candidate_traffic() * 100.0,
            path.join(" -> "),
            self.outcome(),
            self.baseline.samples,
            self.required_samples().0,
            self.candidate.samples,
            self.required_samples().1,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::DecisionAction;

    fn experiment() -> Experiment {
        Experiment::new(
            1,
            Decision::new(
                "auto_index",
                "users.country",
                DecisionAction::Enable,
                "test",
            ),
            Guardrails::default(),
        )
    }

    fn healthy(e: &mut Experiment) {
        e.baseline = Measurement {
            samples: 10_000,
            p50_nanos: 1_000,
            p99_nanos: 5_000,
            errors: 0,
            ram_bytes: 0,
        };
        e.candidate = Measurement {
            samples: 10_000,
            p50_nanos: 400,
            p99_nanos: 2_000,
            errors: 0,
            ram_bytes: 0,
        };
    }

    #[test]
    fn a_healthy_experiment_ramps_to_promotion() {
        let mut e = experiment();
        healthy(&mut e);
        let mut seen = Vec::new();
        for _ in 0..12 {
            seen.push(e.advance());
            if e.phase.is_terminal() {
                break;
            }
        }
        assert_eq!(e.phase, Phase::Promoted, "path: {seen:?}");
        // It must have gone through shadow before any traffic moved.
        let shadow_at = e.history.iter().position(|p| *p == Phase::Shadow).unwrap();
        let first_canary = e
            .history
            .iter()
            .position(|p| matches!(p, Phase::Canary(_)))
            .unwrap();
        assert!(
            shadow_at < first_canary,
            "traffic moved before shadow: {:?}",
            e.history
        );
    }

    #[test]
    fn the_canary_starts_small() {
        let mut e = experiment();
        healthy(&mut e);
        while !matches!(e.phase, Phase::Canary(_)) && !e.phase.is_terminal() {
            e.advance();
        }
        assert_eq!(
            e.phase,
            Phase::Canary(1),
            "the first canary step must be small"
        );
        assert!((e.phase.candidate_traffic() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn one_divergence_aborts_regardless_of_everything_else() {
        // Not a threshold. The rebuildability invariant means a derived
        // representation disagreeing with the primary is always a bug.
        let mut e = experiment();
        healthy(&mut e);
        e.record_divergence();
        match e.assess() {
            Assessment::Abort(why) => assert!(why.contains("divergence"), "{why}"),
            other => panic!("a divergence did not abort: {other:?}"),
        }
        e.advance();
        assert_eq!(e.phase, Phase::Reverted);
    }

    #[test]
    fn a_divergence_aborts_before_enough_samples_have_accumulated() {
        // Waiting for significance would mean serving wrong answers meanwhile.
        let mut e = experiment();
        e.candidate.samples = 1;
        e.record_divergence();
        assert!(matches!(e.assess(), Assessment::Abort(_)));
    }

    #[test]
    fn a_p99_regression_beyond_the_guardrail_aborts() {
        let mut e = experiment();
        healthy(&mut e);
        e.candidate.p99_nanos = 10_000; // twice the baseline
        match e.assess() {
            Assessment::Abort(why) => assert!(why.contains("p99 regressed"), "{why}"),
            other => panic!("expected an abort, got {other:?}"),
        }
    }

    #[test]
    fn a_small_regression_within_the_guardrail_is_tolerated() {
        let mut e = experiment();
        healthy(&mut e);
        e.candidate.p99_nanos = 5_200; // 4% worse, limit is 10%
        assert_eq!(e.assess(), Assessment::Healthy);
    }

    #[test]
    fn too_few_samples_is_inconclusive_rather_than_healthy() {
        let mut e = experiment();
        healthy(&mut e);
        e.candidate.samples = 10;
        assert_eq!(e.assess(), Assessment::Inconclusive);
    }

    #[test]
    fn a_tail_is_not_compared_until_a_tail_can_be_estimated() {
        // The second soak run rejected two candidates that shadow had measured
        // as 49% and 53% *faster* at p99, because a "p99" of thirty samples is
        // the worst of thirty samples and was being compared against a real one.
        let mut e = experiment();
        e.guardrails.min_samples = 1_000;
        e.phase = Phase::Canary(1);
        e.baseline = Measurement {
            samples: 3_000,
            p50_nanos: 1_000,
            p99_nanos: 5_000,
            errors: 0,
            ram_bytes: 0,
        };
        e.candidate = Measurement {
            samples: 30,
            p50_nanos: 500,
            // Its worst of thirty, which any small sample will produce.
            p99_nanos: 5_700,
            errors: 0,
            ram_bytes: 0,
        };
        assert_eq!(
            e.assess(),
            Assessment::Healthy,
            "a candidate twice as fast at the median was rejected on a tail \
             estimated from thirty samples"
        );

        // With enough samples on both sides, the same tail regression counts.
        e.candidate.samples = 1_000;
        e.phase = Phase::Canary(90);
        e.baseline.samples = 200;
        match e.assess() {
            Assessment::Abort(why) => assert!(why.contains("p99 regressed"), "{why}"),
            other => panic!("a real tail regression was ignored: {other:?}"),
        }
    }

    #[test]
    fn a_plainly_broken_candidate_is_caught_early_without_waiting_for_a_tail() {
        // The other half. Refusing to judge a tail early must not mean refusing
        // to judge at all — an early canary exists to catch catastrophe cheaply.
        let mut e = experiment();
        e.phase = Phase::Canary(1);
        e.baseline = Measurement {
            samples: 3_000,
            p50_nanos: 1_000,
            p99_nanos: 5_000,
            errors: 0,
            ram_bytes: 0,
        };
        e.candidate = Measurement {
            samples: 30,
            p50_nanos: 4_000, // four times slower at the median
            p99_nanos: 9_000,
            errors: 0,
            ram_bytes: 0,
        };
        match e.assess() {
            Assessment::Abort(why) => assert!(why.contains("p50 regressed"), "{why}"),
            other => panic!("a four-times-slower candidate survived: {other:?}"),
        }
    }

    #[test]
    fn each_side_is_asked_for_evidence_in_proportion_to_what_it_carries() {
        // At one percent the candidate is rare; at ninety the baseline is. A
        // single requirement applied to both is backwards at one end or the
        // other, and makes that step of the ramp unreachable.
        let mut e = experiment();
        e.guardrails.min_samples = 1_000;

        e.phase = Phase::Canary(1);
        let (base, cand) = e.required_samples();
        assert_eq!((base, cand), (990, 30));

        e.phase = Phase::Canary(90);
        let (base, cand) = e.required_samples();
        assert_eq!((base, cand), (100, 900));

        e.phase = Phase::Shadow;
        assert_eq!(e.required_samples(), (1_000, 1_000));

        // Every step costs roughly one `min_samples` worth of queries, which is
        // the property that lets a ramp finish.
        for step in CANARY_STEPS {
            e.phase = Phase::Canary(step);
            let (base, cand) = e.required_samples();
            let share = step as f64 / 100.0;
            let queries_for_candidate = cand as f64 / share.max(0.01);
            let queries_for_baseline = base as f64 / (1.0 - share).max(0.01);
            let cost = queries_for_candidate.max(queries_for_baseline);
            assert!(
                cost <= 3.0 * e.guardrails.min_samples as f64,
                "step {step} costs {cost:.0} queries"
            );
        }
    }

    #[test]
    fn the_evidence_demanded_of_the_candidate_grows_with_its_share() {
        // The bug the first soak run exposed, from the candidate's side. A
        // thousand samples at one percent is a hundred thousand queries, so a
        // ramp that demands the same count at every step never leaves its first
        // one — and because only one experiment runs at a time, nothing else is
        // ever proved either.
        let mut e = experiment();
        e.guardrails.min_samples = 1_000;
        let demands: Vec<u64> = CANARY_STEPS
            .iter()
            .map(|s| {
                e.phase = Phase::Canary(*s);
                e.required_samples().1
            })
            .collect();
        assert!(
            demands.windows(2).all(|w| w[0] <= w[1]),
            "a later, riskier step demanded less of the candidate: {demands:?}"
        );
        assert_eq!(demands.first(), Some(&MIN_SAMPLES_FLOOR));
    }

    #[test]
    fn the_floor_holds_however_small_the_share() {
        // Proportionality must not reach zero: a step that demands nothing is a
        // step that promotes on no evidence at all.
        let mut e = experiment();
        e.guardrails.min_samples = 10;
        e.phase = Phase::Canary(1);
        let (base, cand) = e.required_samples();
        assert!(base >= MIN_SAMPLES_FLOOR && cand >= MIN_SAMPLES_FLOOR);
    }

    #[test]
    fn an_experiment_receiving_no_evidence_gives_up() {
        // Healthy but never judged is the worst outcome available: only one
        // experiment runs at a time, so one that waits forever silently stops
        // every later change from being proved.
        let mut e = experiment();
        healthy(&mut e);
        while !matches!(e.phase, Phase::Canary(_)) {
            e.advance();
        }
        // Evidence stops arriving short of what this step needs: the workload
        // moved on to something this change has nothing to do with.
        e.candidate.samples = 5;
        for _ in 0..MAX_STALLED_ADVANCES + 2 {
            e.advance();
        }
        assert_eq!(e.phase, Phase::Reverted, "path: {:?}", e.history);
        let text = e.explain();
        assert!(text.contains("no new evidence"), "{text}");
    }

    #[test]
    fn evidence_still_arriving_is_not_a_stall() {
        // A slow ramp is healthy. Only the absence of new samples is a stall.
        let mut e = experiment();
        healthy(&mut e);
        while !matches!(e.phase, Phase::Canary(_)) {
            e.advance();
        }
        for i in 0..MAX_STALLED_ADVANCES * 2 {
            e.candidate.samples = 1 + i as u64;
            e.advance();
            if e.phase.is_terminal() {
                break;
            }
        }
        assert_ne!(
            e.phase,
            Phase::Reverted,
            "an experiment making progress was abandoned"
        );
    }

    #[test]
    fn an_inconclusive_canary_holds_instead_of_ramping() {
        let mut e = experiment();
        healthy(&mut e);
        while !matches!(e.phase, Phase::Canary(_)) {
            e.advance();
        }
        let held = e.phase;
        e.candidate.samples = 1; // evidence evaporates
        assert_eq!(e.advance(), held, "it ramped on insufficient evidence");
    }

    #[test]
    fn errors_on_the_candidate_path_abort() {
        let mut e = experiment();
        healthy(&mut e);
        e.candidate.errors = 1;
        assert!(matches!(e.assess(), Assessment::Abort(_)));
    }

    #[test]
    fn a_ram_guardrail_aborts_when_breached() {
        let mut e = experiment();
        healthy(&mut e);
        e.guardrails.max_ram_bytes = Some(1_000_000);
        e.candidate.ram_bytes = 2_000_000;
        match e.assess() {
            Assessment::Abort(why) => assert!(why.contains("MB"), "{why}"),
            other => panic!("expected an abort, got {other:?}"),
        }
    }

    #[test]
    fn a_terminal_experiment_does_not_move() {
        let mut e = experiment();
        e.abort("test");
        assert_eq!(e.phase, Phase::Reverted);
        let before = e.history.len();
        e.advance();
        assert_eq!(e.phase, Phase::Reverted);
        assert_eq!(
            e.history.len(),
            before,
            "a terminal experiment recorded a transition"
        );
    }

    #[test]
    fn traffic_share_matches_the_phase() {
        assert_eq!(Phase::Proposed.candidate_traffic(), 0.0);
        assert_eq!(
            Phase::Shadow.candidate_traffic(),
            0.0,
            "shadow must not serve traffic"
        );
        assert_eq!(Phase::Canary(50).candidate_traffic(), 0.5);
        assert_eq!(Phase::Promoted.candidate_traffic(), 1.0);
        assert_eq!(Phase::Reverted.candidate_traffic(), 0.0);
    }

    #[test]
    fn an_experiment_explains_its_path() {
        let mut e = experiment();
        healthy(&mut e);
        for _ in 0..10 {
            e.advance();
        }
        let text = e.explain();
        assert!(text.contains("auto_index"), "{text}");
        assert!(text.contains("shadow"), "{text}");
        assert!(text.contains("->"), "{text}");
    }
}
