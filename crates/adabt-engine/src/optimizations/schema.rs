//! Optimizations that spend freedom: freezing a collection's schema to the
//! most rigid shape its data supports, and addressing records arithmetically
//! where a fixed schema makes that possible.

use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

// -- schema freezing --------------------------------------------------------

pub struct FreezeSchemaOpt;

const FREEZE_SCHEMA_META: OptMeta = OptMeta {
    name: "freeze_schema",
    summary: "raise a collection's schema to the most rigid its data supports",
    scope_kind: ScopeKind::PerCollection,
    min_level: 8,
    // The largest freedom cost in the library, and the only one that is not
    // notional: a frozen collection rejects records the loose one accepted.
    axis_effects: AxisEffects::new(6, 4, -9),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    // Widening a schema back out is safe, but records written under the narrow
    // one were already constrained by it. That cannot be undone.
    reversibility: Reversibility::Destructive,
    conflicts_with: &[],
};

/// Records that must agree before a shape is called settled.
///
/// Freezing on a handful of records is freezing on a coincidence.
const MIN_ROWS_TO_FREEZE: usize = 1_000;

impl Optimization for FreezeSchemaOpt {
    fn meta(&self) -> &OptMeta {
        &FREEZE_SCHEMA_META
    }

    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        ctx.collections
            .iter()
            .filter(|(c, n)| *n >= MIN_ROWS_TO_FREEZE && !ctx.is_fixed_size(c))
            .map(|(c, _)| c.clone())
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if self.candidate_scopes(ctx).is_empty() {
            return Applicability::NotYet(format!(
                "no collection is both loose and larger than {MIN_ROWS_TO_FREEZE} rows"
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx.collections.iter().map(|(_, n)| n).sum();
        let mut e = CostEstimate::faster(0.7, 0.65);
        // A fixed layout is denser than a tagged one, so this saves as well as
        // speeding up — unusual, and the reason its resource effect is positive.
        e.storage_bytes = -(rows as i64 * 16);
        // Deliberately unconfident: whether the shape is really settled cannot
        // be known from a sample, and being wrong here rejects real data.
        e.with_confidence(0.25).with_build(BuildCost {
            estimated_secs: rows as f64 / 2e5,
            rows_read: rows as u64,
            online: false,
        })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::FreezeSchema {
                collection: scope.to_string(),
            }],
            // No inverse: widening the schema back would not un-constrain the
            // records already written under it.
            vec![],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::default()
    }
}

// -- direct lookup ----------------------------------------------------------

pub struct DirectLookupOpt;

const DIRECT_LOOKUP_META: OptMeta = OptMeta {
    name: "direct_lookup",
    summary: "address records arithmetically where the schema fixes their size",
    scope_kind: ScopeKind::PerCollection,
    min_level: 10,
    // Costs freedom: it only works while the schema stays fixed-width.
    axis_effects: AxisEffects::new(9, -7, -4),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// Below this, the array wastes more memory on empty slots than it saves.
const MIN_DENSITY: f64 = 0.5;

impl Optimization for DirectLookupOpt {
    fn meta(&self) -> &OptMeta {
        &DIRECT_LOOKUP_META
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if ctx.fixed_size_collections.is_empty() {
            // Structural, not temporary: no amount of workload change makes a
            // variable-width schema directly addressable.
            return Applicability::Ineligible(
                "no collection has a fixed-size schema; records must have a constant stride"
                    .to_string(),
            );
        }
        // A flat array spans the whole id range whether or not the slots are
        // used, so a sparse id space turns the optimization into pure waste.
        let sparse: Vec<&String> = ctx
            .fixed_size_collections
            .iter()
            .filter(|c| ctx.density_of(c) < MIN_DENSITY)
            .collect();
        if sparse.len() == ctx.fixed_size_collections.len() {
            let worst = sparse
                .iter()
                .map(|c| (ctx.density_of(c), (*c).clone()))
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .unwrap();
            return Applicability::NotYet(format!(
                "{} ids are only {:.1}% dense, below the {:.0}% at which a flat array pays",
                worst.1,
                worst.0 * 100.0,
                MIN_DENSITY * 100.0
            ));
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let rows: usize = ctx
            .fixed_size_collections
            .iter()
            .map(|c| ctx.rows_in(c))
            .sum();
        // A whole second copy of the data, in exchange for removing the page
        // directory, the slot table and the search from the lookup path.
        CostEstimate::faster(0.4, 0.35)
            .with_ram(rows as i64 * 64)
            .with_maintenance(0.15)
            .with_confidence(0.5)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 2e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(
            vec![Action::SetDirectLookup(true)],
            vec![Action::SetDirectLookup(false)],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, _scope: &str, _params: &Params) -> ChangePlan {
        ChangePlan::new(vec![Action::SetDirectLookup(false)], vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizations::tests::Fx;

    #[test]
    fn direct_lookup_is_structurally_ineligible_without_a_fixed_schema() {
        let fx = Fx::new();
        let a = DirectLookupOpt.applicability(&fx.ctx());
        // Ineligible, not NotYet: no workload change makes this possible.
        assert!(matches!(a, Applicability::Ineligible(_)), "{a:?}");
        assert!(a.reason().unwrap().contains("constant stride"));
    }

    #[test]
    fn direct_lookup_becomes_applicable_with_a_fixed_schema() {
        let mut fx = Fx::new();
        fx.fixed = vec!["users".into()];
        assert!(DirectLookupOpt.applicability(&fx.ctx()).is_applicable());
        let e = DirectLookupOpt.estimate(&fx.ctx());
        assert!(e.helps_latency());
        assert!(e.costs_resources(), "a second copy of the data is not free");
    }

    #[test]
    fn direct_lookup_reports_its_cost_to_freedom() {
        // It only works while the schema stays fixed-width, which is a real
        // constraint on the user, not merely a memory cost.
        const _: () = assert!(DIRECT_LOOKUP_META.axis_effects.freedom < 0);
        const _: () = assert!(DIRECT_LOOKUP_META.axis_effects.speed > 0);
        const _: () = assert!(DIRECT_LOOKUP_META.axis_effects.resources < 0);
    }
}
