//! Optimizations that decide where records sit on pages: clustering by a
//! range-filtered key, and `data_partitioning`, which declares the same
//! clustering for a range-skewed field (plus a flag nothing else reads).

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
        CostEstimate::faster(0.5, 0.4)
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

// -- data-driven partitioning -----------------------------------------------

pub struct DataPartitioningOpt;

const DATA_PARTITIONING_META: OptMeta = OptMeta {
    name: "data_partitioning",
    summary: "partition hot key ranges to separate pages (clustered-sort companion)",
    scope_kind: ScopeKind::PerField,
    min_level: 6,
    axis_effects: AxisEffects::new(4, -1, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::Instant,
};

impl Optimization for DataPartitioningOpt {
    fn meta(&self) -> &OptMeta {
        &DATA_PARTITIONING_META
    }
    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        ctx.telemetry
            .most_range_filtered_fields()
            .into_iter()
            .filter(|(c, _, n)| *n >= 10 && ctx.rows_in(c) >= 5_000)
            .map(|(c, f, _)| format!("{c}.{f}"))
            .collect()
    }
    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if self.candidate_scopes(ctx).is_empty() {
            return Applicability::NotYet("no field shows range-skewed access yet".into());
        }
        Applicability::Applicable
    }
    fn estimate(&self, _ctx: &OptContext<'_>) -> CostEstimate {
        CostEstimate::faster(0.7, 0.65).with_confidence(0.35)
    }
    fn plan_enable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((c, f)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        // Reuse clustered placement — partitioning is its hot-range refinement.
        ChangePlan::new(
            vec![
                Action::SetDataPartitioning(true),
                Action::SetClusterField {
                    collection: c.clone(),
                    field: f.clone(),
                },
            ],
            vec![
                Action::SetDataPartitioning(false),
                Action::ClearClusterField { collection: c },
            ],
        )
    }
    fn plan_disable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((c, _)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        ChangePlan::new(
            vec![
                Action::SetDataPartitioning(false),
                Action::ClearClusterField { collection: c },
            ],
            vec![],
        )
    }
}
