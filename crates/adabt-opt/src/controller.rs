//! The optimization controller.
//!
//! **The only code path that changes physical strategy.** Manual and adaptive
//! decisions both arrive here, are gated identically, and are logged
//! identically. Nothing bypasses it — including a human setting a level.
//!
//! Gating order is not arbitrary. Guarantees are checked first, as a filter,
//! before anything is scored or costed: an optimization the policy forbids is
//! not expensive, it is invisible. Constraints come next, as hard feasibility.
//! Only what survives both is considered on its merits.

use adabt_core::error::Result;
use adabt_core::policy::Policy;

use crate::action::{ActionSink, ChangePlan};
use crate::config::OptimizationConfig;
use crate::cost::CostEstimate;
use crate::decision::{Decision, DecisionAction, DecisionLog, Source, Verdict};
use crate::optimization::{permitted_by, OptContext};
use crate::registry::Registry;

/// The read-only inputs a decision is judged against.
pub struct ApplyEnv<'a> {
    pub registry: &'a Registry,
    pub policy: &'a Policy,
    pub ctx: &'a OptContext<'a>,
}

pub struct OptimizationController {
    config: OptimizationConfig,
    log: DecisionLog,
    /// Applied plans, so a change can be taken back exactly as it was made.
    applied: Vec<(String, String, ChangePlan)>,
}

impl Default for OptimizationController {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of applying a batch of decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub rejected: Vec<(String, Verdict)>,
}

impl ApplyReport {
    pub fn all_applied(&self) -> bool {
        self.rejected.is_empty()
    }
}

impl OptimizationController {
    pub fn new() -> Self {
        Self {
            config: OptimizationConfig::new(),
            log: DecisionLog::new(),
            applied: Vec::new(),
        }
    }

    pub fn config(&self) -> &OptimizationConfig {
        &self.config
    }
    pub fn log(&self) -> &DecisionLog {
        &self.log
    }
    pub fn explain(&self, optimization: &str) -> String {
        self.log.explain(optimization)
    }
    pub fn explain_all(&self) -> String {
        self.log.explain_all()
    }

    /// Estimated resources currently committed to enabled optimizations.
    fn committed_ram(&self, registry: &Registry, ctx: &OptContext<'_>) -> i64 {
        self.config
            .entries()
            .filter_map(|(n, _, _)| registry.get(n))
            .map(|o| o.estimate(ctx).ram_bytes.max(0))
            .sum()
    }

    /// Apply a batch of decisions, gating and logging each one.
    pub fn apply<S: ActionSink>(
        &mut self,
        decisions: Vec<Decision>,
        env: ApplyEnv<'_>,
        sink: &mut S,
        source: Source,
    ) -> Result<ApplyReport> {
        let ApplyEnv {
            registry,
            policy,
            ctx,
        } = env;
        let mut report = ApplyReport::default();

        for decision in decisions {
            let name = decision.optimization;
            let scope = decision.scope.clone();
            let label = format!("{name}[{scope}]");

            let Some(opt) = registry.get(name) else {
                self.reject(
                    decision,
                    Verdict::NotApplicable,
                    Some("not registered".into()),
                    None,
                );
                report.rejected.push((label, Verdict::NotApplicable));
                continue;
            };
            let meta = opt.meta();

            if decision.action == DecisionAction::Disable {
                self.disable(decision, registry, ctx, sink, source, &mut report)?;
                continue;
            }

            // 1. Guarantees. A filter, not a cost. Checked before anything else
            //    so a forbidden optimization is never even priced.
            if !permitted_by(meta, policy) {
                let detail = format!(
                    "policy requires durability {:?} / consistency {:?}",
                    policy.guarantees.durability, policy.guarantees.consistency
                );
                self.reject(decision, Verdict::ForbiddenByGuarantees, Some(detail), None);
                report
                    .rejected
                    .push((label, Verdict::ForbiddenByGuarantees));
                continue;
            }

            // 2. Prerequisites.
            let missing: Vec<&str> = meta
                .prerequisites
                .iter()
                .filter(|p| !self.config.is_enabled_anywhere(p))
                .copied()
                .collect();
            if !missing.is_empty() {
                let detail = format!("requires {}", missing.join(", "));
                self.reject(decision, Verdict::MissingPrerequisite, Some(detail), None);
                report.rejected.push((label, Verdict::MissingPrerequisite));
                continue;
            }

            // 3. Conflicts.
            let enabled: Vec<String> = self
                .config
                .entries()
                .map(|(n, _, _)| n.to_string())
                .collect();
            let conflicts = registry.conflicts_among(name, &enabled);
            if !conflicts.is_empty() {
                let detail = format!("conflicts with {}", conflicts.join(", "));
                self.reject(decision, Verdict::Conflicts, Some(detail), None);
                report.rejected.push((label, Verdict::Conflicts));
                continue;
            }

            // 4. Applicability.
            let applicability = opt.applicability(ctx);
            if !applicability.is_applicable() {
                let detail = applicability.reason().map(str::to_string);
                self.reject(decision, Verdict::NotApplicable, detail, None);
                report.rejected.push((label, Verdict::NotApplicable));
                continue;
            }

            let estimate = opt.estimate(ctx);
            let plan = opt.plan_enable(ctx, &scope, &decision.params);

            // 5. Constraints. Hard ceilings, checked against what is already
            //    committed rather than against this change alone.
            if let Some(max_ram) = policy.constraints.max_ram_bytes {
                let projected = self.committed_ram(registry, ctx) + estimate.ram_bytes.max(0);
                if projected as u64 > max_ram {
                    let detail = format!(
                        "would use {:.1} MB against a {:.1} MB ceiling",
                        projected as f64 / 1e6,
                        max_ram as f64 / 1e6
                    );
                    self.reject(
                        decision,
                        Verdict::ExceedsConstraints,
                        Some(detail),
                        Some(estimate),
                    );
                    report.rejected.push((label, Verdict::ExceedsConstraints));
                    continue;
                }
            }

            // 6. The sink gets the last word: it knows what actually exists.
            if let Some(bad) = plan.apply.iter().find(|a| !sink.can_apply(a)) {
                let detail = format!("engine refused: {}", bad.describe());
                self.reject(
                    decision,
                    Verdict::NotApplicable,
                    Some(detail),
                    Some(estimate),
                );
                report.rejected.push((label, Verdict::NotApplicable));
                continue;
            }

            for action in &plan.apply {
                sink.apply_action(action)?;
            }
            self.config
                .enable(name, scope.clone(), decision.params.clone());
            self.applied
                .push((name.to_string(), scope.clone(), plan.clone()));
            self.log.record(
                decision,
                Verdict::Applied,
                None,
                Some(estimate),
                plan,
                source,
            );
            report.applied.push(label);
        }
        Ok(report)
    }

    fn disable<S: ActionSink>(
        &mut self,
        decision: Decision,
        registry: &Registry,
        ctx: &OptContext<'_>,
        sink: &mut S,
        source: Source,
        report: &mut ApplyReport,
    ) -> Result<()> {
        let name = decision.optimization;
        let scope = decision.scope.clone();
        let label = format!("{name}[{scope}]");

        // Undo with the exact inverse recorded when it was applied. Working the
        // inverse out now, from an engine that has already changed, would be
        // guesswork.
        let recorded = self
            .applied
            .iter()
            .rposition(|(n, s, _)| n == name && *s == scope)
            .map(|i| self.applied.remove(i));

        let plan = match recorded {
            Some((_, _, applied)) => ChangePlan::new(applied.revert.clone(), applied.apply),
            None => registry
                .get(name)
                .map(|o| o.plan_disable(ctx, &scope, &decision.params))
                .unwrap_or_default(),
        };

        for action in &plan.apply {
            sink.apply_action(action)?;
        }
        self.config.disable(name, &scope);
        self.log
            .record(decision, Verdict::Applied, None, None, plan, source);
        report.applied.push(label);
        Ok(())
    }

    /// Record an outcome for a change that is already in the log.
    ///
    /// The experiment runner needs this because it cannot know at apply time
    /// whether a change will survive: a structure built for a trial is logged as
    /// applied — it genuinely was — and only later earns a promotion or a
    /// revert. Writing that verdict as a second record rather than editing the
    /// first keeps the log append-only, so "this was promoted after being
    /// trialled" and "this was applied outright" stay distinguishable.
    pub fn note(&mut self, decision: Decision, verdict: Verdict, detail: String, source: Source) {
        self.log.record(
            decision,
            verdict,
            Some(detail),
            None,
            ChangePlan::default(),
            source,
        );
    }

    fn reject(
        &mut self,
        decision: Decision,
        verdict: Verdict,
        detail: Option<String>,
        estimate: Option<CostEstimate>,
    ) {
        self.log.record(
            decision,
            verdict,
            detail,
            estimate,
            ChangePlan::default(),
            Source::Manual,
        );
    }
}
