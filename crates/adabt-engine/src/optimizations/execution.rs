//! Optimizations that change how queries execute rather than how data is
//! stored: per-core sharding and cardinality-based join ordering.

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- thread-per-core --------------------------------------------------------

pub struct ThreadPerCoreOpt;

const THREAD_PER_CORE_META: OptMeta = OptMeta {
    name: "thread_per_core",
    summary: "per-core sharding with pinning and run-to-completion",
    scope_kind: ScopeKind::Global,
    min_level: 9,
    axis_effects: AxisEffects::new(8, -6, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

impl Optimization for ThreadPerCoreOpt {
    fn meta(&self) -> &OptMeta {
        &THREAD_PER_CORE_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < 10_000 {
            return Applicability::NotYet(format!(
                "largest collection holds {biggest} rows, below the 10,000 at which per-core sharding pays"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, _ctx: &OptContext<'_>) -> CostEstimate {
        CostEstimate::faster(0.6, 0.5)
            .with_ram(0)
            .with_maintenance(0.05)
            .with_confidence(0.3)
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetThreadPerCore(true)],
            vec![Action::SetThreadPerCore(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetThreadPerCore(false)], vec![])
    }
}

// -- join order -------------------------------------------------------------

pub struct JoinOrderOpt;

const JOIN_ORDER_META: OptMeta = OptMeta {
    name: "join_order",
    summary: "reorder join inputs by estimated cardinality (small side build)",
    scope_kind: ScopeKind::Global,
    min_level: 6,
    axis_effects: AxisEffects::new(5, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for JoinOrderOpt {
    fn meta(&self) -> &OptMeta {
        &JOIN_ORDER_META
    }
    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        // Needs at least one join observed; telemetry for joins is derived from
        // filtered_fields co-occurrence already used for composite indexes.
        let has_join = ctx
            .telemetry
            .most_pinned_sets()
            .iter()
            .any(|(_, f, _)| f.len() >= 2);
        if !has_join {
            return Applicability::NotYet("no join-shaped queries observed yet".into());
        }
        if ctx.collections.iter().map(|(_, n)| *n).sum::<usize>() < 1_000 {
            return Applicability::NotYet("collections too small for join order to matter".into());
        }
        Applicability::Applicable
    }
    fn estimate(&self, _ctx: &OptContext<'_>) -> CostEstimate {
        CostEstimate::faster(0.6, 0.55)
            .with_confidence(0.4)
            .with_build(BuildCost {
                estimated_secs: 0.0,
                rows_read: 0,
                online: true,
            })
    }
    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetJoinOrder(true)],
            vec![Action::SetJoinOrder(false)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetJoinOrder(false)], vec![])
    }
}
