//! The adaptive driver.
//!
//! Observes, proposes, and — the part that matters — *retracts*. An optimizer
//! that only ever adds is a ratchet, and the level-matrix benchmark showed what
//! that costs: a level built an index the planner never chose, paying memory
//! and write maintenance for it forever, and no threshold on "was this field
//! filtered" could have noticed.
//!
//! # Evidence, estimate, and the argument between them
//!
//! An earlier driver contradicted itself: it dropped an unused index on
//! evidence, then re-added it on an estimate claiming a 70% win, forever. Two
//! mechanisms settle that now, in order of preference:
//!
//! 1. A **calibrated model** corrects the estimate from what was measured, so a
//!    refuted prior stops arguing for itself. This is the actual repair.
//! 2. A **long retraction cooldown** as a backstop, for the window before
//!    enough measurements exist to correct anything.
//!
//! Evidence outranks estimate throughout: one is a guess about what would
//! happen, the other a record of what did.
//!
//! # Stability
//!
//! An optimizer that oscillates is worse than none — every flip pays a build
//! cost and invalidates caches. A minimum observation window, per-scope
//! cooldowns, and a cap on changes per cycle keep it settled.

use adabt_core::policy::Policy;
use adabt_telemetry::{Histogram, QueryShape, Snapshot};
use std::collections::HashMap;

use crate::config::OptimizationConfig;
use crate::cost::CostEstimate;
use crate::decision::{Decision, DecisionAction, Source};
use crate::driver::{DriverInput, OptimizationDriver};
use crate::memory::{Fingerprint, WorkloadMemory};
use crate::model::{CostModel, Metrics, Observation};
use crate::optimization::{permitted_by, OptContext, Reversibility};
use crate::registry::Registry;
use crate::score::score_against;
use crate::search::{best_combination, Candidate};

/// Operations that must have been observed before the driver acts at all.
///
/// **A recent count, not a lifetime one.** Telemetry decays by
/// [`TELEMETRY_DECAY`] each cycle, so a steadily busy database settles at
/// roughly `ops_per_cycle / (1 - 3/4)` — four times its per-cycle rate — and
/// this threshold therefore asks for about 125 operations per cycle sustained
/// rather than 500 operations ever.
///
/// That is the intended reading: an optimizer should act on a database that is
/// busy *now*, not on one that was busy once. It is worth stating because the
/// constant did not change when its meaning did.
const MIN_OBSERVATIONS: u64 = 500;

/// Cycles a scope is left alone after it is changed.
const COOLDOWN_CYCLES: u64 = 3;

/// Cycles before something retracted *for lack of use* may be proposed again.
///
/// A backstop for the window before the model has enough measurements to
/// correct the estimate that would otherwise re-propose it.
const RETRACTION_COOLDOWN_CYCLES: u64 = 50;

/// Changes proposed per cycle. Small so each one's effect stays separable in
/// the decision log rather than tangled with four others.
const MAX_CHANGES_PER_CYCLE: usize = 2;

/// Score below which a candidate is not worth applying.
///
/// Not zero: a change scoring barely positive is inside the noise of estimates
/// this rough, and applying it costs a rebuild either way.
const MIN_SCORE: f64 = 0.5;

/// Score below which an *already enabled* change is retracted.
///
/// **A change has to go on justifying itself by the standard that admitted it.**
/// Retraction used to require a score below `-MIN_SCORE` — actively harmful, by
/// a wide margin — while admission required one above `+MIN_SCORE`. That leaves a
/// dead band between them in which a change can never be reconsidered, and
/// calibration walks changes straight into it: correcting an optimistic prior
/// pulls a score down toward zero, not below it, so a change admitted on a
/// 40%-confidence guess settles at 0.2 and stays forever.
///
/// That is not hypothetical. `docs/history/diagnosis.md` records a standing measurement
/// of it: across every soak run the aggregate phase fails to improve, because
/// `column_store` is applied on a 40%-confidence prior, scores 0.54, and is
/// never re-examined.
///
/// The bar to keep is therefore the bar to admit, less a hysteresis margin. The
/// margin is what stops the two rules arguing with each other — the failure
/// mode an earlier driver already hit once, where the driver dropped a thing on
/// evidence and re-added it on estimate forever.
///
/// This is the cheap half of the fix. The complete answer is to *re-prove* an
/// applied change by turning it off in shadow and measuring, which needs the
/// experiment loop to cover changes that are not derived-representation
/// additions. Until then, this at least lets a corrected prior have consequences.
const KEEP_SCORE: f64 = MIN_SCORE / 2.0;

/// Baseline index writes per planner use above which an index is a net loss,
/// before the policy's own priorities scale it.
///
/// Deliberately generous. An index legitimately costs several writes per read
/// on a write-heavy collection and still earns its keep by turning a full scan
/// into a lookup, and the cost of retracting one that was actually useful is a
/// full rebuild. This is a floor for "obviously losing", not a tuning knob for
/// "optimal" — the same posture `MIN_SCORE` takes.
const MIN_WRITES_PER_USE: f64 = 50.0;

/// How much of what telemetry has counted survives each optimization cycle.
///
/// Three quarters. A structure that stops being used has its record fall to
/// nothing in roughly thirty cycles, which is long enough that a brief lull in a
/// workload does not cost an index and short enough that a workload which has
/// genuinely moved on stops paying for the last one. It is a forgetting rate,
/// and like every forgetting rate it is a guess about how fast the world
/// changes; it is stated here in one place so it can be argued with.
pub const TELEMETRY_DECAY: (u64, u64) = (3, 4);

/// Cycles to wait before judging a change.
///
/// Long enough for the workload to exercise the new configuration, short enough
/// that the measurement is not swamped by everything else that moved.
const OBSERVE_AFTER_CYCLES: u64 = 3;

/// Cycles with no change proposed before the current configuration is
/// recorded as what this workload shape settled on.
///
/// Two, not one: a single quiet cycle happens routinely while a change is
/// waiting to be measured, and remembering then would record a
/// configuration the driver has not finished judging.
const STABLE_CYCLES_BEFORE_REMEMBERING: u32 = 2;

/// Identity of one `(optimization, scope)` pair, for matching a search
/// result back to the decision it came from. A NUL separator cannot occur in
/// either half, so the join is unambiguous.
fn scope_key(name: &str, scope: &str) -> String {
    format!("{name}\u{0}{scope}")
}

/// A change waiting to be measured.
struct Pending {
    optimization: String,
    baseline: Metrics,
    /// The measured shape's latency histogram when the change was made.
    ///
    /// Latency histograms are never decayed, so a shape that has run for an
    /// hour carries an hour of samples, and its percentiles three cycles after
    /// a change barely move whatever the change did. The judgment is taken
    /// over the samples recorded since this copy instead.
    baseline_latency: Histogram,
    applied_at: u64,
    /// Whether an experiment ran while this change was waiting to be judged.
    ///
    /// Queries on an experiment's collection are routed around the caches
    /// and see the candidate's structures, so what they measure is the
    /// experiment, not this change. On the soak a result cache enabled beside
    /// an index experiment served almost nothing for its whole window and was
    /// recorded as making the median 31% *slower*.
    confounded: bool,
    /// The uncalibrated estimate the change was admitted on, so the
    /// measurement can be read as a correction to it.
    prior: CostEstimate,
}

pub struct AdaptiveDriver {
    cycles: u64,
    /// Cycle at which each `(optimization, scope)` was last changed.
    last_changed: HashMap<(String, String), u64>,
    /// Cycle at which each scope was retracted for demonstrably not being used,
    /// as opposed to merely scoring badly.
    retracted: HashMap<(String, String), u64>,
    /// Priors corrected by what was actually measured.
    model: CostModel,
    /// Changes applied but not yet measured.
    pending: Vec<Pending>,
    /// Configurations that worked for workload shapes seen before.
    ///
    /// Consulted to decide what to *try first*, never to bypass a bar — see
    /// `crate::memory` for why recall is a hypothesis rather than evidence.
    memory: WorkloadMemory,
    /// Cycles in a row that ended with no change proposed. A configuration
    /// is only worth remembering once it has stopped moving; remembering a
    /// half-converged one would teach the memory a waypoint as a
    /// destination.
    stable_cycles: u32,
    /// Decisions emitted, for tests and the decision log.
    pub proposals: u64,
}

impl Default for AdaptiveDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveDriver {
    pub fn new() -> Self {
        Self {
            cycles: 0,
            last_changed: HashMap::new(),
            retracted: HashMap::new(),
            model: CostModel::new(),
            pending: Vec::new(),
            memory: WorkloadMemory::new(),
            stable_cycles: 0,
            proposals: 0,
        }
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// Start with priors already corrected by earlier measurement.
    ///
    /// A driver that forgets everything it measured whenever the process
    /// restarts relearns the same corrections from scratch, and relearning is
    /// not free: it costs another round of applying changes that were already
    /// known not to work. Nothing persists the model yet; this is the seam that
    /// persistence would plug into, and it is what lets a test state "given
    /// this has been measured N times" without simulating N cycles.
    pub fn with_model(model: CostModel) -> Self {
        Self {
            model,
            ..Self::new()
        }
    }

    /// What the model has learned, for the decision log.
    pub fn model(&self) -> &CostModel {
        &self.model
    }

    pub fn pending_measurements(&self) -> usize {
        self.pending.len()
    }

    fn in_cooldown(&self, name: &str, scope: &str) -> bool {
        self.last_changed
            .get(&(name.to_string(), scope.to_string()))
            .is_some_and(|c| self.cycles < c + COOLDOWN_CYCLES)
    }

    fn recently_retracted(&self, name: &str, scope: &str) -> bool {
        self.retracted
            .get(&(name.to_string(), scope.to_string()))
            .is_some_and(|c| self.cycles < c + RETRACTION_COOLDOWN_CYCLES)
    }

    fn mark_changed(&mut self, name: &str, scope: &str) {
        self.last_changed
            .insert((name.to_string(), scope.to_string()), self.cycles);
    }

    /// Fold any change that has had time to show an effect into the model.
    ///
    /// A change is judged on its own query's samples since it was made, and
    /// not at all if an experiment overlapped the wait; see [`Pending`].
    fn learn(&mut self, telemetry: &Snapshot, bytes: u64, experimenting: bool) {
        let cycle = self.cycles;
        let mut still_pending = Vec::new();
        for mut p in std::mem::take(&mut self.pending) {
            p.confounded |= experimenting;
            if cycle < p.applied_at + OBSERVE_AFTER_CYCLES {
                still_pending.push(p);
                continue;
            }
            if p.confounded {
                continue;
            }
            let Some(now) = metrics_since(telemetry, &p.baseline, &p.baseline_latency, bytes)
            else {
                continue;
            };
            if let Some(obs) =
                Observation::between(&p.baseline, &now).map(|o| o.predicting(&p.prior))
            {
                self.model.record(&p.optimization, obs);
            }
        }
        self.pending = still_pending;
    }

    /// Scopes worth turning on, best first.
    fn additions(
        &self,
        registry: &Registry,
        current: &OptimizationConfig,
        policy: &Policy,
        ctx: &OptContext<'_>,
    ) -> Vec<(Decision, f64)> {
        let mut out = Vec::new();
        for opt in registry.iter() {
            let meta = opt.meta();
            // Guarantees are the controller's filter, but proposing something
            // the policy forbids would fill the log with noise the user cannot
            // act on.
            if !permitted_by(meta, policy) {
                continue;
            }
            if !opt.applicability(ctx).is_applicable() {
                continue;
            }
            // An irreversible change is never made automatically. Every safety
            // mechanism here — measurement, retraction, shadow comparison —
            // assumes a bad decision can be taken back. Where it cannot, the
            // decision belongs to a human who can weigh what is being given up.
            if meta.reversibility == Reversibility::Destructive {
                continue;
            }
            // One decision per scope, so each is judged, applied and retracted
            // on its own evidence rather than standing or falling with the rest.
            for scope in opt.candidate_scopes(ctx) {
                if current.is_enabled(meta.name, &scope)
                    || self.in_cooldown(meta.name, &scope)
                    || self.recently_retracted(meta.name, &scope)
                {
                    continue;
                }
                // Calibrated, so an estimate that measurement has refuted stops
                // arguing for itself.
                let est = self.model.calibrate(meta.name, opt.estimate(ctx));
                let s = score_against(&meta.axis_effects, &est, policy, ctx.current_bytes);
                if s.total < MIN_SCORE {
                    continue;
                }
                out.push((
                    Decision::new(
                        meta.name,
                        scope,
                        DecisionAction::Enable,
                        format!("{}; {}", meta.summary, s.describe()),
                    ),
                    s.total,
                ));
            }
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out
    }

    /// Scopes worth turning off, worst first.
    ///
    /// The half a threshold cannot do. Whether an optimization *is paying* is a
    /// different question from whether it *looked promising*, and only the
    /// first can be answered after the fact.
    fn removals(
        &self,
        registry: &Registry,
        current: &OptimizationConfig,
        policy: &Policy,
        telemetry: &Snapshot,
        ctx: &OptContext<'_>,
    ) -> Vec<(Decision, f64)> {
        let mut out = Vec::new();
        for (name, scope, _) in current.entries() {
            if self.in_cooldown(name, scope) {
                continue;
            }
            let Some(opt) = registry.get(name) else {
                continue;
            };
            let meta = opt.meta();

            if let Some(reason) = unused_reason(name, scope, telemetry, ctx) {
                out.push((
                    Decision::new(meta.name, scope, DecisionAction::Disable, reason),
                    f64::NEG_INFINITY,
                ));
                continue;
            }

            // Otherwise re-score under the workload as it is now. A change that
            // was right when the workload was read-heavy may not be now.
            let est = self.model.calibrate(meta.name, opt.estimate(ctx));
            let s = score_against(&meta.axis_effects, &est, policy, ctx.current_bytes);
            if s.total < KEEP_SCORE {
                out.push((
                    Decision::new(
                        meta.name,
                        scope,
                        DecisionAction::Disable,
                        format!(
                            "no longer clears the bar that admitted it; {}",
                            s.describe()
                        ),
                    ),
                    s.total,
                ));
            }
        }
        out.sort_by(|a, b| a.1.total_cmp(&b.1));
        out
    }
}

/// Whether a disable was driven by measurement rather than by re-scoring.
fn evidence_based(trigger: &str) -> bool {
    trigger.contains("has not chosen") || trigger.contains("hit rate is")
}

/// Why an enabled scope is demonstrably not being used, if it is not.
fn unused_reason(
    name: &str,
    scope: &str,
    telemetry: &Snapshot,
    ctx: &OptContext<'_>,
) -> Option<String> {
    match name {
        "auto_index" => {
            // An index the planner never picks costs write maintenance and
            // memory for nothing. Judged per scope, so one dead index does not
            // take the useful ones with it.
            let (collection, field) = scope.split_once('.')?;
            if !ctx
                .existing_indexes
                .iter()
                .any(|(c, f, _)| c == collection && f == field)
            {
                return None;
            }
            // Judged against *recent* traffic, because telemetry decays. The
            // question is not whether this index was ever worth building — it
            // may well have been — but whether anything is using it now. An
            // index built for a workload that has ended costs write maintenance
            // and memory to answer questions nobody is asking.
            let recent = telemetry.total_calls();
            if recent < MIN_OBSERVATIONS {
                return None;
            }
            let uses = telemetry.index_use_count(collection, field);
            let upkeep = telemetry.index_maintenance_count(collection, field);
            if uses == 0 {
                return Some(format!(
                    "the planner has not chosen {collection}.{field} in the last {recent} \
                     operations, while paying {upkeep} index writes to keep it"
                ));
            }
            // **Cost-benefit, not use/don't-use.** An index the planner still
            // picks occasionally can nonetheless be a loss: every write to the
            // collection maintains it, and if it is bought thousands of times
            // per read it earns nothing. The old rule — retract only at
            // *zero* uses — could not see that at all, which is exactly the
            // half of the decision `Event::IndexMaintained` was added to
            // supply.
            //
            // The ratio is weighed against the policy's own axes rather than
            // a constant: a `resources`-heavy policy should give up on an
            // expensive index sooner than a `speed`-heavy one, and that
            // preference is precisely what `Priorities` exists to express.
            // `speed`/`resources` are 0-10, so the tolerated writes-per-use
            // spans a wide band without any new tunable.
            let p = ctx.policy.priority.clamped();
            let tolerance =
                MIN_WRITES_PER_USE * (1.0 + p.speed as f64) / (1.0 + p.resources as f64);
            let per_use = upkeep as f64 / uses as f64;
            if per_use > tolerance {
                return Some(format!(
                    "{collection}.{field} costs {per_use:.0} index writes per planner use, \
                     past the {tolerance:.0} this policy tolerates ({uses} uses, {upkeep} writes)"
                ));
            }
            None
        }
        "result_cache" | "plan_cache" => {
            let hits = telemetry.hit_rate(name)?;
            let probes = telemetry.cache_hits.get(name).copied().unwrap_or(0)
                + telemetry.cache_misses.get(name).copied().unwrap_or(0);
            if probes < MIN_OBSERVATIONS || hits >= 0.05 {
                return None;
            }
            Some(format!(
                "hit rate is {:.1}% over {probes} probes",
                hits * 100.0
            ))
        }
        _ => None,
    }
}

impl OptimizationDriver for AdaptiveDriver {
    fn source(&self) -> Source {
        Source::Adaptive
    }

    fn decide(&mut self, input: DriverInput<'_>) -> Vec<Decision> {
        self.cycles += 1;

        let now = metrics_from(input.telemetry, input.ctx.current_bytes);
        // Fold in anything that has had time to show an effect, so this cycle
        // decides against the most corrected priors available.
        self.learn(
            input.telemetry,
            input.ctx.current_bytes,
            !input.under_experiment.is_empty(),
        );

        // Acting on a handful of operations would be reacting to startup, not
        // to a workload.
        if input.telemetry.total_calls() < MIN_OBSERVATIONS {
            return Vec::new();
        }

        // Nor on a workload an experiment is reshaping. While one runs, queries
        // on its collection skip the caches and are planned around a structure
        // the planner cannot see yet, so the telemetry describes the trial
        // rather than the workload. On the soak, every filtered query was a
        // scan for the few cycles an index spent in shadow, which was enough
        // to admit a column store and read-ahead that the index made useless
        // the cycle it was promoted, and they were retracted again: an
        // add-then-drop in every filtering phase. The cache's real hit rate
        // also collapsed for the duration, which argued for retracting it.
        // Waiting a few cycles for the verdict costs less than either.
        if !input.under_experiment.is_empty() {
            return Vec::new();
        }

        // Removals first: freeing a resource may make an addition affordable,
        // and dropping something that is not paying is never regrettable.
        // Leave alone anything an experiment is in the middle of proving. It is
        // hidden from the planner, so its usage figures are a statement about
        // the experiment rather than about the workload, and both retracting it
        // and re-proposing it would be decisions taken on no evidence.
        let mut chosen = self.choose_retractions(&input);

        // What this workload looks like now, and what was learned about a
        // workload shaped like it before.
        let fingerprint = Fingerprint::of(input.telemetry);
        let recalled = self
            .memory
            .recall(&fingerprint)
            .map(|(cfg, score)| (cfg.clone(), score));

        let additions = self.ranked_additions(&input, recalled.as_ref());
        choose_additions(&mut chosen, additions, input.registry, input.policy);

        self.remember_if_settled(chosen.is_empty(), fingerprint, input.current);
        self.record_changes(&chosen, now, &input);
        self.proposals += chosen.len() as u64;
        chosen
    }
}

/// The steps of [`AdaptiveDriver::decide`], in the order it takes them.
impl AdaptiveDriver {
    /// Retractions to propose this cycle, worst first, never touching a scope
    /// under experiment or pinned.
    fn choose_retractions(&self, input: &DriverInput<'_>) -> Vec<Decision> {
        let mut chosen: Vec<Decision> = Vec::new();
        for (d, _) in self.removals(
            input.registry,
            input.current,
            input.policy,
            input.telemetry,
            input.ctx,
        ) {
            if chosen.len() >= MAX_CHANGES_PER_CYCLE {
                break;
            }
            // Never retract something correctness depends on, whatever it scores.
            if names_any(input.under_experiment, &d) || names_any(input.pinned, &d) {
                continue;
            }
            chosen.push(d);
        }
        chosen
    }

    /// Candidate additions not under experiment, best first, with anything a
    /// recalled configuration used moved to the front.
    fn ranked_additions(
        &self,
        input: &DriverInput<'_>,
        recalled: Option<&(OptimizationConfig, f64)>,
    ) -> Vec<(Decision, f64)> {
        let mut additions: Vec<(Decision, f64)> = self
            .additions(input.registry, input.current, input.policy, input.ctx)
            .into_iter()
            .filter(|(d, _)| !names_any(input.under_experiment, d))
            .collect();

        // **Recall reorders; it never admits.** Every candidate here has
        // already cleared `MIN_SCORE` on its own merits. A remembered
        // configuration only decides which of those to try *first*, which is
        // exactly the claim `crate::memory` makes for it: a hypothesis worth
        // trying first, not evidence that it is still correct. Lowering a bar
        // on the strength of memory would let a stale configuration reapply
        // itself against a workload that has genuinely moved on.
        if let Some((cfg, similarity)) = recalled {
            try_recalled_first(&mut additions, cfg, *similarity);
        }
        additions
    }

    /// Counts quiet cycles and, once the configuration has settled, remembers
    /// it for this workload shape.
    fn remember_if_settled(
        &mut self,
        nothing_chosen: bool,
        fingerprint: Fingerprint,
        current: &OptimizationConfig,
    ) {
        // A configuration is only worth remembering once it has stopped
        // moving. Remembering a half-converged one would teach the memory a
        // waypoint as a destination.
        // "Nothing proposed" is not the same as "settled". A driver can be
        // quiet because every candidate is in cooldown, or because a change
        // it already made is still waiting to be measured — both mean it is
        // mid-decision, and recording the configuration then would teach the
        // memory a waypoint as a destination. Settled means quiet *and* with
        // nothing outstanding to learn.
        if nothing_chosen && self.pending.is_empty() {
            self.stable_cycles = self.stable_cycles.saturating_add(1);
            if self.stable_cycles >= STABLE_CYCLES_BEFORE_REMEMBERING {
                self.memory.remember(fingerprint, current.clone());
            }
        } else {
            self.stable_cycles = 0;
        }
    }

    /// Starts the cooldown on every chosen scope, queues enables for
    /// measurement, and bars evidence-based retractions from quick re-proposal.
    fn record_changes(&mut self, chosen: &[Decision], now: Metrics, input: &DriverInput<'_>) {
        let latency = input
            .telemetry
            .per_shape
            .get(&QueryShape(now.shape))
            .map(|s| s.latency.clone())
            .unwrap_or_default();
        for d in chosen {
            self.mark_changed(d.optimization, &d.scope);
            match d.action {
                DecisionAction::Enable => self.pending.push(Pending {
                    optimization: d.optimization.to_string(),
                    baseline: now,
                    baseline_latency: latency.clone(),
                    applied_at: self.cycles,
                    confounded: false,
                    prior: input
                        .registry
                        .get(d.optimization)
                        .map(|o| o.estimate(input.ctx))
                        .unwrap_or_else(CostEstimate::neutral),
                }),
                DecisionAction::Disable if evidence_based(&d.trigger) => {
                    // Measurement refuted the estimate; do not let the estimate
                    // put it straight back.
                    self.retracted
                        .insert((d.optimization.to_string(), d.scope.clone()), self.cycles);
                }
                _ => {}
            }
        }
    }
}

/// Whether `d` is about one of the listed `(optimization, scope)` pairs.
fn names_any(pairs: &[(&'static str, String)], d: &Decision) -> bool {
    pairs
        .iter()
        .any(|(o, s)| *o == d.optimization && *s == d.scope)
}

/// Moves additions the recalled configuration used to the front, noting why
/// in their triggers; score order is kept within each group.
fn try_recalled_first(
    additions: &mut [(Decision, f64)],
    cfg: &OptimizationConfig,
    similarity: f64,
) {
    for (d, _) in additions.iter_mut() {
        if cfg.is_enabled(d.optimization, &d.scope) {
            d.trigger = format!(
                "{}; tried first — a workload {:.0}% like this one used it",
                d.trigger,
                similarity * 100.0
            );
        }
    }
    additions.sort_by(|a, b| {
        let a_known = cfg.is_enabled(a.0.optimization, &a.0.scope);
        let b_known = cfg.is_enabled(b.0.optimization, &b.0.scope);
        b_known.cmp(&a_known).then(b.1.total_cmp(&a.1))
    });
}

/// Fills what is left of the per-cycle change budget with the additions joint
/// search picks, in ranked order.
fn choose_additions(
    chosen: &mut Vec<Decision>,
    additions: Vec<(Decision, f64)>,
    registry: &Registry,
    policy: &Policy,
) {
    // Joint search over the combination rather than greedy top-N, so
    // conflicts, prerequisites and the shared memory budget are judged
    // against the set actually being proposed. See `crate::search` for
    // why greedy is provably wrong when optimizations interact.
    if chosen.len() >= MAX_CHANGES_PER_CYCLE || additions.is_empty() {
        return;
    }
    let candidates: Vec<Candidate<'_>> = additions
        .iter()
        .take(crate::search::MAX_EXHAUSTIVE)
        .filter_map(|(d, s)| {
            let meta = registry.meta(d.optimization)?;
            Some(Candidate {
                key: scope_key(d.optimization, &d.scope),
                name: meta.name,
                meta,
                solo_score: *s,
                ram_bytes: 0,
            })
        })
        .collect();
    let picked: Vec<String> = match best_combination(&candidates, policy) {
        Some(c) => c.keys,
        // Nothing coherent and affordable, or too many candidates to
        // enumerate exactly — fall back to the greedy order rather
        // than treat "no combination" as "propose nothing".
        None => additions
            .iter()
            .map(|(d, _)| scope_key(d.optimization, &d.scope))
            .collect(),
    };
    for (d, _) in additions {
        if chosen.len() >= MAX_CHANGES_PER_CYCLE {
            break;
        }
        if picked.contains(&scope_key(d.optimization, &d.scope)) {
            chosen.push(d);
        }
    }
}

/// Current workload metrics, as the model measures them.
///
/// Latency comes from the shape dominating total time rather than an average
/// across shapes: a change aimed at the expensive query is neither helped nor
/// hurt by how many trivial ones ran alongside it.
pub fn metrics_from(telemetry: &Snapshot, bytes: u64) -> Metrics {
    let hottest = telemetry.hottest_shapes(1);
    match hottest.first() {
        Some((shape, stats)) => Metrics {
            shape: shape.0,
            p50_nanos: stats.latency.percentile(50.0),
            p99_nanos: stats.latency.percentile(99.0),
            bytes,
            // The samples the percentiles are taken over. Not `calls`, which
            // decays while the histogram does not.
            operations: stats.latency.count(),
        },
        None => Metrics {
            bytes,
            ..Default::default()
        },
    }
}

/// The measured shape's latency over the samples recorded since `baseline`
/// was taken, or `None` if the shape has not run since.
fn metrics_since(
    telemetry: &Snapshot,
    baseline: &Metrics,
    baseline_latency: &Histogram,
    bytes: u64,
) -> Option<Metrics> {
    let stats = telemetry.per_shape.get(&QueryShape(baseline.shape))?;
    let newer = stats.latency.since(baseline_latency);
    Some(Metrics {
        shape: baseline.shape,
        p50_nanos: newer.percentile(50.0),
        p99_nanos: newer.percentile(99.0),
        bytes,
        operations: newer.count(),
    })
}

#[cfg(test)]
impl AdaptiveDriver {
    /// Workload shapes the driver has recorded a configuration for.
    ///
    /// A test hook, not public surface: the point of memory is that it
    /// changes behaviour, and behaviour is what the tests assert — this only
    /// lets one confirm the memory was populated at all rather than
    /// inferring it from an absence.
    pub(crate) fn remembered_shapes(&self) -> usize {
        self.memory.len()
    }
}

/// The wiring tests: memory and joint search have to *change what the driver
/// does*, not merely exist and pass their own unit tests.
///
/// These exist because the first version of outcome memory and joint search
/// shipped both modules fully tested in isolation and called from nowhere — the
/// same shipped-but-unreachable shape an audit had just caught elsewhere in
/// this codebase. A module with green tests and no call site is not a feature.
#[cfg(test)]
mod wiring {
    use super::*;
    use crate::cost::{AxisEffects, CostEstimate};
    use crate::optimization::{Applicability, OptMeta, Optimization, ScopeKind};
    use adabt_core::policy::GuaranteeRequirements;
    use adabt_telemetry::event::{Event, OpKind, QueryShape};
    use adabt_telemetry::{CollectingProbe, Probe, Snapshot};

    struct Simple(&'static OptMeta);

    const A_META: OptMeta = OptMeta {
        name: "a",
        summary: "a",
        scope_kind: ScopeKind::Global,
        min_level: 1,
        axis_effects: AxisEffects::new(8, -1, 0),
        requires_guarantees: GuaranteeRequirements::ANY,
        prerequisites: &[],
        conflicts_with: &[],
        reversibility: Reversibility::Instant,
    };

    impl Optimization for Simple {
        fn meta(&self) -> &OptMeta {
            self.0
        }
        fn applicability(&self, _: &OptContext<'_>) -> Applicability {
            Applicability::Applicable
        }
        fn estimate(&self, _: &OptContext<'_>) -> CostEstimate {
            CostEstimate::faster(0.4, 0.4).with_confidence(0.8)
        }
        fn plan_enable(
            &self,
            _: &OptContext<'_>,
            _: &str,
            _: &crate::config::Params,
        ) -> crate::action::ChangePlan {
            crate::action::ChangePlan::default()
        }
        fn plan_disable(
            &self,
            _: &OptContext<'_>,
            _: &str,
            _: &crate::config::Params,
        ) -> crate::action::ChangePlan {
            crate::action::ChangePlan::default()
        }
    }

    fn snap(reads: u32, writes: u32) -> Snapshot {
        let p = CollectingProbe::new();
        for _ in 0..reads {
            p.record(Event::Op {
                collection: "users",
                kind: OpKind::Get,
                shape: QueryShape(1),
                nanos: 100,
                rows: 1,
            });
        }
        for _ in 0..writes {
            p.record(Event::Op {
                collection: "users",
                kind: OpKind::Insert,
                shape: QueryShape(2),
                nanos: 100,
                rows: 1,
            });
        }
        p.snapshot()
    }

    struct Fx {
        collections: Vec<(String, usize)>,
        filtered: Vec<(String, String, u64)>,
        fixed: Vec<String>,
        max_ids: Vec<(String, u64)>,
        indexes: Vec<(String, String, adabt_core::index_kind::IndexKind)>,
    }
    impl Fx {
        fn new() -> Self {
            Self {
                collections: vec![("users".into(), 50_000)],
                filtered: vec![],
                fixed: vec![],
                max_ids: vec![("users".into(), 49_999)],
                indexes: vec![],
            }
        }
        fn ctx<'a>(&'a self, p: &'a Policy, s: &'a Snapshot) -> OptContext<'a> {
            OptContext {
                policy: p,
                telemetry: s,
                collections: &self.collections,
                filtered_fields: &self.filtered,
                fixed_size_collections: &self.fixed,
                max_ids: &self.max_ids,
                existing_indexes: &self.indexes,
                current_bytes: 4_000_000,
            }
        }
    }

    #[test]
    fn a_settled_configuration_is_remembered_after_the_workload_stops_moving() {
        let mut reg = Registry::new();
        reg.register(Box::new(Simple(&A_META)));
        let policy = Policy::conventional();
        let s = snap(2_000, 0);
        let fx = Fx::new();
        let ctx = fx.ctx(&policy, &s);
        // Already enabled, so nothing is left to propose and the driver is
        // by definition settled.
        let mut config = OptimizationConfig::new();
        config.enable("a", "global", Default::default());

        let mut d = AdaptiveDriver::new();
        assert_eq!(d.remembered_shapes(), 0);
        for _ in 0..(STABLE_CYCLES_BEFORE_REMEMBERING + 1) {
            d.decide(DriverInput {
                registry: &reg,
                current: &config,
                policy: &policy,
                telemetry: &s,
                ctx: &ctx,
                under_experiment: &[],
                pinned: &[],
            });
        }
        assert_eq!(
            d.remembered_shapes(),
            1,
            "the driver never recorded what this workload settled on"
        );
    }

    #[test]
    fn a_workload_still_converging_is_not_remembered() {
        // The failure this guards: recording a half-converged configuration
        // teaches the memory a waypoint as a destination.
        let mut reg = Registry::new();
        reg.register(Box::new(Simple(&A_META)));
        let policy = Policy::conventional();
        let s = snap(2_000, 0);
        let fx = Fx::new();
        let ctx = fx.ctx(&policy, &s);
        let empty = OptimizationConfig::new();

        let mut d = AdaptiveDriver::new();
        // Nothing enabled, so every cycle proposes something — never settled.
        for _ in 0..5 {
            let out = d.decide(DriverInput {
                registry: &reg,
                current: &empty,
                policy: &policy,
                telemetry: &s,
                ctx: &ctx,
                under_experiment: &[],
                pinned: &[],
            });
            // The driver may fall quiet on later cycles — every candidate
            // enters cooldown once proposed. Quiet is fine; what must not
            // happen is that quiet-because-mid-change gets recorded as a
            // settled configuration.
            let _ = out;
        }
        assert_eq!(
            d.remembered_shapes(),
            0,
            "a configuration that never settled was remembered anyway"
        );
    }

    /// Runs the driver from an empty configuration over a workload whose one
    /// shape takes `before_nanos` until the first change and `after_nanos`
    /// from then on, with an experiment running throughout if `experimenting`.
    /// Returns the p50 ratios the model recorded for `a`.
    fn measure(before_nanos: u64, after_nanos: u64, experimenting: bool) -> Vec<f64> {
        let mut reg = Registry::new();
        reg.register(Box::new(Simple(&A_META)));
        let policy = Policy::conventional();
        let fx = Fx::new();
        let probe = CollectingProbe::new();
        let record = |nanos: u64, n: usize| {
            for _ in 0..n {
                probe.record(Event::Op {
                    collection: "users",
                    kind: OpKind::Get,
                    shape: QueryShape(1),
                    nanos,
                    rows: 1,
                });
            }
        };
        // A long history before the change, so a measurement taken over the
        // whole histogram would barely see the change at all.
        record(before_nanos, 20_000);
        let under: Vec<(&'static str, String)> = if experimenting {
            vec![("other", "global".to_string())]
        } else {
            Vec::new()
        };
        let mut config = OptimizationConfig::new();
        let mut d = AdaptiveDriver::new();
        for _ in 0..(OBSERVE_AFTER_CYCLES + 2) {
            let s = probe.snapshot();
            let ctx = fx.ctx(&policy, &s);
            for x in d.decide(DriverInput {
                registry: &reg,
                current: &config,
                policy: &policy,
                telemetry: &s,
                ctx: &ctx,
                under_experiment: &under,
                pinned: &[],
            }) {
                config.enable(x.optimization, x.scope.clone(), x.params.clone());
            }
            record(after_nanos, 500);
        }
        d.model()
            .observations("a")
            .iter()
            .map(|o| o.p50_ratio)
            .collect()
    }

    #[test]
    fn a_change_is_measured_on_the_samples_since_it_was_made() {
        // Twenty thousand samples at 1 us, then the change halves it. Over
        // the cumulative histogram the median would still read 1 us.
        let seen = measure(1_000, 500, false);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0] < 0.6, "the halving was measured as {:.2}x", seen[0]);
    }

    #[test]
    fn nothing_is_decided_while_an_experiment_reshapes_the_workload() {
        let mut reg = Registry::new();
        reg.register(Box::new(Simple(&A_META)));
        let policy = Policy::conventional();
        let s = snap(2_000, 0);
        let fx = Fx::new();
        let ctx = fx.ctx(&policy, &s);
        let empty = OptimizationConfig::new();
        let input = |under| DriverInput {
            registry: &reg,
            current: &empty,
            policy: &policy,
            telemetry: &s,
            ctx: &ctx,
            under_experiment: under,
            pinned: &[],
        };

        let trial = [("other", "global".to_string())];
        assert!(AdaptiveDriver::new().decide(input(&trial)).is_empty());
        // With no trial running, the same inputs do produce a proposal.
        assert!(!AdaptiveDriver::new().decide(input(&[])).is_empty());
    }

    #[test]
    fn a_change_made_beside_an_experiment_is_not_measured() {
        // An experiment reroutes queries around the caches and through its
        // candidate, so the window measures the experiment, not this change.
        assert!(measure(1_000, 500, true).is_empty());
    }

    #[test]
    fn joint_search_still_proposes_an_independent_candidate() {
        // Search replaced greedy top-N selection; the floor is that a single
        // clearly-good candidate is still chosen. A search that returned
        // nothing here would silently disable the whole adaptive path.
        let mut reg = Registry::new();
        reg.register(Box::new(Simple(&A_META)));
        let policy = Policy::conventional();
        let s = snap(2_000, 0);
        let fx = Fx::new();
        let ctx = fx.ctx(&policy, &s);
        let empty = OptimizationConfig::new();

        let mut d = AdaptiveDriver::new();
        let out = d.decide(DriverInput {
            registry: &reg,
            current: &empty,
            policy: &policy,
            telemetry: &s,
            ctx: &ctx,
            under_experiment: &[],
            pinned: &[],
        });
        assert!(
            out.iter()
                .any(|x| x.optimization == "a" && x.action == DecisionAction::Enable),
            "joint search dropped a candidate greedy would have taken: {out:?}"
        );
    }
}
