//! Optimizations that decide where records sit on pages: clustering by a
//! range-filtered key, so a range over it touches pages in proportion to the
//! range rather than the collection.

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

use super::split_scope;

// -- clustered sort ---------------------------------------------------------

pub struct ClusteredSortOpt;

const CLUSTERED_SORT_META: OptMeta = OptMeta {
    name: "clustered_sort",
    summary: "co-locate records with nearby clustering keys so range scans touch pages in proportion to range",
    scope_kind: ScopeKind::PerField,
    min_level: 5,
    axis_effects: AxisEffects::new(6, -2, -1),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

const MIN_ROWS_FOR_CLUSTER: usize = 5_000;
const MIN_QUERIES_FOR_CLUSTER: u64 = 10;

impl ClusteredSortOpt {
    fn candidates(ctx: &OptContext<'_>) -> Vec<(String, String)> {
        ctx.telemetry
            .most_range_filtered_fields()
            .into_iter()
            .filter(|(c, f, n)| {
                *n >= MIN_QUERIES_FOR_CLUSTER
                    && ctx.rows_in(c) >= MIN_ROWS_FOR_CLUSTER
                    && ctx
                        .filtered_fields
                        .iter()
                        .any(|(cc, ff, _)| cc == c && ff == f)
            })
            .map(|(c, f, _)| (c, f))
            .collect()
    }
}

impl Optimization for ClusteredSortOpt {
    fn meta(&self) -> &OptMeta {
        &CLUSTERED_SORT_META
    }

    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        Self::candidates(ctx)
            .into_iter()
            .map(|(c, f)| format!("{c}.{f}"))
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let ranges = ctx.telemetry.most_range_filtered_fields();
        if ranges.is_empty() {
            return Applicability::NotYet("no query has used a range predicate yet".to_string());
        }
        if Self::candidates(ctx).is_empty() {
            let (c, f, n) = &ranges[0];
            let detail = if *n < MIN_QUERIES_FOR_CLUSTER {
                format!("{c}.{f} ranged only {n} times, below the {MIN_QUERIES_FOR_CLUSTER} needed")
            } else {
                format!(
                    "{c} holds {} rows, below the {MIN_ROWS_FOR_CLUSTER} at which clustering pays",
                    ctx.rows_in(c)
                )
            };
            return Applicability::NotYet(detail);
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let candidates = Self::candidates(ctx);
        let rows: usize = candidates.iter().map(|(c, _)| ctx.rows_in(c)).sum();
        // Placement helps a range read touch fewer pages and nothing else, so
        // the gain is claimed on the share of the workload that ranges. Once
        // range traffic ends the claim goes with it, which is what lets a
        // clustering built for an ended phase be given back.
        let ranges = ctx.workload().range_share();
        CostEstimate::faster(1.0 - 0.5 * ranges, 1.0 - 0.6 * ranges)
            .with_ram(0)
            .with_maintenance(0.05)
            .with_confidence(0.4)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 2e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((collection, field)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        ChangePlan::new(
            vec![Action::SetClusterField {
                collection: collection.clone(),
                field: field.clone(),
            }],
            vec![Action::ClearClusterField { collection }],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((collection, _)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        ChangePlan::new(vec![Action::ClearClusterField { collection }], vec![])
    }
}
