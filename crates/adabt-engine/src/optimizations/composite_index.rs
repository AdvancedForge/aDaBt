//! `auto_composite_index`: one index over a set of fields that queries
//! repeatedly pin to literals together.

use adabt_core::index_kind::IndexKind;
use adabt_core::policy::GuaranteeRequirements;
use adabt_opt::action::{Action, ChangePlan};
use adabt_opt::config::Params;
use adabt_opt::cost::{AxisEffects, BuildCost, CostEstimate};
use adabt_opt::optimization::{
    Applicability, OptContext, OptMeta, Optimization, Reversibility, ScopeKind,
};

use super::auto_index::MIN_ROWS_FOR_INDEX;
use super::split_scope;

pub struct AutoCompositeIndexOpt;

const AUTO_COMPOSITE_INDEX_META: OptMeta = OptMeta {
    name: "auto_composite_index",
    summary: "index field sets that queries repeatedly pin together",
    scope_kind: ScopeKind::PerField,
    // Level 5: "workload-aware". A composite index is chosen from which fields
    // this workload pins together, which is a fact about the traffic rather
    // than the schema. Above `auto_index` because it costs more per write and
    // serves a narrower set of queries.
    min_level: 5,
    axis_effects: AxisEffects::new(8, -4, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// How often a field set must be pinned together before an index for it pays.
///
/// Higher than the single-field threshold on purpose. A composite index serves
/// a narrower range of queries — only those pinning *every* one of its fields
/// — so it needs more evidence that those queries are the ones actually being
/// asked, and it costs more per write than a single-field index does.
const MIN_QUERIES_FOR_COMPOSITE: u64 = 20;

/// Widest composite index worth proposing.
///
/// Each additional field narrows the set of queries the index can serve while
/// adding to what every write costs. Past a few fields the index is nearly a
/// bespoke answer to one query shape, which is a thing an expert may well want
/// and not a thing to infer from traffic.
const MAX_COMPOSITE_FIELDS: usize = 4;

impl AutoCompositeIndexOpt {
    /// Field sets worth a composite index.
    fn candidates(ctx: &OptContext<'_>) -> Vec<(String, Vec<String>)> {
        ctx.telemetry
            .most_pinned_sets()
            .into_iter()
            .filter(|(c, fields, n)| {
                *n >= MIN_QUERIES_FOR_COMPOSITE
                    && fields.len() >= 2
                    && fields.len() <= MAX_COMPOSITE_FIELDS
                    && ctx.rows_in(c) >= MIN_ROWS_FOR_INDEX
                    && !ctx.has_index(c, &adabt_index::composite_name(fields))
            })
            .map(|(c, fields, _)| (c, fields))
            .collect()
    }

    /// The scope name for a field set.
    ///
    /// The fields are joined with the same NUL the index itself is named with,
    /// so the scope round-trips through `split_scope` — which splits on the
    /// first `.` — without a second encoding to keep in step.
    fn scope_of(collection: &str, fields: &[String]) -> String {
        format!("{collection}.{}", adabt_index::composite_name(fields))
    }
}

impl Optimization for AutoCompositeIndexOpt {
    fn meta(&self) -> &OptMeta {
        &AUTO_COMPOSITE_INDEX_META
    }

    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        Self::candidates(ctx)
            .into_iter()
            .map(|(c, fields)| Self::scope_of(&c, &fields))
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let sets = ctx.telemetry.most_pinned_sets();
        if sets.is_empty() {
            return Applicability::NotYet(
                "no query has pinned two fields to literals at once yet".to_string(),
            );
        }
        if Self::candidates(ctx).is_empty() {
            let (c, fields, n) = &sets[0];
            let detail = if ctx.has_index(c, &adabt_index::composite_name(fields)) {
                format!("({}) on {c} is already indexed", fields.join(", "))
            } else if *n < MIN_QUERIES_FOR_COMPOSITE {
                format!(
                    "({}) pinned together only {n} times, below the \
                     {MIN_QUERIES_FOR_COMPOSITE} needed",
                    fields.join(", ")
                )
            } else if fields.len() > MAX_COMPOSITE_FIELDS {
                format!(
                    "({}) is {} fields wide, past the {MAX_COMPOSITE_FIELDS} worth inferring",
                    fields.join(", "),
                    fields.len()
                )
            } else {
                format!(
                    "{c} holds {} rows, below the {MIN_ROWS_FOR_INDEX} at which an index pays",
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
        let widest = candidates.iter().map(|(_, f)| f.len()).max().unwrap_or(2) as i64;
        // Narrows harder than a single-field index, which is the point — but
        // the key is wider and every write maintains all of it, so both the
        // memory and the maintenance charge scale with the field count rather
        // than being the single-field figures again.
        CostEstimate::faster(0.45, 0.35)
            .with_ram(rows as i64 * 48 * widest)
            .with_maintenance(0.10 * widest as f64)
            // Lower than `auto_index`'s. The single-field case has been
            // measured on this engine repeatedly; this one is inferred from a
            // co-occurrence count and has more ways to be wrong, so it should
            // have to prove more in the shadow phase before it is trusted.
            .with_confidence(0.45)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 1e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((collection, joined)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        // `CreateIndex` with a NUL-joined name already builds a composite
        // index — `create_index_from` recognises the separator. No new action
        // is needed, which also means the inverse and the drop path work
        // unchanged rather than needing their own.
        ChangePlan::new(
            vec![Action::CreateIndex {
                collection: collection.clone(),
                field: joined.clone(),
                kind: IndexKind::Hash,
            }],
            vec![Action::DropIndex {
                collection,
                field: joined,
                kind: IndexKind::Hash,
            }],
        )
    }

    fn plan_disable(&self, _ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((collection, joined)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        ChangePlan::new(
            vec![Action::DropIndex {
                collection,
                field: joined,
                kind: IndexKind::Hash,
            }],
            vec![],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizations::tests::Fx;

    // -- composite index selection ------------------------------------------

    /// The structure existed for a long time and nothing chose it, because
    /// nothing recorded which fields queries pin *together*. These tests are
    /// about the signal as much as the decision.
    #[test]
    fn auto_composite_waits_for_fields_to_be_pinned_together() {
        let mut fx = Fx::new();
        fx.collections.push(("users".into(), 10_000));
        // Each field filtered often, never together.
        fx.filtered.push(("users".into(), "country".into(), 100));
        fx.filtered.push(("users".into(), "age".into(), 100));
        let a = AutoCompositeIndexOpt.applicability(&fx.ctx());
        assert!(
            !a.is_applicable(),
            "two individually-hot fields are not evidence for a composite index"
        );
    }

    #[test]
    fn auto_composite_proposes_the_set_that_is_actually_pinned_together() {
        let mut fx = Fx::new();
        fx.collections.push(("users".into(), 10_000));
        fx.pin("users", &["country", "age"], MIN_QUERIES_FOR_COMPOSITE);

        let a = AutoCompositeIndexOpt.applicability(&fx.ctx());
        assert!(a.is_applicable(), "{a:?}");

        let scopes = AutoCompositeIndexOpt.candidate_scopes(&fx.ctx());
        assert_eq!(
            scopes,
            vec![format!(
                "users.{}",
                adabt_index::composite_name(&["age".to_string(), "country".to_string()])
            )],
            "the scope must name the sorted field set"
        );
    }

    #[test]
    fn auto_composite_needs_more_evidence_than_a_single_field_index() {
        let mut fx = Fx::new();
        fx.collections.push(("users".into(), 10_000));
        fx.pin("users", &["country", "age"], MIN_QUERIES_FOR_COMPOSITE - 1);
        assert!(
            !AutoCompositeIndexOpt
                .applicability(&fx.ctx())
                .is_applicable(),
            "one observation below the threshold must not be enough"
        );
    }

    #[test]
    fn auto_composite_plans_a_create_and_its_exact_inverse() {
        let mut fx = Fx::new();
        fx.collections.push(("users".into(), 10_000));
        fx.pin("users", &["country", "age"], MIN_QUERIES_FOR_COMPOSITE);
        let scope = AutoCompositeIndexOpt.candidate_scopes(&fx.ctx())[0].clone();

        let p = AutoCompositeIndexOpt.plan_enable(&fx.ctx(), &scope, &Params::default());
        assert_eq!(p.apply.len(), 1);
        assert_eq!(p.revert.len(), 1);
        match (&p.apply[0], &p.revert[0]) {
            (
                Action::CreateIndex {
                    collection: c1,
                    field: f1,
                    ..
                },
                Action::DropIndex {
                    collection: c2,
                    field: f2,
                    ..
                },
            ) => {
                assert_eq!(c1, "users");
                assert_eq!(c1, c2);
                assert_eq!(f1, f2, "the inverse must drop exactly what was created");
                assert!(
                    f1.contains(adabt_index::COMPOSITE_SEP),
                    "the created index must be named as a composite one, or \
                     `create_index_from` will build a single-field index over \
                     a field no record has"
                );
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn auto_composite_does_not_re_propose_an_existing_index() {
        let mut fx = Fx::new();
        fx.collections.push(("users".into(), 10_000));
        fx.pin("users", &["country", "age"], MIN_QUERIES_FOR_COMPOSITE);
        fx.indexes.push((
            "users".into(),
            adabt_index::composite_name(&["age".to_string(), "country".to_string()]),
            IndexKind::Hash,
        ));
        assert!(!AutoCompositeIndexOpt
            .applicability(&fx.ctx())
            .is_applicable());
    }

    /// Past a few fields a composite index is a bespoke answer to one query
    /// shape. That is a thing an expert may want and not a thing to infer.
    #[test]
    fn auto_composite_refuses_a_set_too_wide_to_infer() {
        let mut fx = Fx::new();
        fx.collections.push(("users".into(), 10_000));
        let wide: Vec<&str> = vec!["a", "b", "c", "d", "e"];
        fx.pin("users", &wide, MIN_QUERIES_FOR_COMPOSITE * 10);
        assert!(!AutoCompositeIndexOpt
            .applicability(&fx.ctx())
            .is_applicable());
    }
}
