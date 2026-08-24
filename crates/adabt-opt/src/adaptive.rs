//! The adaptive driver.
//!
//! Observes, proposes, and — the part that matters — *retracts*. An optimizer
//! that only ever adds is a ratchet, and the M7 matrix showed what that costs:
//! a level built an index the planner never chose, paying memory and write
//! maintenance for it forever, and no threshold on "was this field filtered"
//! could have noticed.
//!
//! # Evidence, estimate, and the argument between them
//!
//! M8 ended with the driver contradicting itself: it dropped an unused index on
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
use adabt_telemetry::Snapshot;
use std::collections::HashMap;

use crate::config::OptimizationConfig;
use crate::decision::{Decision, DecisionAction, Source};
use crate::driver::{DriverInput, OptimizationDriver};
use crate::model::{CostModel, Metrics, Observation};
use crate::optimization::{permitted_by, OptContext, Reversibility};
use crate::registry::Registry;
use crate::score::score_against;

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
/// That is not hypothetical. `docs/diagnosis.md` records a standing measurement
/// of it: across every soak run the aggregate phase fails to improve, because
/// `column_store` is applied on a 40%-confidence prior, scores 0.54, and is
/// never re-examined.
///
/// The bar to keep is therefore the bar to admit, less a hysteresis margin. The
/// margin is what stops the two rules arguing with each other — the failure mode
/// M8 already found once, where the driver dropped a thing on evidence and
/// re-added it on estimate forever.
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

/// A change waiting to be measured.
struct Pending {
    optimization: String,
    baseline: Metrics,
    applied_at: u64,
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
            proposals: 0,
        }
    }

    pub fn cycles(&self) -> u64 {
        self.cycles
    }

    /// What the model has learned, for the decision log.
    /// Start with priors already corrected by earlier measurement.
    ///
    /// A driver that forgets everything it measured whenever the process
    /// restarts relearns the same corrections from scratch, and relearning is
    /// not free: it costs another round of applying changes that were already
    /// known not to work. Persisting the model is later work; this is the seam
    /// it needs, and it is what lets a test state "given this has been measured
    /// N times" without simulating N cycles.
    pub fn with_model(model: CostModel) -> Self {
        Self {
            model,
            ..Self::new()
        }
    }

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
    fn learn(&mut self, now: Metrics) {
        let cycle = self.cycles;
        let mut still_pending = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            if cycle < p.applied_at + OBSERVE_AFTER_CYCLES {
                still_pending.push(p);
                continue;
            }
            if let Some(obs) = Observation::between(&p.baseline, &now) {
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
        let DriverInput {
            registry,
            current,
            policy,
            telemetry,
            ctx,
            under_experiment,
            pinned,
        } = input;

        let now = metrics_from(telemetry, ctx.current_bytes);
        // Fold in anything that has had time to show an effect, so this cycle
        // decides against the most corrected priors available.
        self.learn(now);

        // Acting on a handful of operations would be reacting to startup, not
        // to a workload.
        if telemetry.total_calls() < MIN_OBSERVATIONS {
            return Vec::new();
        }

        // Removals first: freeing a resource may make an addition affordable,
        // and dropping something that is not paying is never regrettable.
        // Leave alone anything an experiment is in the middle of proving. It is
        // hidden from the planner, so its usage figures are a statement about
        // the experiment rather than about the workload, and both retracting it
        // and re-proposing it would be decisions taken on no evidence.
        let untouchable = |d: &Decision| {
            under_experiment
                .iter()
                .any(|(o, s)| *o == d.optimization && *s == d.scope)
        };

        let is_pinned = |d: &Decision| {
            pinned
                .iter()
                .any(|(o, s)| *o == d.optimization && *s == d.scope)
        };

        let mut chosen: Vec<Decision> = Vec::new();
        for (d, _) in self.removals(registry, current, policy, telemetry, ctx) {
            if chosen.len() >= MAX_CHANGES_PER_CYCLE {
                break;
            }
            // Never retract something correctness depends on, whatever it scores.
            if untouchable(&d) || is_pinned(&d) {
                continue;
            }
            chosen.push(d);
        }
        for (d, _) in self.additions(registry, current, policy, ctx) {
            if chosen.len() >= MAX_CHANGES_PER_CYCLE {
                break;
            }
            if untouchable(&d) {
                continue;
            }
            chosen.push(d);
        }

        for d in &chosen {
            self.mark_changed(d.optimization, &d.scope);
            match d.action {
                DecisionAction::Enable => self.pending.push(Pending {
                    optimization: d.optimization.to_string(),
                    baseline: now,
                    applied_at: self.cycles,
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
        self.proposals += chosen.len() as u64;
        chosen
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
        Some((_, stats)) => Metrics {
            p50_nanos: stats.latency.percentile(50.0),
            p99_nanos: stats.latency.percentile(99.0),
            bytes,
            operations: stats.calls,
        },
        None => Metrics {
            bytes,
            ..Default::default()
        },
    }
}
