//! `auto_index`: a single-field index for each field queries repeatedly filter
//! on, with the index structure (hash or b-tree) chosen from how the field is
//! filtered.
//!
//! Also owns `MIN_ROWS_FOR_INDEX`, the collection size below which no index —
//! single-field, composite or covering — pays for itself.

use adabt_core::index_kind::IndexKind;
use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

use super::split_scope;

pub struct AutoIndexOpt;

const AUTO_INDEX_META: OptMeta = OptMeta {
    name: "auto_index",
    summary: "index fields that queries repeatedly filter on",
    scope_kind: ScopeKind::PerField,
    min_level: 2,
    axis_effects: AxisEffects::new(7, -3, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    // The index has to be rebuilt from the primary to come back.
    reversibility: Reversibility::RebuildRequired,
};

pub(super) const MIN_ROWS_FOR_INDEX: usize = 1_000;
const MIN_QUERIES_FOR_INDEX: u64 = 8;

impl AutoIndexOpt {
    /// Fields worth indexing: filtered often enough, on a collection big enough
    /// that a scan actually costs something, and not already indexed.
    fn candidates(ctx: &OptContext<'_>) -> Vec<(String, String)> {
        ctx.filtered_fields
            .iter()
            .filter(|(c, f, n)| {
                *n >= MIN_QUERIES_FOR_INDEX
                    && ctx.rows_in(c) >= MIN_ROWS_FOR_INDEX
                    && !ctx.has_index(c, f)
            })
            .map(|(c, f, _)| (c.clone(), f.clone()))
            .collect()
    }
}

impl Optimization for AutoIndexOpt {
    fn meta(&self) -> &OptMeta {
        &AUTO_INDEX_META
    }

    /// One scope per field worth indexing, so each index is judged, applied and
    /// retracted on its own evidence.
    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        Self::candidates(ctx)
            .into_iter()
            .map(|(c, f)| format!("{c}.{f}"))
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        if ctx.filtered_fields.is_empty() {
            return Applicability::NotYet("no queries have filtered on any field yet".to_string());
        }
        if Self::candidates(ctx).is_empty() {
            let best = ctx.filtered_fields.iter().max_by_key(|(_, _, n)| *n);
            let detail = match best {
                Some((c, f, n)) if ctx.has_index(c, f) => {
                    format!("{c}.{f} is already indexed")
                }
                Some((c, f, n)) if *n < MIN_QUERIES_FOR_INDEX => format!(
                    "{c}.{f} filtered only {n} times, below the {MIN_QUERIES_FOR_INDEX} needed"
                ),
                Some((c, _, _)) => format!(
                    "{c} holds {} rows, below the {MIN_ROWS_FOR_INDEX} at which an index pays",
                    ctx.rows_in(c)
                ),
                None => "no candidate fields".to_string(),
            };
            return Applicability::NotYet(detail);
        }
        Applicability::Applicable
    }

    fn estimate(&self, ctx: &OptContext<'_>) -> CostEstimate {
        let candidates = Self::candidates(ctx);
        let rows: usize = candidates.iter().map(|(c, _)| ctx.rows_in(c)).sum();
        CostEstimate::faster(0.3, 0.25)
            // Roughly a key plus an id plus node overhead per row.
            .with_ram(rows as i64 * 64)
            .with_maintenance(0.10)
            .with_confidence(0.6)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 1e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, ctx: &OptContext<'_>, scope: &str, params: &Params) -> ChangePlan {
        // One index, for the scope being decided. A plan covering every
        // candidate would make the whole set stand or fall together.
        let Some((collection, field)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        // An explicit `kind` param — set by a manual override that named one
        // ("index users.country hash") — wins over the telemetry-derived
        // guess; absent, the field is chosen from how it is actually
        // filtered, exactly as before this existed.
        let kind =
            index_kind_param(params).unwrap_or_else(|| index_kind_for(ctx, &collection, &field));
        ChangePlan::new(
            vec![Action::CreateIndex {
                collection: collection.clone(),
                field: field.clone(),
                kind,
            }],
            vec![Action::DropIndex {
                collection,
                field,
                kind,
            }],
        )
    }

    fn plan_disable(&self, ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        // Drop only this scope's index. Dropping every index because one
        // stopped paying was an earlier driver's behaviour, and it threw away
        // the ones that were.
        let Some((collection, field)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        let kind = ctx
            .existing_indexes
            .iter()
            .find(|(c, f, _)| *c == collection && *f == field)
            .map(|(_, _, k)| *k)
            .unwrap_or_else(|| index_kind_for(ctx, &collection, &field));
        ChangePlan::new(
            vec![Action::DropIndex {
                collection,
                field,
                kind,
            }],
            vec![],
        )
    }
}

/// An explicit `kind` param, if a decision's `params` carries one.
///
/// The encoding is `Params`'s own constraint, not a design choice: `Params`
/// is `BTreeMap<String, i64>`, so `IndexKind` — which already exists to be
/// *named* without depending on what builds one, see `adabt_core::index_kind`
/// — is carried as its ordinal rather than its `as_str()`/`parse()` string
/// form. `Params` staying integer-only is deliberately not widened for this:
/// every other optimization's params are already numeric, and a single
/// string-valued key would mean every reader of `Params` has to handle a
/// case that means something only here.
fn index_kind_param(params: &Params) -> Option<IndexKind> {
    params.get("kind").and_then(IndexKind::from_ordinal)
}

/// The index structure a field's predicates can actually use.
///
/// A hash index cannot answer a range at all, so proposing one for a
/// range-filtered field builds something the planner will never choose — which
/// is exactly the pure loss the level-matrix benchmark measured.
fn index_kind_for(ctx: &OptContext<'_>, collection: &str, field: &str) -> IndexKind {
    const MOSTLY_EQUALITY: f64 = 0.8;
    match ctx.telemetry.equality_fraction(collection, field) {
        Some(f) if f >= MOSTLY_EQUALITY => IndexKind::Hash,
        // Mixed or range-dominated: an ordered index serves both, at a higher
        // maintenance cost than a hash index would have been.
        Some(_) => IndexKind::BTree,
        None => IndexKind::Hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizations::tests::Fx;

    #[test]
    fn auto_index_waits_until_there_is_evidence() {
        let mut fx = Fx::new();
        assert!(matches!(
            AutoIndexOpt.applicability(&fx.ctx()),
            Applicability::NotYet(_)
        ));

        // Filtered, but not often enough.
        fx.filtered = vec![("users".into(), "country".into(), 3)];
        let a = AutoIndexOpt.applicability(&fx.ctx());
        assert!(
            a.reason().unwrap().contains("filtered only 3 times"),
            "{a:?}"
        );

        // Now often enough.
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        assert!(AutoIndexOpt.applicability(&fx.ctx()).is_applicable());
    }

    #[test]
    fn auto_index_ignores_a_collection_too_small_to_benefit() {
        let mut fx = Fx::new();
        fx.collections = vec![("users".into(), 10)];
        fx.filtered = vec![("users".into(), "country".into(), 500)];
        let a = AutoIndexOpt.applicability(&fx.ctx());
        assert!(a.reason().unwrap().contains("below the 1000"), "{a:?}");
    }

    #[test]
    fn auto_index_does_not_re_propose_an_existing_index() {
        let mut fx = Fx::new();
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        fx.indexes = vec![("users".into(), "country".into(), IndexKind::Hash)];
        let a = AutoIndexOpt.applicability(&fx.ctx());
        assert!(a.reason().unwrap().contains("already indexed"), "{a:?}");
    }

    #[test]
    fn auto_index_plans_a_create_and_its_exact_inverse() {
        let mut fx = Fx::new();
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let p = AutoIndexOpt.plan_enable(&fx.ctx(), "users.country", &Params::default());
        assert_eq!(p.apply.len(), 1);
        assert_eq!(p.revert.len(), 1);
        assert!(p.describe().contains("create hash index on users.country"));
        assert!(matches!(p.revert[0], Action::DropIndex { .. }));
    }

    #[test]
    fn auto_index_honors_an_explicit_kind_param_over_the_telemetry_guess() {
        let mut fx = Fx::new();
        // Overwhelmingly equality, which alone would auto-detect Hash.
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let forced_btree = Params::default().with("kind", IndexKind::BTree.as_ordinal());
        let p = AutoIndexOpt.plan_enable(&fx.ctx(), "users.country", &forced_btree);
        assert!(
            p.describe().contains("create btree index on users.country"),
            "{}",
            p.describe()
        );
    }

    #[test]
    fn an_unrecognized_kind_param_falls_back_to_the_telemetry_guess() {
        let mut fx = Fx::new();
        fx.filtered = vec![("users".into(), "country".into(), 50)];
        let garbage = Params::default().with("kind", 99);
        let p = AutoIndexOpt.plan_enable(&fx.ctx(), "users.country", &garbage);
        assert!(p.describe().contains("create hash index on users.country"));
    }
}
