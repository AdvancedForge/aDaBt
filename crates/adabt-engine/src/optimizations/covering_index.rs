//! `auto_covering_index`: an index on a filtered field that also carries the
//! fields those queries usually project, so they are answered without
//! fetching the record.

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

pub struct AutoCoveringIndexOpt;

const AUTO_COVERING_INDEX_META: OptMeta = OptMeta {
    name: "auto_covering_index",
    summary: "carry each filtered field's usual projection inside its index",
    scope_kind: ScopeKind::PerField,
    // Level 5, beside `auto_composite_index`: both are chosen from what the
    // traffic does rather than from the schema. A covering index costs more
    // per write than either a plain or a composite index — it stores a
    // projection beside every key — and serves only queries whose projection
    // it happens to carry.
    min_level: 5,
    axis_effects: AxisEffects::new(8, -4, 0),
    requires_guarantees: GuaranteeRequirements::ANY,
    prerequisites: &[],
    conflicts_with: &[],
    reversibility: Reversibility::RebuildRequired,
};

/// How often a filtered field must be seen with the same projection before a
/// covering index for that pair pays.
///
/// Same reasoning as `MIN_QUERIES_FOR_COMPOSITE`, one notch stricter in
/// effect: a wrong composite index still narrows a query; a wrong covering
/// index is dead weight on every write and answers nothing.
const MIN_QUERIES_FOR_COVERING: u64 = 20;

/// Widest projection worth carrying.
///
/// Every covered field is stored beside every key. Past a few, the index
/// approaches a second copy of the collection sorted differently — which is
/// a thing an expert may choose deliberately and not a thing to infer from
/// traffic.
const MAX_COVER_FIELDS: usize = 4;

impl AutoCoveringIndexOpt {
    /// (collection, filtered field, covers, equality) worth a covering index.
    ///
    /// The flag rides along because it picks the backing structure: equality
    /// wants hash, range wants b-tree. A proposal that ignored the distinction
    /// would build indexes its own queries cannot use.
    fn candidates(ctx: &OptContext<'_>) -> Vec<(String, String, Vec<String>, bool)> {
        ctx.telemetry
            .most_projected_covers()
            .into_iter()
            .filter(|(c, f, covers, _equality, n)| {
                *n >= MIN_QUERIES_FOR_COVERING
                    && !covers.is_empty()
                    && covers.len() <= MAX_COVER_FIELDS
                    && ctx.rows_in(c) >= MIN_ROWS_FOR_INDEX
                    && !ctx.has_index(c, &Self::index_name(f, covers))
            })
            .map(|(c, f, covers, equality, _)| (c, f, covers, equality))
            .collect()
    }

    /// The canonical name of the index this proposal builds.
    ///
    /// A covering index ALWAYS carries its own key among its covered fields —
    /// a correctness requirement, because the plan re-evaluates the whole
    /// predicate against the row the index produces, and a row missing the
    /// filtered field evaluates to Unknown. `create_covering_index` adds the
    /// field before naming; this must produce byte-identical names or two
    /// paths build two indexes for one idea, and the existence check above
    /// would never see the second.
    fn index_name(field: &str, covers: &[String]) -> String {
        let mut all: Vec<String> = covers.to_vec();
        all.push(field.to_string());
        all.sort();
        all.dedup();
        adabt_index::covering_name(field, &all)
    }

    /// The scope name for one proposal: the canonical index name, so
    /// `split_scope` and `create_index_from` stay in step with it.
    fn scope_of(collection: &str, field: &str, covers: &[String]) -> String {
        format!("{collection}.{}", Self::index_name(field, covers))
    }

    /// Hash or b-tree, for the index named by one proposal's encoded scope.
    ///
    /// The backing structure follows the evidence: the field these queries
    /// filtered by equality wants a hash-backed covering index; one they
    /// range-filtered wants a b-tree. The name does not encode the kind —
    /// the action carries it — so both readings of the same evidence land
    /// on the same index name and the existence check stays honest.
    fn kind_of(ctx: &OptContext<'_>, collection: &str, joined: &str) -> IndexKind {
        let equality = ctx
            .telemetry
            .most_projected_covers()
            .into_iter()
            .filter(|(c, f, _, _, _)| *c == collection && covering_parts_field(joined) == *f)
            .map(|(_, _, _, e, n)| (e, n))
            .max_by_key(|(_, n)| *n)
            .map(|(e, _)| e)
            .unwrap_or(true);
        if equality {
            IndexKind::Hash
        } else {
            IndexKind::BTree
        }
    }
}

impl Optimization for AutoCoveringIndexOpt {
    fn meta(&self) -> &OptMeta {
        &AUTO_COVERING_INDEX_META
    }

    fn candidate_scopes(&self, ctx: &OptContext<'_>) -> Vec<String> {
        Self::candidates(ctx)
            .into_iter()
            .map(|(c, f, covers, _)| Self::scope_of(&c, &f, &covers))
            .collect()
    }

    fn applicability(&self, ctx: &OptContext<'_>) -> Applicability {
        let sets = ctx.telemetry.most_projected_covers();
        if sets.is_empty() {
            return Applicability::NotYet(
                "no query has filtered a field while projecting others yet".to_string(),
            );
        }
        if Self::candidates(ctx).is_empty() {
            let (c, f, covers, _e, n) = &sets[0];
            let detail = if ctx.has_index(c, &Self::index_name(f, covers)) {
                format!("{f} on {c} already carries {}", covers.join(", "))
            } else if *n < MIN_QUERIES_FOR_COVERING {
                format!(
                    "{f} filtered with this projection only {n} times, below \
                     the {MIN_QUERIES_FOR_COVERING} needed"
                )
            } else if covers.len() > MAX_COVER_FIELDS {
                format!(
                    "{} fields wide, past the {MAX_COVER_FIELDS} worth inferring",
                    covers.len()
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
        let rows: usize = candidates.iter().map(|(c, _, _, _)| ctx.rows_in(c)).sum();
        let widest = candidates
            .iter()
            .map(|(_, _, c, _)| c.len() + 1)
            .max()
            .unwrap_or(2) as i64;
        // The win it offers is larger than a plain index's — served queries
        // skip their fetches entirely — but it only pays for queries whose
        // projection matches, so the confidence stays at the inferred-from-
        //-traffic tier rather than the measured one.
        CostEstimate::faster(0.35, 0.25)
            .with_ram(rows as i64 * 48 * widest)
            .with_maintenance(0.10 * widest as f64)
            .with_confidence(0.45)
            .with_build(BuildCost {
                estimated_secs: rows as f64 / 1e6,
                rows_read: rows as u64,
                online: true,
            })
    }

    fn plan_enable(&self, ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((collection, joined)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        let kind = Self::kind_of(ctx, &collection, &joined);
        // A CreateIndex carrying the `\u{1}`-separated covering name builds a
        // covering index — `create_index_from` recognises the separator and
        // rebuilds the projection from the heap. No new action variant, so
        // persistence, the inverse and the drop path all work unchanged.
        ChangePlan::new(
            vec![Action::CreateIndex {
                collection: collection.clone(),
                field: joined.clone(),
                kind,
            }],
            vec![Action::DropIndex {
                collection,
                field: joined,
                kind,
            }],
        )
    }

    fn plan_disable(&self, ctx: &OptContext<'_>, scope: &str, _params: &Params) -> ChangePlan {
        let Some((collection, joined)) = split_scope(scope) else {
            return ChangePlan::default();
        };
        let kind = Self::kind_of(ctx, &collection, &joined);
        ChangePlan::new(
            vec![Action::DropIndex {
                collection,
                field: joined,
                kind,
            }],
            vec![],
        )
    }
}

/// The base field of a covering index's encoded scope name.
fn covering_parts_field(joined: &str) -> String {
    adabt_index::covering_parts(joined).0
}
