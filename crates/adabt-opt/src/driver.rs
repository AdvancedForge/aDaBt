//! Drivers: who chooses.
//!
//! This is the symmetry the architecture is built around. Manual selection and
//! adaptive selection are two implementations of one trait, emitting the same
//! `Decision` values into the same controller. Nothing the optimizer can do is
//! something a human could not have asked for by hand, and nothing a human can
//! ask for bypasses the machinery the optimizer uses.
//!
//! `AdaptiveDriver` is a stub returning no decisions. It exists now so the
//! symmetry is structurally enforced rather than merely intended — the seam is
//! load-bearing from the first day, not retrofitted in Phase 5.

use adabt_core::policy::{Mode, Policy};
use adabt_telemetry::Snapshot;

use crate::optimization::{OptContext, ScopeKind};

use crate::config::{OptimizationConfig, Params};
use crate::decision::{Decision, DecisionAction, Source};
use crate::levels::config_for_level;
use crate::registry::Registry;

pub trait OptimizationDriver {
    fn source(&self) -> Source;

    /// What should change, given what the workload looks like now.
    ///
    /// Takes the same `OptContext` an optimization is judged against, so a
    /// driver cannot reach a different conclusion from the one the controller
    /// will act on.
    fn decide(&mut self, input: DriverInput<'_>) -> Vec<Decision>;
}

/// Everything a driver may look at.
pub struct DriverInput<'a> {
    pub registry: &'a Registry,
    pub current: &'a OptimizationConfig,
    pub policy: &'a Policy,
    pub telemetry: &'a Snapshot,
    pub ctx: &'a OptContext<'a>,
    /// `(optimization, scope)` that may never be retracted, for any reason.
    ///
    /// **Some structures are load-bearing for correctness, not for speed.** An
    /// index backing a unique constraint is the canonical case: dropping it does
    /// not make the database slower, it makes it wrong, and no amount of
    /// cost-benefit arithmetic should be able to reach that conclusion.
    ///
    /// Expressed as a hard exclusion rather than a very large score, because a
    /// score is something the optimizer weighs and this is something it must not
    /// weigh at all. It is the same shape as the guarantees filter: not
    /// expensive, invisible.
    ///
    /// Empty until unique constraints exist. It is here first so that the rule
    /// is in place before the retraction logic gets more aggressive, rather than
    /// being remembered afterwards.
    pub pinned: &'a [(&'static str, String)],
    /// `(optimization, scope)` currently being proved by an experiment.
    ///
    /// **A structure under experiment is invisible to the workload on purpose,
    /// and its telemetry says so.** A candidate index is hidden from the planner
    /// until it has earned its way out, so the planner never chooses it, so
    /// every use-based measurement reads zero — which is exactly what an index
    /// nobody wants also reads.
    ///
    /// Without this, the two mechanisms destroy each other. The retraction logic
    /// sees a structure with no uses and drops it as dead weight; the experiment
    /// then promotes a thing that no longer exists and records the promotion as
    /// a success. Both components are individually correct and the combination
    /// is silently wrong, which is why it took a soak run to find rather than a
    /// unit test.
    pub under_experiment: &'a [(&'static str, String)],
}

/// Resolves a level and explicit overrides into decisions.
///
/// Once it has brought the configuration in line with the policy it proposes
/// nothing further: manual mode means the engine does not change strategy on its
/// own, and a driver that kept re-proposing would violate that.
#[derive(Default)]
pub struct ManualDriver;

impl OptimizationDriver for ManualDriver {
    fn source(&self) -> Source {
        Source::Manual
    }

    fn decide(&mut self, input: DriverInput<'_>) -> Vec<Decision> {
        let ctx = input.ctx;
        let DriverInput {
            registry,
            current,
            policy,
            ..
        } = input;
        let Mode::Manual { level, overrides } = &policy.mode else {
            return Vec::new();
        };

        let mut target = config_for_level(*level);
        // Explicit settings win over the level. A level is a starting posture,
        // not a cage.
        //
        // A level only ever enables an optimization at `"global"` — the same
        // blanket request an unscoped override makes, expanded the same way
        // below. Naming a specific scope is the caller choosing exactly which
        // of those candidates it wants, so the level's own blanket entry for
        // that optimization is cleared first: without this, both requests
        // would survive side by side, and the level's blanket one — with no
        // params of its own — would still expand into every candidate
        // scope's `Decision`, including the one already spoken for, silently
        // fighting the very override that named it.
        for ov in overrides {
            if ov.scope != "global" {
                target.disable(&ov.name, "global");
            }
        }
        for ov in overrides {
            let params = ov
                .params
                .iter()
                .fold(Params::new(), |p, (k, v)| p.with(k.clone(), *v));
            if ov.enabled {
                target.enable(ov.name.clone(), ov.scope.clone(), params);
            } else {
                target.disable(&ov.name, &ov.scope);
            }
        }

        let mut out = Vec::new();
        let diff = current.diff(&target);
        for (name, scope) in diff.added {
            let Some(opt) = registry.get(&name) else {
                continue;
            };
            let meta = opt.meta();
            let params = target.params(&name, &scope).cloned().unwrap_or_default();
            // A level names an optimization, not a scope — and so does an
            // override left at the default `"global"`. Either way, a
            // non-`Global` optimization has to be expanded into the scopes
            // that actually exist, or it enables something with nothing to
            // act on. An override that named a specific scope explicitly
            // (`"users.country"`, not `"global"`) skips that expansion and
            // targets exactly the place it named — this is what "index
            // users.country hash" means as data: the caller is not asking
            // `auto_index` to reconsider every field, only this one.
            //
            // Both the level path and the adaptive driver still expand
            // through the same `candidate_scopes`, so manual and adaptive
            // cannot disagree about what an unscoped request means.
            let scopes = if meta.scope_kind == ScopeKind::Global || scope != "global" {
                vec![scope]
            } else {
                opt.candidate_scopes(ctx)
            };
            for s in scopes {
                out.push(Decision {
                    optimization: meta.name,
                    scope: s,
                    action: DecisionAction::Enable,
                    params: params.clone(),
                    trigger: format!("level {level} preset"),
                });
            }
        }
        for (name, scope) in diff.removed {
            let Some(meta) = registry.meta(&name) else {
                continue;
            };
            out.push(Decision {
                optimization: meta.name,
                scope,
                action: DecisionAction::Disable,
                params: Default::default(),
                trigger: format!("not part of level {level}"),
            });
        }
        for (name, scope) in diff.retuned {
            let Some(meta) = registry.meta(&name) else {
                continue;
            };
            let params = target.params(&name, &scope).cloned().unwrap_or_default();
            out.push(Decision {
                optimization: meta.name,
                scope,
                action: DecisionAction::Retune,
                params,
                trigger: format!("level {level} tuning"),
            });
        }
        out
    }
}
