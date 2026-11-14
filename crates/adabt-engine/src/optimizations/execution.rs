//! Optimizations that change how queries execute rather than how data is
//! stored: best-effort pinning of a sharded database's scan workers to cores.

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- thread-per-core --------------------------------------------------------

pub struct ThreadPerCoreOpt;

const THREAD_PER_CORE_META: OptMeta = OptMeta {
    name: "thread_per_core",
    summary: "pin a sharded database's scan workers to cores (best effort)",
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

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        // Pinning changes where scan workers run, and nothing else: a lookup
        // or a write takes one shard and never reaches a worker. So the gain
        // is claimed on the share of the workload that scans. Whether the
        // database is sharded at all is not visible here; on one that is not,
        // calibration has to learn that this does nothing.
        let scans = ctx.workload().scan_share();
        CostEstimate::faster(1.0 - 0.4 * scans, 1.0 - 0.5 * scans)
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
