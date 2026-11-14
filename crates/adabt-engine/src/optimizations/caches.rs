//! Optimizations that keep an answer instead of recomputing it: the plan
//! cache, the result cache and materialized views.
//!
//! All three spend memory, or write-path work, to skip repeating a computation
//! whose result is already known.

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- plan cache -------------------------------------------------------------

pub struct PlanCacheOpt;

const PLAN_CACHE_META: OptMeta = OptMeta {
    name: "plan_cache",
    summary: "reuse physical plans across queries of the same shape",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(2, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for PlanCacheOpt {
    fn meta(&self) -> &OptMeta {
        &PLAN_CACHE_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        Applicability::Applicable
    }
    fn estimate(&self, _ctx: &OptContext<'_>) -> CostEstimate {
        // Planning is a small part of a query's cost, so the benefit is modest
        // and the confidence low until measured.
        CostEstimate::faster(0.95, 0.95)
            .with_ram(512 * 4_096)
            .with_confidence(0.4)
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        let n = entries(ctx, 512);
        ChangePlan::new(
            vec![Action::SetPlanCacheEntries(n)],
            vec![Action::SetPlanCacheEntries(0)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetPlanCacheEntries(0)], vec![])
    }
}

// -- result cache -----------------------------------------------------------

pub struct ResultCacheOpt;

const RESULT_CACHE_META: OptMeta = OptMeta {
    name: "result_cache",
    summary: "serve repeated identical queries from memory",
    scope_kind: ScopeKind::Global,
    min_level: 1,
    axis_effects: AxisEffects::new(5, -4, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for ResultCacheOpt {
    fn meta(&self) -> &OptMeta {
        &RESULT_CACHE_META
    }
    fn applicability(&self, _ctx: &OptContext<'_>) -> Applicability {
        // Structurally always possible. Whether it is a good *idea* under a
        // write-heavy workload is a scoring judgment, and it belongs in the
        // estimate below rather than here.
        //
        // Putting it here was a bug worth recording: a bulk load makes the
        // workload look 100% writes, so a database loaded and then queried
        // would refuse its own result cache forever on the strength of its
        // loading phase. Applicability answers "is this possible", not "is this
        // wise" — conflating the two lets a transient workload permanently
        // veto an optimization.
        Applicability::Applicable
    }
    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let read_fraction = 1.0 - ctx.telemetry.write_fraction();
        CostEstimate::faster(1.0 - 0.5 * read_fraction, 1.0 - 0.3 * read_fraction)
            .with_ram(entries(ctx, 256) as i64 * 4_096)
            .with_maintenance(0.02)
            .with_confidence(0.35)
    }
    fn plan_enable(&self, ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        let n = entries(ctx, 256);
        ChangePlan::new(
            vec![Action::SetResultCacheEntries(n)],
            vec![Action::SetResultCacheEntries(0)],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetResultCacheEntries(0)], vec![])
    }
}

// -- materialized views -----------------------------------------------------

pub struct MaterializedViewOpt;

const MATERIALIZED_VIEW_META: OptMeta = OptMeta {
    name: "materialized_view",
    summary: "keep grouped counts up to date on write instead of recomputing them",
    scope_kind: ScopeKind::Global,
    min_level: 4,
    // A large speed effect on the queries it covers — a handful of numbers
    // instead of every row — paid for on the write path, where every insert and
    // delete now adjusts a total. The freedom axis is untouched: nothing about
    // the data or the schema is constrained by keeping a count of it.
    axis_effects: AxisEffects::new(7, -2, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// Below this, recomputing the aggregate is cheap enough that maintaining it on
/// every write costs more than it saves.
const MIN_ROWS_FOR_VIEW: usize = 2_000;

impl Optimization for MaterializedViewOpt {
    fn meta(&self) -> &OptMeta {
        &MATERIALIZED_VIEW_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let biggest = ctx.collections.iter().map(|(_, n)| *n).max().unwrap_or(0);
        if biggest < MIN_ROWS_FOR_VIEW {
            return Applicability::NotYet(format!(
                "the largest collection holds {biggest} rows, below the \
                 {MIN_ROWS_FOR_VIEW} at which maintaining a total beats recomputing it"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        // The saving is enormous where it applies and nil where it does not, and
        // which of those is true depends on how much of the workload aggregates
        // — something only telemetry knows. Hence the low confidence: this is an
        // estimate that expects to be corrected by measurement.
        let read_share = 1.0 - ctx.telemetry.write_fraction();
        CostEstimate::faster(1.0 - 0.5 * read_share, 1.0 - 0.5 * read_share)
            .with_maintenance(0.03)
            .with_confidence(0.35)
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetMaterializedViews(true)],
            vec![Action::SetMaterializedViews(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetMaterializedViews(false)],
            vec![Action::SetMaterializedViews(true)],
        )
    }
}

/// Cache size from the level preset, falling back to a default.
fn entries(ctx: &OptContext<'_>, default: usize) -> usize {
    let _ = ctx;
    default
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizations::tests::Fx;

    #[test]
    fn a_write_heavy_workload_lowers_the_result_cache_estimate_without_vetoing_it() {
        use adabt_telemetry::Probe as _;
        let mut fx = Fx::new();
        let read_heavy = ResultCacheOpt.estimate(&fx.ctx());

        let probe = adabt_telemetry::CollectingProbe::new();
        for _ in 0..200 {
            probe.record(adabt_telemetry::Event::Op {
                collection: "users",
                kind: adabt_telemetry::OpKind::Insert,
                shape: adabt_telemetry::QueryShape::UNKNOWN,
                nanos: 1,
                rows: 1,
            });
        }
        fx.snap = probe.snapshot();
        let write_heavy = ResultCacheOpt.estimate(&fx.ctx());

        // Still possible — a bulk load must not permanently veto the cache —
        // but plainly worth less.
        assert!(ResultCacheOpt.applicability(&fx.ctx()).is_applicable());
        assert!(
            write_heavy.p50_delta.0 > read_heavy.p50_delta.0,
            "a write-heavy workload should reduce the expected benefit"
        );
    }
}
