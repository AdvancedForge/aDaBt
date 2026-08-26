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
///
/// One covering index as the planner sees it: its encoded name, the
/// projection it carries, and its backing structure — the kind decides
/// whether the index can serve a range or only an equality.
pub type CoveringEntry<'a> = (&'a str, Vec<String>, IndexKind);

pub struct PlanContext<'a> {
    /// Indexes available per collection, keyed by field.
    pub indexes: HashMap<&'a str, Vec<(&'a str, IndexKind)>>,
    /// Composite indexes per collection, as the ordered field list each
    /// covers. Separate from `indexes` because they are matched by a
    /// different question — "does the predicate constrain every one of
    /// these fields" rather than "is this one field indexed" — and folding
    /// them into the same map would mean every existing lookup had to
    /// learn to skip them.
    pub composite: HashMap<&'a str, Vec<Vec<String>>>,
    /// Covering indexes per collection: the indexed field and the projection
    /// it carries. Kept apart from `indexes` for the same reason `composite`
    /// is — the question asked of it is different ("does this projection
    /// contain everything the query reads") and folding it in would make
    /// every existing lookup learn to skip entries it must not select.
    pub covering: HashMap<&'a str, Vec<CoveringEntry<'a>>>,
    /// Partial indexes per collection: the index's full name, the condition
    /// it holds only the qualifying records for, and its kind.
    pub partial: HashMap<&'a str, Vec<(&'a str, Expr, IndexKind)>>,
    /// Collections with a columnar derived representation.
    pub columnar: Vec<&'a str>,
    /// Which fields each collection's columnar representation can
    /// reconstruct. Kept beside `columnar` rather than folded into it because
    /// one question is yes/no and the other is a membership test — and the
    /// membership test is load-bearing: a columnar projection silently omits
    /// fields it does not carry, so a planner that guessed from presence
    /// alone could order a top-K by absence and return an arbitrary k.
    pub columnar_fields: HashMap<&'a str, Vec<String>>,
    /// Estimated distinct values per indexed field — the selectivity signal.
    ///
    /// Sourced from each index's own key count, which is why it exists only
    /// for fields that are already indexed: the planner consults it to choose
    /// *between* serving structures, never to invent one. A field with a
    /// million distinct values narrows an equality probe to roughly one row;
    /// a field with four narrows it to a quarter of the collection, and when
    /// both are indexed the first is the index to ask.
    pub cardinality: HashMap<&'a str, HashMap<&'a str, u64>>,
    /// Approximate row count per collection, as reported by the engine.
    ///
    /// Used with `cardinality` to turn a selectivity ratio into an estimated
    /// match count, and with `crate::cost` to compare a scan's linear cost
    /// against an index's calibrated lookup cost. Absent means unknown — the
    /// planner falls back to the previous rule-based choice rather than
    /// inventing a number.
    pub row_counts: HashMap<&'a str, u64>,
}

impl<'a> PlanContext<'a> {
    pub fn empty() -> Self {
        Self {
            indexes: HashMap::new(),
            composite: HashMap::new(),
            covering: HashMap::new(),
            partial: HashMap::new(),
            columnar: Vec::new(),
            columnar_fields: HashMap::new(),
            cardinality: HashMap::new(),
            row_counts: HashMap::new(),
        }
    }

    pub fn from_indexes(collection: &'a str, indexes: &'a [Box<dyn Index>]) -> Self {
        let mut m = HashMap::new();
        m.insert(
            collection,
            indexes.iter().map(|i| (i.field(), i.kind())).collect(),
        );
        Self {
            covering: {
                let mut c: HashMap<&str, Vec<CoveringEntry>> = HashMap::new();
                let found: Vec<CoveringEntry> = indexes
                    .iter()
                    .filter(|i| !i.covers().is_empty())
                    .map(|i| (i.field(), i.covers().to_vec(), i.kind()))
                    .collect();
                if !found.is_empty() {
                    c.insert(collection, found);
                }
                c
            },
            indexes: m,
            composite: HashMap::new(),
            partial: HashMap::new(),
            columnar: Vec::new(),
            columnar_fields: HashMap::new(),
            cardinality: {
                let mut c: HashMap<&str, HashMap<&str, u64>> = HashMap::new();
                c.insert(
                    collection,
                    indexes
                        .iter()
                        .map(|i| (i.field(), i.key_count() as u64))
                        .collect(),
                );
                c
            },
            row_counts: HashMap::new(),
        }
    }

    fn index_for(&self, collection: &str, field: &str) -> Option<IndexKind> {
        let list = self.indexes.get(collection)?;
        // Prefer a hash index for equality: the caller only asks about a
        // field it already intends to match exactly, and hash is the one
        // structure whose cost does not care how many distinct values the
        // field has. A bitmap answers the same query and `adabt-bench
        // index-scale` shows its lookups tying with hash at every scale
        // measured (100k–1M rows, differences within run-to-run noise, sign
        // flipping between runs) while holding ~6% of the memory — so on
        // latency alone the tie would go to bitmap. It stays second because
        // a bitmap's footprint scales with distinct values times rows, and
        // this engine's ceiling IS its memory; hash-first is the choice that
        // cannot explode on a high-cardinality field nobody has told us is
        // low-cardinality.
        //
        // The cardinality signal this comment awaited now exists — and it
        // reopens the question exactly as predicted: when the field's own
        // key count says low-cardinality, the blow-up risk is off the table,
        // so the measured tie goes to the bitmap. The executor applies the
        // same rule (`Database::index_lookup`), from the same constant, so
        // planning and execution cannot disagree about which structure
        // serves the probe.
        let cardinality = self
            .cardinality
            .get(collection)
            .and_then(|c| c.get(field))
            .copied()
            .unwrap_or(u64::MAX);
        let low_cardinality = cardinality <= adabt_index::LOW_CARDINALITY_KEY_COUNT as u64;
        // Presence flags, so the decision never depends on creation order.
        let mut hash = false;
        let mut bitmap = false;
        let mut other = None;
        for (f, kind) in list {
            if *f == field {
                match kind {
                    IndexKind::Hash => hash = true,
                    IndexKind::Bitmap => bitmap = true,
                    k => other = other.or(Some(*k)),
                }
            }
        }
        if low_cardinality {
            // Measured tie on latency; bitmap's memory advantage decides.
            bitmap
                .then_some(IndexKind::Bitmap)
                .or_else(|| hash.then_some(IndexKind::Hash))
                .or(other)
        } else if hash {
            Some(IndexKind::Hash).or(other)
        } else if bitmap {
            Some(IndexKind::Bitmap).or(other)
        } else {
            other
        }
    }

    /// A covering index on one of the pinned fields whose projection contains
    /// everything the query reads.
    ///
    /// `needed` is `None` when whole records escape the plan; a projection can
    /// never serve that, so this declines rather than looking for a best fit.
    fn covering_for(
        &self,
        collection: &str,
        equalities: &[(String, adabt_core::value::Value)],
        needed: Option<&[String]>,
    ) -> Option<(String, adabt_core::value::Value, Vec<String>)> {
        let needed = needed?;
        let list = self.covering.get(collection)?;
        for (name, covers, _kind) in list {
            let (base, _) = adabt_index::covering_parts(name);
            let Some((_, key)) = equalities.iter().find(|(f, _)| *f == base) else {
                continue;
            };
            if needed.iter().all(|n| covers.contains(n)) {
                return Some((base, key.clone(), needed.to_vec()));
            }
        }
        None
    }

    /// A covering index on one of the range-constrained fields whose
    /// projection contains everything the query reads and whose backing can
    /// walk a range.
    ///
    /// The b-tree sibling of `covering_for`. A hash-backed covering index
    /// holds no ordering, so a range over one is not a slow plan but a silent
    /// empty answer — the backing kind is checked here rather than trusted to
    /// whoever created the index.
    fn covering_range_for(
        &self,
        collection: &str,
        range_fields: &[String],
        needed: Option<&[String]>,
    ) -> Option<String> {
        let needed = needed?;
        let list = self.covering.get(collection)?;
        for (name, covers, kind) in list {
            if !matches!(kind, IndexKind::BTree) {
                continue;
            }
            let (base, _) = adabt_index::covering_parts(name);
            if range_fields.contains(&base) && needed.iter().all(|n| covers.contains(n)) {
                return Some(base);
            }
        }
        None
    }

    /// A partial index on `field` whose condition the query's predicate
    /// guarantees.
    ///
    /// The test is *syntactic containment*: the predicate must be, or must
    /// contain as a top-level `AND` conjunct, an expression structurally equal
    /// to the index's condition. That is far weaker than real implication —
    /// `age > 20` does not match an index conditioned on `age > 18`, though it
    /// entails it — and weak is the correct direction to err. Failing to
    /// recognise an implication costs a slower plan; inventing one returns
    /// rows that are not there, or drops rows that are.
    fn partial_for(
        &self,
        collection: &str,
        field: &str,
        predicate: &Expr,
    ) -> Option<(String, IndexKind)> {
        let list = self.partial.get(collection)?;
        for (name, condition, kind) in list {
            let (base, _) = adabt_index::partial_parts(name);
            if base != field {
                continue;
            }
            if implies(predicate, condition) {
                return Some((name.to_string(), *kind));
            }
        }
        None
    }

    /// A composite index every one of whose fields the predicate pins to a
    /// literal, with the key that lookup needs.
    ///
    /// Longest first: an index over `(a, b, c)` narrows strictly harder than
    /// one over `(a, b)` when the predicate constrains all three, and picking
    /// the shorter one would leave work for the residual filter that the index
    /// could have done.
    fn composite_for(
        &self,
        collection: &str,
        equalities: &[(String, adabt_core::value::Value)],
    ) -> Option<(Vec<String>, adabt_core::value::Value)> {
        let list = self.composite.get(collection)?;
        let mut best: Option<(Vec<String>, adabt_core::value::Value)> = None;
        for fields in list {
            let mut parts = Vec::with_capacity(fields.len());
            let mut covered = true;
            for f in fields {
                match equalities.iter().find(|(name, _)| name == f) {
                    Some((_, v)) => parts.push(v.clone()),
                    None => {
                        covered = false;
                        break;
                    }
                }
            }
            if !covered {
                continue;
            }
            let longer = best.as_ref().is_none_or(|(b, _)| fields.len() > b.len());
            if longer {
                best = Some((fields.clone(), adabt_core::value::Value::List(parts)));
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
            composite: HashMap::new(),
            covering: HashMap::new(),
            partial: HashMap::new(),
            columnar: Vec::new(),
            columnar_fields: HashMap::new(),
            cardinality: HashMap::new(),
            row_counts: HashMap::new(),
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

    /// `ctx_with` plus per-field cardinality estimates.
    fn ctx_with_card(pairs: Vec<(&'static str, IndexKind)>, card: u64) -> PlanContext<'static> {
        let mut ctx = ctx_with(pairs);
        let mut c = HashMap::new();
        let mut inner = HashMap::new();
        inner.insert("country", card);
        c.insert("users", inner);
        ctx.cardinality = c;
        ctx
    }

    #[test]
    fn a_low_cardinality_field_plans_its_bitmap_over_a_hash_tie() {
        // The measurement said the two tie on latency and the bitmap holds
        // ~6% of the memory; with cardinality proving the field small, the
        // memory decides. Creation order must not matter — test both orders.
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        for pairs in [
            vec![("country", IndexKind::Hash), ("country", IndexKind::Bitmap)],
            vec![("country", IndexKind::Bitmap), ("country", IndexKind::Hash)],
        ] {
            let p = plan(&l, &ctx_with_card(pairs, 4));
            match p.root.access_path() {
                PhysicalOp::IndexLookup { kind, .. } => {
                    assert_eq!(*kind, IndexKind::Bitmap)
                }
                other => panic!("expected an index lookup, got {}", other.name()),
            }
        }
    }

    #[test]
    fn a_high_cardinality_field_keeps_the_hash_first_ordering() {
        let l = LogicalOp::scan("users").filter(Expr::eq("user_id", 7));
        let _ = &l;
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let p = plan(
            &l,
            &ctx_with_card(
                vec![("country", IndexKind::Bitmap), ("country", IndexKind::Hash)],
                900_000,
            ),
        );
        match p.root.access_path() {
            PhysicalOp::IndexLookup { kind, .. } => assert_eq!(*kind, IndexKind::Hash),
            other => panic!("expected an index lookup, got {}", other.name()),
        }
    }

    #[test]
    fn unknown_cardinality_preserves_the_shipped_hash_first_choice() {
        // No estimate: behave exactly as before the signal existed.
        let l = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let p = plan(
            &l,
            &ctx_with(vec![
                ("country", IndexKind::Bitmap),
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
    ColumnScan {
        fields: Vec<String>,
    },
    IndexLookup {
        field: String,
        kind: IndexKind,
    },
    /// A composite index covering every field the predicate pins.
    CompositeLookup {
        fields: Vec<String>,
    },
    /// A covering index that answers the whole query without a fetch.
    CoveringLookup {
        field: String,
        needed: Vec<String>,
    },
    /// A range over a b-tree-backed covering index, answered without a fetch.
    ///
    /// The decision carries only shape-stable facts — which field, what the
    /// projection must contain. The bounds bind at build time from this
    /// query's predicate, exactly as an index lookup binds its key.
    CoveringRange {
        field: String,
        needed: Vec<String>,
    },
    IndexRange {
        field: String,
    },
    /// A top-K over a single sort key, served from the column store: read the
    /// key column, keep the k smallest under Sort's exact order, fetch only
    /// the winners. Chosen for a `Limit` directly over a single-key `Sort`
    /// over a bare `Scan`, where sorting every materialized record would
    /// throw all but k of them away.
    ///
    /// The decision deliberately does NOT carry k. Decisions are cached by
    /// shape, and a query's shape hashes the presence of a limit but never
    /// its value — so a k stored here would be the first asker's k, served
    /// to every later limit on the same shape as if it were theirs. The
    /// limit's value is bound at build time from the node itself, the same
    /// way an index lookup binds its key from the predicate above the scan.
    ColumnarTopK {
        key: String,
        descending: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDecision {
    pub access: AccessDecision,
    pub rationale: String,
}

/// Whether `predicate` guarantees `condition` holds for every row it selects.
///
/// Structural, not semantic. `predicate` implies `condition` when the two are
/// equal, or when `predicate` is an `AND` one of whose conjuncts implies it.
/// Everything else is `false`.
///
/// This is deliberately the weakest rule that is obviously sound. Real
/// predicate implication is undecidable in general and expensive well before
/// the general case, and every step toward cleverness here trades a slower
/// plan for the possibility of a wrong one. `OR` is not descended into for the
/// same reason it must not be: `a OR b` guarantees neither.
fn implies(predicate: &Expr, condition: &Expr) -> bool {
    if predicate == condition {
        return true;
    }
    match predicate {
        Expr::And(parts) => parts.iter().any(|p| implies(p, condition)),
        _ => false,
    }
}

/// The predicate of a `Filter` sitting directly on a `Scan`, if the plan has
/// one.
///
/// The same shape `decide`'s walk looks for, extracted so the access decision
/// can be made before the walk. Only a filter *directly* over a scan counts:
/// anything between them changes what reaches the filter, and an index chosen
/// from a predicate that no longer applies to the whole collection would
/// return the wrong rows.
fn filter_directly_over_scan(op: &LogicalOp) -> Option<&Expr> {
    match op {
        LogicalOp::Filter { input, predicate }
            if matches!(input.as_ref(), LogicalOp::Scan { .. }) =>
        {
            Some(predicate)
        }
        LogicalOp::Filter { input, .. }
        | LogicalOp::Project { input, .. }
        | LogicalOp::Sort { input, .. }
        | LogicalOp::Limit { input, .. }
        | LogicalOp::Aggregate { input, .. } => filter_directly_over_scan(input),
        _ => None,
    }
}

/// The shape a `ColumnarTopK` answers: a limit directly over a single-key
/// sort directly over a bare scan.
///
/// Single-key only, and no filter anywhere between: the columnar read
/// reconstructs only the fields it is asked for, so filtering before the
/// winners are chosen would evaluate the predicate against records that do
/// not carry it. Both restrictions are the weakest obviously-sound rule,
/// the same policy the partial-index matcher follows.
fn topk_over_scan(op: &LogicalOp) -> Option<(&str, &str, bool, usize)> {
    // Projections above the limit are descended through: a top-K returns
    // whole records for its winners, so whatever projects over them composes
    // unchanged. Anything between the sort and the scan still blocks.
    let mut op = op;
    while let LogicalOp::Project { input, .. } = op {
        op = input;
    }
    let LogicalOp::Limit { input, n } = op else {
        return None;
    };
    let LogicalOp::Sort { input, keys } = input.as_ref() else {
        return None;
    };
    let [only] = keys.as_slice() else {
        return None;
    };
    let LogicalOp::Scan { collection } = input.as_ref() else {
        return None;
    };
    Some((
        collection.as_str(),
        only.field.as_str(),
        only.descending,
        *n,
    ))
}

/// Choose an access path. Depends on the plan's shape and the available
/// indexes, never on its literals.
pub fn decide(logical: &LogicalOp, ctx: &PlanContext<'_>) -> PlanDecision {
    // What the plan actually reads, computed once. `None` means whole records
    // reach the caller, which rules out both of the representations below:
    // neither a columnar read nor a covering index can reconstruct a field
    // nobody told them to keep.
    let needed = logical.required_fields();

    // Covering is considered before columnar, and before the walk to the leaf,
    // because it is the only access path that returns rows without reading the
    // primary at all. A column scan still scans; a covering lookup does not.
    if let Some(pred) = filter_directly_over_scan(logical) {
        let equalities = pred.equality_constraints();
        if let Some((field, _, needed)) =
            ctx.covering_for(logical.collection(), &equalities, needed.as_deref())
        {
            let rationale = format!(
                "equality on {}.{field} answered from a covering index ({})",
                logical.collection(),
                needed.join(", ")
            );
            return PlanDecision {
                access: AccessDecision::CoveringLookup { field, needed },
                rationale,
            };
        }
        // The range sibling: same no-fetch idea, b-tree backing, bounds
        // bound at build time from the predicate above the scan.
        if let Some(pred) = filter_directly_over_scan(logical) {
            let mut ranged = Vec::new();
            range_fields(pred, &mut ranged);
            if let Some(field) =
                ctx.covering_range_for(logical.collection(), &ranged, needed.as_deref())
            {
                let rationale = format!(
                    "range on {}.{field} answered from a covering index",
                    logical.collection()
                );
                return PlanDecision {
                    access: AccessDecision::CoveringRange {
                        field,
                        needed: needed.unwrap_or_default(),
                    },
                    rationale,
                };
            }
        }
    }

    // A limit over a single-key sort over a bare scan is a top-K question,
    // not a sort question. Considered before the walk because, like covering
    // above, it replaces more than the leaf access — it removes the sort
    // itself. Gated on the column store actually holding the key field: the
    // columnar projection silently omits fields it does not carry, and a
    // top-K ordered by absence would return an arbitrary k rather than a
    // wrong-by-a-little k.
    if let Some((collection, key, descending, _)) = topk_over_scan(logical) {
        if ctx
            .columnar_fields
            .get(collection)
            .is_some_and(|fields| fields.iter().any(|f| f == key))
        {
            return PlanDecision {
                access: AccessDecision::ColumnarTopK {
                    key: key.to_string(),
                    descending,
                },
                rationale: format!(
                    "top-N by {key} read columnarly from {collection}; only the winners are fetched"
                ),
            };
        }
    }

    // Walk to the leaf and settle the primary access path BEFORE columnar is
    // considered.
    //
    // It did not always. The columnar branch used to sit here, above the
    // walk, behind a guard reading `equality_indexed = false` — a constant,
    // so the guard never fired, and every filtered query over a collection
    // with a column store was served columnar even when an index served the
    // same predicate better. Measured against SQLite, indexed equality went
    // 43 ms at level 0 to 210 ms after `optimize()` (range: 30 ms to 171 ms):
    // a hash lookup fetching 12,500 records replaced by a full columnar scan
    // of all 100,000. The module's own comment already claimed the opposite
    // precedence — "considered only where the access would otherwise be a
    // full scan" — so the code now does what it said.
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
                    let equalities = pred.equality_constraints();
                    // Composite first: an index covering three pinned fields
                    // narrows harder than any single-field index over one of
                    // them, so asking about single fields first would take a
                    // worse path whenever both exist.
                    if let Some((fields, _)) = ctx.composite_for(collection, &equalities) {
                        let rationale = format!(
                            "equality on every field of the composite index ({}) on {collection}",
                            fields.join(", ")
                        );
                        return PlanDecision {
                            access: AccessDecision::CompositeLookup { fields },
                            rationale,
                        };
                    }
                    // Among single-field candidates, the most selective
                    // field's index is probed: each candidate narrows the
                    // collection to roughly rows/distinct, so the field with
                    // the most distinct values fetches the fewest ids, and
                    // the `Filter` above re-checks the rest of the predicate
                    // either way. Fields without a cardinality estimate rank
                    // as if unbounded — worse than any known count but still
                    // usable — which preserves the previous first-wins order
                    // when nothing is measured yet.
                    let mut best: Option<(u64, &str)> = None;
                    for (field, _) in &equalities {
                        let has_index = ctx.partial_for(collection, field, pred).is_some()
                            || ctx.index_for(collection, field).is_some();
                        if !has_index {
                            continue;
                        }
                        let distinct = ctx
                            .cardinality
                            .get(collection.as_str())
                            .and_then(|mm| mm.get(field.as_str()))
                            .copied()
                            .unwrap_or(u64::MAX);
                        // Strictly greater, so ties and unknowns keep the
                        // predicate's own order and behaviour is unchanged
                        // when every estimate is absent.
                        if best.is_none_or(|(d, _)| distinct > d) {
                            best = Some((distinct, field));
                        }
                    }
                    if let Some((distinct, field)) = best {
                        // Calibrated cost gate: when the predicate matches a
                        // large fraction of the collection, a full scan can be
                        // cheaper than probing the index and fetching the hits.
                        // The calibrated `scan_wins_over_lookups` threshold is
                        // very low for small tables (a 2k-row scan is ~8µs vs
                        // ~6.3µs per lookup), so we gate only when the match
                        // fraction is substantial (>1/3), preserving the
                        // previous behaviour for typical selective indexes while
                        // still flipping the unselective large-table case.
                        let scan_wins = ctx
                            .row_counts
                            .get(collection.as_str())
                            .copied()
                            .is_some_and(|rows| {
                                if distinct == u64::MAX || distinct == 0 || rows < 1_000 {
                                    return false;
                                }
                                let matched = (rows / distinct).max(1);
                                matched * 3 > rows
                            });
                        if !scan_wins {
                            // A partial index first when the predicate guarantees
                            // its condition: it holds a subset of the same rows,
                            // so it is strictly cheaper to probe, and the `Filter`
                            // above re-checks the whole predicate either way.
                            //
                            // The decision names the index by its full name —
                            // condition and all — because that is what the index
                            // answers to. A bare field name would find the
                            // unrestricted index, or nothing.
                            if let Some((name, kind)) = ctx.partial_for(collection, field, pred) {
                                let rationale = format!(
                                    "equality on {collection}.{field} served by a partial {} index",
                                    kind.as_str()
                                );
                                return PlanDecision {
                                    access: AccessDecision::IndexLookup { field: name, kind },
                                    rationale,
                                };
                            }
                            if let Some(kind) = ctx.index_for(collection, field) {
                                let rationale = format!(
                                    "equality on {collection}.{field} served by {} index",
                                    kind.as_str()
                                );
                                return PlanDecision {
                                    access: AccessDecision::IndexLookup {
                                        field: field.to_string(),
                                        kind,
                                    },
                                    rationale,
                                };
                            }
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
                // No index serves this plan, so the choice is columnar versus
                // heap — and here a partial read of the needed fields beats
                // materializing whole records. `required_fields` returning
                // None means whole records reach the caller and columnar is
                // not legal at all. An index that exists but does not fit the
                // predicate is not consulted; whether columnar should also
                // lose to an index whose selectivity is terrible is a cost
                // question with no stats behind it yet, and precedence here
                // stays on the documented side.
                if ctx.has_columnar(collection) {
                    if let Some(fields) = needed.clone() {
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
                        // The decision may name a *partial* index, whose name
                        // is the field plus its encoded condition. The key is
                        // still bound from the plain field, so strip the
                        // condition before matching — comparing the whole name
                        // against the predicate's field names would never
                        // match, and the plan would silently fall through to a
                        // scan.
                        let (base, _) = adabt_index::partial_parts(field);
                        if let Some((_, key)) = predicate
                            .equality_constraints()
                            .into_iter()
                            .find(|(f, _)| *f == base)
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
                    AccessDecision::CoveringLookup { field, needed } => {
                        // Rebound from this predicate's literals, like every
                        // other access path here.
                        //
                        // The `Filter` above it is not redundant. A covering
                        // index pins one field; the predicate may constrain
                        // others, and those rows are in the index too. Dropping
                        // the filter here would return them.
                        let equalities = predicate.equality_constraints();
                        if let Some((_, key)) = equalities.iter().find(|(n, _)| n == field) {
                            return PhysicalOp::Filter {
                                input: Box::new(PhysicalOp::CoveringLookup {
                                    collection: collection.clone(),
                                    field: field.clone(),
                                    key: key.clone(),
                                    needed: needed.clone(),
                                }),
                                predicate: predicate.clone(),
                            };
                        }
                    }
                    AccessDecision::CompositeLookup { fields } => {
                        // Rebound from *this* predicate's literals, like every
                        // other access path here — a cached decision names the
                        // fields, never the values.
                        let equalities = predicate.equality_constraints();
                        let mut parts = Vec::with_capacity(fields.len());
                        let mut covered = true;
                        for f in fields {
                            match equalities.iter().find(|(n, _)| n == f) {
                                Some((_, v)) => parts.push(v.clone()),
                                None => {
                                    covered = false;
                                    break;
                                }
                            }
                        }
                        if covered {
                            return PhysicalOp::Filter {
                                input: Box::new(PhysicalOp::CompositeLookup {
                                    collection: collection.clone(),
                                    fields: fields.clone(),
                                    key: adabt_core::value::Value::List(parts),
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
                    AccessDecision::CoveringRange { field, needed } => {
                        if let Some((lo, hi)) = range_constraint(predicate, field) {
                            return PhysicalOp::Filter {
                                input: Box::new(PhysicalOp::CoveringRange {
                                    collection: collection.clone(),
                                    field: field.clone(),
                                    needed: needed.clone(),
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
        LogicalOp::Limit { input, n } => {
            // A top-K decision names the whole subtree beneath this limit —
            // the columnar read, the winner selection and the fetch of the
            // survivors are one operator. Nothing below is built.
            //
            // k comes from THIS node, never from the decision: decisions are
            // cached by shape and a shape does not know what n is.
            if let AccessDecision::ColumnarTopK { key, descending } = &decision.access {
                let LogicalOp::Sort { input, .. } = input.as_ref() else {
                    unreachable!("a ColumnarTopK decision is only made over Limit(Sort(..))")
                };
                let LogicalOp::Scan { collection } = input.as_ref() else {
                    unreachable!("a ColumnarTopK decision is only made over a bare Scan")
                };
                return PhysicalOp::ColumnarTopK {
                    collection: collection.clone(),
                    key: key.clone(),
                    descending: *descending,
                    k: *n,
                };
            }
            PhysicalOp::Limit {
                input: Box::new(build_node(input, decision)),
                n: *n,
            }
        }
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
            composite: HashMap::new(),
            covering: HashMap::new(),
            partial: HashMap::new(),
            columnar: Vec::new(),
            columnar_fields: HashMap::new(),
            cardinality: HashMap::new(),
            row_counts: HashMap::new(),
        }
    }

    fn ctx_with_cardinality(pairs: Vec<(&'static str, IndexKind, u64)>) -> PlanContext<'static> {
        let mut m = HashMap::new();
        let mut card = HashMap::new();
        m.insert(
            "users",
            pairs.iter().map(|(f, k, _)| (*f, *k)).collect::<Vec<_>>(),
        );
        card.insert(
            "users",
            pairs
                .iter()
                .map(|(f, _, d)| (*f, *d))
                .collect::<HashMap<&str, u64>>(),
        );
        PlanContext {
            indexes: m,
            composite: HashMap::new(),
            covering: HashMap::new(),
            partial: HashMap::new(),
            columnar: Vec::new(),
            columnar_fields: HashMap::new(),
            cardinality: card,
            row_counts: HashMap::new(),
        }
    }

    #[test]
    fn the_more_selective_indexed_field_is_probed_first() {
        // country has 4 distinct values; user_id has 100_000. A predicate
        // constraining both is served by the user_id index — probing country
        // first would fetch 250× the ids only to throw them away.
        let ctx = ctx_with_cardinality(vec![
            ("country", IndexKind::Hash, 4),
            ("user_id", IndexKind::Hash, 100_000),
        ]);
        let d = decide(
            &LogicalOp::scan("users").filter(Expr::And(vec![
                Expr::eq("country", "NO"),
                Expr::eq("user_id", Value::U64(7)),
            ])),
            &ctx,
        );
        match d.access {
            AccessDecision::IndexLookup { field, .. } => {
                assert_eq!(field, "user_id", "rationale was: {}", d.rationale)
            }
            _other => panic!("expected an index lookup, got {}", d.rationale),
        }
    }

    #[test]
    fn unknown_selectivity_keeps_the_predicate_order() {
        // With no estimates at all (the empty map every pre-existing caller
        // passes), behaviour is exactly what shipped before: the predicate's
        // first indexed field wins.
        let ctx = ctx_with(vec![("a", IndexKind::Hash), ("b", IndexKind::Hash)]);
        let d = decide(
            &LogicalOp::scan("users").filter(Expr::And(vec![
                Expr::eq("b", Value::U64(1)),
                Expr::eq("a", "x"),
            ])),
            &ctx,
        );
        match d.access {
            AccessDecision::IndexLookup { field, .. } => assert_eq!(field, "b"),
            _other => panic!("expected an index lookup, got {}", d.rationale),
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

    #[test]
    fn cost_gate_prefers_scan_when_index_matches_most_of_a_large_collection() {
        // 800k rows, field with only 2 distinct values -> ~400k matches.
        // scan_wins_over_lookups(800k) ~ 258, so 400k lookups lose badly.
        let mut ctx = ctx_with(vec![("status", IndexKind::Hash)]);
        ctx.cardinality
            .entry("users")
            .or_default()
            .insert("status", 2);
        ctx.row_counts.insert("users", 800_000);
        let l = LogicalOp::scan("users").filter(Expr::eq("status", "active"));
        let p = plan(&l, &ctx);
        assert!(
            p.is_full_scan(),
            "low-cardinality index on large table should lose to scan, got {}",
            p.rationale
        );
    }

    #[test]
    fn cost_gate_keeps_index_when_selective_on_same_large_collection() {
        // Same 800k rows, but 400k distinct -> ~2 matches, well under threshold.
        let mut ctx = ctx_with(vec![("user_id", IndexKind::Hash)]);
        ctx.cardinality
            .entry("users")
            .or_default()
            .insert("user_id", 400_000);
        ctx.row_counts.insert("users", 800_000);
        let l = LogicalOp::scan("users").filter(Expr::eq("user_id", 42i64));
        let p = plan(&l, &ctx);
        assert_eq!(p.root.access_path().name(), "IndexLookup");
    }
}
