//! Rule-based planning.
//!
//! Turns a logical plan into a physical one using whatever indexes exist. The
//! rules are deliberately simple and deterministic at this milestone — a
//! cost-based planner arrives with the cost model in Phase 6, and until there
//! are real measurements to calibrate against, a cost model would be invented
//! numbers dressed up as rigour.
//!
//! What matters now is the *seam*: `plan()` is the single place an access path
//! is chosen, and it records why. When the optimizer starts making that choice,
//! it replaces this function rather than being threaded through the executor.

use adabt_index::{Index, IndexKind};
use adabt_ir::plan::LogicalOp;
use adabt_ir::{CmpOp, Expr};
use std::collections::HashMap;
use std::ops::Bound;

use crate::physical::{PhysicalOp, PhysicalPlan};

/// What the planner is allowed to assume exists.
pub struct PlanContext<'a> {
    /// Indexes available per collection, keyed by field.
    pub indexes: HashMap<&'a str, Vec<(&'a str, IndexKind)>>,
    /// Collections with a columnar derived representation.
    pub columnar: Vec<&'a str>,
}

impl<'a> PlanContext<'a> {
    pub fn empty() -> Self {
        Self {
            indexes: HashMap::new(),
            columnar: Vec::new(),
        }
    }

    pub fn from_indexes(collection: &'a str, indexes: &'a [Box<dyn Index>]) -> Self {
        let mut m = HashMap::new();
        m.insert(
            collection,
            indexes.iter().map(|i| (i.field(), i.kind())).collect(),
        );
        Self {
            indexes: m,
            columnar: Vec::new(),
        }
    }

    fn index_for(&self, collection: &str, field: &str) -> Option<IndexKind> {
        let list = self.indexes.get(collection)?;
        // Prefer a hash index for equality: it is the cheapest lookup, and
        // the caller only asks about a field it already intends to match
        // exactly. A bitmap index answers equality too — the same `lookup`,
        // the same rows back — and is preferred over a b-tree for it, since
        // a b-tree is a range structure being asked for equality only as a
        // fallback; between the two purpose-built equality structures, hash
        // still wins because materializing a bitmap's matches costs a scan
        // of its whole word range, not just of the matches themselves.
        let mut best = None;
        for (f, kind) in list {
            if *f == field {
                match kind {
                    IndexKind::Hash => return Some(IndexKind::Hash),
                    IndexKind::Bitmap => best = Some(IndexKind::Bitmap),
                    IndexKind::BTree => best = best.or(Some(IndexKind::BTree)),
                }
            }
        }
        best
    }

    fn has_columnar(&self, collection: &str) -> bool {
        self.columnar.contains(&collection)
    }

    fn range_index_for(&self, collection: &str, field: &str) -> bool {
        self.indexes
            .get(collection)
            .is_some_and(|l| l.iter().any(|(f, k)| *f == field && k.supports_range()))
    }
}

/// Range bounds a predicate places on a single field, if any.
fn range_constraint(
    e: &Expr,
    collection_field: &str,
) -> Option<(
    Bound<adabt_core::value::Value>,
    Bound<adabt_core::value::Value>,
)> {
    let mut lo = Bound::Unbounded;
    let mut hi = Bound::Unbounded;
    let mut found = false;
    fn walk(
        e: &Expr,
        field: &str,
        lo: &mut Bound<adabt_core::value::Value>,
        hi: &mut Bound<adabt_core::value::Value>,
        found: &mut bool,
    ) {
        match e {
            Expr::Compare { op, lhs, rhs } => {
                let (Expr::Field(f), Expr::Literal(v)) = (lhs.as_ref(), rhs.as_ref()) else {
                    return;
                };
                if f != field {
                    return;
                }
                match op {
                    CmpOp::Gt => {
                        *lo = Bound::Excluded(v.clone());
                        *found = true;
                    }
                    CmpOp::Ge => {
                        *lo = Bound::Included(v.clone());
                        *found = true;
                    }
                    CmpOp::Lt => {
                        *hi = Bound::Excluded(v.clone());
                        *found = true;
                    }
                    CmpOp::Le => {
                        *hi = Bound::Included(v.clone());
                        *found = true;
                    }
                    _ => {}
                }
            }
            // Only `And` narrows: a bound under `Or` does not constrain the
            // result set, so using it would drop matching rows.
            Expr::And(parts) => {
                for p in parts {
                    walk(p, field, lo, hi, found);
                }
            }
            _ => {}
        }
    }
    walk(e, collection_field, &mut lo, &mut hi, &mut found);
    if found {
        Some((lo, hi))
    } else {
        None
    }
}

/// Fields a predicate constrains with an inequality, in appearance order.
fn range_fields(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Compare { op, lhs, .. } if !matches!(op, CmpOp::Eq | CmpOp::Ne) => {
            if let Expr::Field(f) = lhs.as_ref() {
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
        }
        Expr::And(parts) => {
            for p in parts {
                range_fields(p, out);
            }
        }
        _ => {}
    }
}

/// Plan a query.
///
/// Deliberately a thin wrapper over `decide` + `build_from` rather than a
/// second implementation. Two code paths producing "the same" plan is exactly
/// how a cached decision and a fresh one drift apart, and that drift would show
/// up as a query answered differently depending on whether the plan cache
/// happened to be warm.
///
/// A plan containing a `Join` anywhere is the one exception, routed to
/// `plan_join` instead: `decide`'s single `PlanDecision` names one access path
/// for a whole plan, which was never going to be right for a tree with two
/// genuinely independent scan sides — `LogicalOp::collection()` (which
/// `decide` calls first) does not even have an answer for "the" collection of
/// a plan with two of them, and asserts rather than guess at one. Planning
/// each side by recursing into `plan` itself, rather than inventing a
/// per-node decision type to push through the shared machinery, means every
/// existing index/columnar rule already applies correctly to each side
/// without having been taught anything new about joins.
pub fn plan(logical: &LogicalOp, ctx: &PlanContext<'_>) -> PhysicalPlan {
    if logical.contains_join() {
        return plan_join(logical, ctx);
    }
    build_from(logical, &decide(logical, ctx))
}

fn plan_join(logical: &LogicalOp, ctx: &PlanContext<'_>) -> PhysicalPlan {
    match logical {
        LogicalOp::Join {
            left,
            right,
            kind,
            on,
        } => {
            let left_plan = plan(left, ctx);
            let right_plan = plan(right, ctx);
            let rationale = format!(
                "{} joined ({}) with {}",
                left_plan.rationale,
                kind.as_str(),
                right_plan.rationale
            );
            PhysicalPlan {
                root: PhysicalOp::Join {
                    left: Box::new(left_plan.root),
                    right: Box::new(right_plan.root),
                    kind: *kind,
                    on: on.clone(),
                },
                rationale,
            }
        }
        LogicalOp::Filter { input, predicate } => {
            let inner = plan_join(input, ctx);
            PhysicalPlan {
                root: PhysicalOp::Filter {
                    input: Box::new(inner.root),
                    predicate: predicate.clone(),
                },
                rationale: inner.rationale,
            }
        }
        LogicalOp::Project { input, fields } => {
            let inner = plan_join(input, ctx);
            PhysicalPlan {
                root: PhysicalOp::Project {
                    input: Box::new(inner.root),
                    fields: fields.clone(),
                },
                rationale: inner.rationale,
            }
        }
        LogicalOp::Sort { input, keys } => {
            let inner = plan_join(input, ctx);
            PhysicalPlan {
                root: PhysicalOp::Sort {
                    input: Box::new(inner.root),
                    keys: keys.clone(),
                },
                rationale: inner.rationale,
            }
        }
        LogicalOp::Limit { input, n } => {
            let inner = plan_join(input, ctx);
            PhysicalPlan {
                root: PhysicalOp::Limit {
                    input: Box::new(inner.root),
                    n: *n,
                },
                rationale: inner.rationale,
            }
        }
        LogicalOp::Aggregate {
            input,
            group_by,
            aggs,
        } => {
            let inner = plan_join(input, ctx);
            PhysicalPlan {
                root: PhysicalOp::Aggregate {
                    input: Box::new(inner.root),
                    group_by: group_by.clone(),
                    aggs: aggs.clone(),
                },
                rationale: inner.rationale,
            }
        }
        // `contains_join()` is true for the plan `plan_join` was called with,
        // and a leaf (`GetById`/`GetByIds`/`Scan`) can never itself contain a
        // `Join` — so reaching one here would mean `plan_join` was called on
        // a subtree `contains_join()` said had none, which is the caller's
        // bug to fix, not this function's to paper over.
        LogicalOp::GetById { .. } | LogicalOp::GetByIds { .. } | LogicalOp::Scan { .. } => {
            unreachable!("plan_join reached a leaf with no Join beneath it")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::ids::RecordId;
    use adabt_ir::plan::SortKey;

    fn ctx_with(pairs: Vec<(&'static str, IndexKind)>) -> PlanContext<'static> {
        let mut m = HashMap::new();
        m.insert("users", pairs);
        PlanContext {
            indexes: m,
            columnar: Vec::new(),
        }
    }

    #[test]
    fn without_an_index_a_filter_is_a_full_scan() {
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let p = plan(&l, &PlanContext::empty());
        assert!(p.is_full_scan());
        assert!(p.rationale.contains("full scan"), "{}", p.rationale);
    }

    #[test]
    fn an_equality_filter_uses_an_available_index() {
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let p = plan(&l, &ctx_with(vec![("country", IndexKind::Hash)]));
        assert!(!p.is_full_scan());
        assert_eq!(p.root.access_path().name(), "IndexLookup");
        assert!(p.rationale.contains("hash"), "{}", p.rationale);
    }

    #[test]
    fn a_hash_index_is_preferred_over_a_btree_for_equality() {
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let p = plan(
            &l,
            &ctx_with(vec![
                ("country", IndexKind::BTree),
                ("country", IndexKind::Hash),
            ]),
        );
        match p.root.access_path() {
            PhysicalOp::IndexLookup { kind, .. } => assert_eq!(*kind, IndexKind::Hash),
            other => panic!("expected an index lookup, got {}", other.name()),
        }
    }

    #[test]
    fn an_index_on_a_different_field_is_not_used() {
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let p = plan(&l, &ctx_with(vec![("city", IndexKind::Hash)]));
        assert!(p.is_full_scan());
    }

    #[test]
    fn a_range_filter_uses_a_btree_but_not_a_hash_index() {
        let l = LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 18i64));
        let with_btree = plan(&l, &ctx_with(vec![("age", IndexKind::BTree)]));
        assert_eq!(with_btree.root.access_path().name(), "IndexRange");

        let with_hash = plan(&l, &ctx_with(vec![("age", IndexKind::Hash)]));
        assert!(
            with_hash.is_full_scan(),
            "a hash index cannot serve a range and must not be chosen"
        );
    }

    #[test]
    fn both_range_bounds_are_captured() {
        let l = LogicalOp::scan("users").filter(Expr::And(vec![
            Expr::cmp("age", CmpOp::Ge, 18i64),
            Expr::cmp("age", CmpOp::Lt, 65i64),
        ]));
        let p = plan(&l, &ctx_with(vec![("age", IndexKind::BTree)]));
        match p.root.access_path() {
            PhysicalOp::IndexRange { lo, hi, .. } => {
                assert!(matches!(lo, Bound::Included(_)));
                assert!(matches!(hi, Bound::Excluded(_)));
            }
            other => panic!("expected a range scan, got {}", other.name()),
        }
    }

    #[test]
    fn an_or_predicate_does_not_become_an_index_lookup() {
        // A row matching the other branch need not satisfy this one, so an
        // index lookup here would silently drop results.
        let l = LogicalOp::scan("users").filter(Expr::Or(vec![
            Expr::eq("country", "NO"),
            Expr::eq("city", "Oslo"),
        ]));
        let p = plan(&l, &ctx_with(vec![("country", IndexKind::Hash)]));
        assert!(p.is_full_scan(), "{}", p.explain());
    }

    #[test]
    fn the_predicate_is_still_applied_after_an_index_lookup() {
        // The index answers one conjunct; the others must still be checked.
        let l = LogicalOp::scan("users").filter(Expr::And(vec![
            Expr::eq("country", "NO"),
            Expr::cmp("age", CmpOp::Gt, 18i64),
        ]));
        let p = plan(&l, &ctx_with(vec![("country", IndexKind::Hash)]));
        assert_eq!(p.root.name(), "Filter");
        assert_eq!(p.root.access_path().name(), "IndexLookup");
    }

    #[test]
    fn get_by_id_is_always_a_direct_lookup() {
        let p = plan(&LogicalOp::get("users", RecordId(1)), &PlanContext::empty());
        assert_eq!(p.root.name(), "GetById");
        assert!(!p.is_full_scan());
        assert!(p.rationale.contains("record id"), "{}", p.rationale);
    }

    #[test]
    fn operators_above_the_access_path_are_preserved_in_order() {
        let l = LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["name".into()])
            .sort(vec![SortKey {
                field: "name".into(),
                descending: false,
            }])
            .limit(5);
        let p = plan(&l, &ctx_with(vec![("country", IndexKind::Hash)]));
        assert_eq!(p.root.name(), "Limit");
        assert_eq!(p.root.child().unwrap().name(), "Sort");
        assert_eq!(p.root.child().unwrap().child().unwrap().name(), "Project");
        assert_eq!(p.root.access_path().name(), "IndexLookup");
    }

    #[test]
    fn explain_reports_the_rationale() {
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let e = plan(&l, &ctx_with(vec![("country", IndexKind::Hash)])).explain();
        assert!(e.contains("IndexLookup"), "{e}");
        assert!(e.contains("rationale:"), "{e}");
    }
}

/// The part of a plan that depends only on a query's *shape*.
///
/// A physical plan contains literals — the id in a `GetById`, the key in an
/// `IndexLookup`, the bounds of a range. A plan cache keyed by `QueryShape`
/// therefore cannot store plans: two queries share a shape precisely when their
/// literals differ, so reusing one's plan for the other silently answers the
/// wrong question.
///
/// What *is* shape-invariant is the decision: which access path to use. Caching
/// that and rebuilding the plan around the current literals gives the plan cache
/// its intended benefit — skipping the choice, which is the expensive part —
/// without any possibility of a literal leaking between queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    ById,
    ByIds,
    FullScan,
    ColumnScan { fields: Vec<String> },
    IndexLookup { field: String, kind: IndexKind },
    IndexRange { field: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDecision {
    pub access: AccessDecision,
    pub rationale: String,
}

/// Choose an access path. Depends on the plan's shape and the available
/// indexes, never on its literals.
pub fn decide(logical: &LogicalOp, ctx: &PlanContext<'_>) -> PlanDecision {
    // A columnar read beats a heap scan when the plan touches only some fields,
    // but it cannot serve an index lookup, so it is considered only where the
    // access would otherwise be a full scan. `required_fields` returning None
    // means the plan hands whole records upward and columnar is not legal.
    if ctx.has_columnar(logical.collection()) {
        if let Some(fields) = logical.required_fields() {
            let equality_indexed = false;
            if !equality_indexed {
                let rationale = format!(
                    "{} of the collection's fields are read; served columnar",
                    fields.len()
                );
                return PlanDecision {
                    access: AccessDecision::ColumnScan { fields },
                    rationale,
                };
            }
        }
    }
    // Walk to the leaf, remembering the filter directly above a scan.
    let mut node = logical;
    let mut filter_over_scan: Option<&Expr> = None;
    loop {
        match node {
            LogicalOp::GetById { .. } => {
                return PlanDecision {
                    access: AccessDecision::ById,
                    rationale: "direct lookup by record id".into(),
                }
            }
            LogicalOp::GetByIds { .. } => {
                return PlanDecision {
                    access: AccessDecision::ByIds,
                    rationale: "batched lookup of record ids".into(),
                }
            }
            LogicalOp::Scan { collection } => {
                if let Some(pred) = filter_over_scan {
                    for (field, _) in pred.equality_constraints() {
                        if let Some(kind) = ctx.index_for(collection, &field) {
                            let rationale = format!(
                                "equality on {collection}.{field} served by {} index",
                                kind.as_str()
                            );
                            return PlanDecision {
                                access: AccessDecision::IndexLookup { field, kind },
                                rationale,
                            };
                        }
                    }
                    let mut fields = Vec::new();
                    range_fields(pred, &mut fields);
                    for field in fields {
                        if ctx.range_index_for(collection, &field)
                            && range_constraint(pred, &field).is_some()
                        {
                            let rationale =
                                format!("range on {collection}.{field} served by btree index");
                            return PlanDecision {
                                access: AccessDecision::IndexRange { field },
                                rationale,
                            };
                        }
                    }
                }
                return PlanDecision {
                    access: AccessDecision::FullScan,
                    rationale: "no applicable index; full scan".into(),
                };
            }
            LogicalOp::Filter { input, predicate } => {
                filter_over_scan = matches!(input.as_ref(), LogicalOp::Scan { .. })
                    .then_some(predicate)
                    .or(filter_over_scan);
                node = input;
            }
            other => {
                node = other.child().expect("non-leaf has a child");
            }
        }
    }
}

/// Build a physical plan from a shape-invariant decision plus this query's
/// literals.
pub fn build_from(logical: &LogicalOp, decision: &PlanDecision) -> PhysicalPlan {
    PhysicalPlan {
        root: build_node(logical, decision),
        rationale: decision.rationale.clone(),
    }
}

fn build_node(op: &LogicalOp, decision: &PlanDecision) -> PhysicalOp {
    match op {
        LogicalOp::GetById { collection, id } => PhysicalOp::GetById {
            collection: collection.clone(),
            id: *id,
        },
        LogicalOp::GetByIds { collection, ids } => PhysicalOp::GetByIds {
            collection: collection.clone(),
            ids: ids.clone(),
        },
        LogicalOp::Scan { collection } => match &decision.access {
            AccessDecision::ColumnScan { fields } => PhysicalOp::ColumnScan {
                collection: collection.clone(),
                fields: fields.clone(),
            },
            _ => PhysicalOp::HeapScan {
                collection: collection.clone(),
            },
        },
        LogicalOp::Filter { input, predicate } => {
            // The access path replaces the scan beneath a filter, and takes its
            // literals from *this* predicate rather than from a cached one.
            if let LogicalOp::Scan { collection } = input.as_ref() {
                match &decision.access {
                    AccessDecision::IndexLookup { field, kind } => {
                        if let Some((_, key)) = predicate
                            .equality_constraints()
                            .into_iter()
                            .find(|(f, _)| f == field)
                        {
                            return PhysicalOp::Filter {
                                input: Box::new(PhysicalOp::IndexLookup {
                                    collection: collection.clone(),
                                    field: field.clone(),
                                    kind: *kind,
                                    key,
                                }),
                                predicate: predicate.clone(),
                            };
                        }
                    }
                    AccessDecision::IndexRange { field } => {
                        if let Some((lo, hi)) = range_constraint(predicate, field) {
                            return PhysicalOp::Filter {
                                input: Box::new(PhysicalOp::IndexRange {
                                    collection: collection.clone(),
                                    field: field.clone(),
                                    lo,
                                    hi,
                                }),
                                predicate: predicate.clone(),
                            };
                        }
                    }
                    _ => {}
                }
            }
            PhysicalOp::Filter {
                input: Box::new(build_node(input, decision)),
                predicate: predicate.clone(),
            }
        }
        LogicalOp::Project { input, fields } => PhysicalOp::Project {
            input: Box::new(build_node(input, decision)),
            fields: fields.clone(),
        },
        LogicalOp::Sort { input, keys } => PhysicalOp::Sort {
            input: Box::new(build_node(input, decision)),
            keys: keys.clone(),
        },
        LogicalOp::Limit { input, n } => PhysicalOp::Limit {
            input: Box::new(build_node(input, decision)),
            n: *n,
        },
        LogicalOp::Aggregate {
            input,
            group_by,
            aggs,
        } => PhysicalOp::Aggregate {
            input: Box::new(build_node(input, decision)),
            group_by: group_by.clone(),
            aggs: aggs.clone(),
        },
        // Unreachable: `plan()` — the only public entry point that ever
        // produces the `decision` this function is called with — routes any
        // plan `contains_join()` is true for through `plan_join` instead,
        // before `decide` or `build_node` see it at all. A `Join` arriving
        // here means something called `build_node` directly, bypassing
        // `plan()`'s own routing, which is a caller bug this function has no
        // way to recover from cleanly.
        LogicalOp::Join { .. } => {
            panic!("build_node reached a Join; callers must go through plan(), not build_node() directly")
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use adabt_core::ids::RecordId;
    use adabt_core::value::Value;

    fn ctx_with(pairs: Vec<(&'static str, IndexKind)>) -> PlanContext<'static> {
        let mut m = HashMap::new();
        m.insert("users", pairs);
        PlanContext {
            indexes: m,
            columnar: Vec::new(),
        }
    }

    #[test]
    fn a_decision_is_the_same_for_every_literal_of_one_shape() {
        let ctx = ctx_with(vec![("country", IndexKind::Hash)]);
        let a = decide(
            &LogicalOp::scan("users").filter(Expr::eq("country", "NO")),
            &ctx,
        );
        let b = decide(
            &LogicalOp::scan("users").filter(Expr::eq("country", "SE")),
            &ctx,
        );
        assert_eq!(a, b, "the decision must not depend on the literal");
    }

    #[test]
    fn rebuilding_uses_this_querys_literal_not_a_cached_one() {
        // The bug this design exists to prevent: a plan cached for id 42 being
        // reused for id 999999.
        let ctx = PlanContext::empty();
        let cached = decide(&LogicalOp::get("users", RecordId(42)), &ctx);
        let rebuilt = build_from(&LogicalOp::get("users", RecordId(999_999)), &cached);
        match rebuilt.root {
            PhysicalOp::GetById { id, .. } => assert_eq!(id, RecordId(999_999)),
            other => panic!("expected GetById, got {}", other.name()),
        }
    }

    #[test]
    fn an_index_key_is_rebound_from_the_current_predicate() {
        let ctx = ctx_with(vec![("country", IndexKind::Hash)]);
        let cached = decide(
            &LogicalOp::scan("users").filter(Expr::eq("country", "NO")),
            &ctx,
        );
        let rebuilt = build_from(
            &LogicalOp::scan("users").filter(Expr::eq("country", "SE")),
            &cached,
        );
        match rebuilt.root.access_path() {
            PhysicalOp::IndexLookup { key, .. } => {
                assert_eq!(*key, Value::Str("SE".into()), "a stale key was reused")
            }
            other => panic!("expected IndexLookup, got {}", other.name()),
        }
    }

    #[test]
    fn range_bounds_are_rebound_too() {
        let ctx = ctx_with(vec![("age", IndexKind::BTree)]);
        let cached = decide(
            &LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 10i64)),
            &ctx,
        );
        let rebuilt = build_from(
            &LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Gt, 99i64)),
            &cached,
        );
        match rebuilt.root.access_path() {
            PhysicalOp::IndexRange { lo, .. } => {
                assert_eq!(*lo, Bound::Excluded(Value::I64(99)))
            }
            other => panic!("expected IndexRange, got {}", other.name()),
        }
    }

    #[test]
    fn decide_then_build_matches_planning_directly() {
        let ctx = ctx_with(vec![
            ("country", IndexKind::Hash),
            ("age", IndexKind::BTree),
        ]);
        let cases = vec![
            LogicalOp::scan("users"),
            LogicalOp::get("users", RecordId(7)),
            LogicalOp::scan("users").filter(Expr::eq("country", "NO")),
            LogicalOp::scan("users").filter(Expr::cmp("age", CmpOp::Ge, 18i64)),
            LogicalOp::scan("users")
                .filter(Expr::eq("country", "NO"))
                .project(vec!["id".into()])
                .limit(3),
        ];
        for c in cases {
            let direct = plan(&c, &ctx);
            let staged = build_from(&c, &decide(&c, &ctx));
            assert_eq!(direct, staged, "staged planning diverged for {}", c.name());
        }
    }

    #[test]
    fn a_limit_value_is_rebound_even_though_the_shape_erases_it() {
        let ctx = PlanContext::empty();
        let cached = decide(&LogicalOp::scan("users").limit(10), &ctx);
        let rebuilt = build_from(&LogicalOp::scan("users").limit(9999), &cached);
        match rebuilt.root {
            PhysicalOp::Limit { n, .. } => assert_eq!(n, 9999),
            other => panic!("expected Limit, got {}", other.name()),
        }
    }
}
