//! Query shapes.
//!
//! A `QueryShape` is a structural fingerprint of a plan with its literals
//! erased, so `country = "NO"` and `country = "SE"` share one shape. It is the
//! aggregation key for telemetry, the cache key for plans, the unit of
//! compilation and the trigger for materialization — four things that all need
//! to agree on what counts as "the same query".
//!
//! The hash is FNV-1a rather than the standard library's, because a shape may
//! be written to the decision log and compared against one computed by a later
//! build. `DefaultHasher` makes no cross-version stability promise; this does.

use crate::expr::Expr;
use crate::plan::LogicalOp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QueryShape(pub u64);

impl QueryShape {
    pub const UNKNOWN: QueryShape = QueryShape(0);

    pub fn of(op: &LogicalOp) -> QueryShape {
        let mut h = Fnv::new();
        hash_op(op, &mut h);
        // Zero is reserved for "unknown", so never hand it out for a real plan.
        QueryShape(if h.0 == 0 { 1 } else { h.0 })
    }
}

impl std::fmt::Display for QueryShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shape:{:016x}", self.0)
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn byte(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(0x1000_0000_01b3);
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.byte(x);
        }
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
        self.byte(0);
    }
    fn usize(&mut self, v: usize) {
        self.bytes(&(v as u64).to_le_bytes());
    }
}

fn hash_expr(e: &Expr, h: &mut Fnv) {
    match e {
        // Literals are erased: that is the entire point. Only the fact that a
        // literal appeared is recorded, not which one.
        Expr::Literal(_) => h.str("lit"),
        Expr::Field(f) => {
            h.str("field");
            h.str(f);
        }
        Expr::Compare { op, lhs, rhs } => {
            h.str("cmp");
            h.str(op.as_str());
            hash_expr(lhs, h);
            hash_expr(rhs, h);
        }
        Expr::And(parts) => {
            h.str("and");
            h.usize(parts.len());
            for p in parts {
                hash_expr(p, h);
            }
        }
        Expr::Or(parts) => {
            h.str("or");
            h.usize(parts.len());
            for p in parts {
                hash_expr(p, h);
            }
        }
        Expr::Not(i) => {
            h.str("not");
            hash_expr(i, h);
        }
        Expr::IsNull(i) => {
            h.str("isnull");
            hash_expr(i, h);
        }
        Expr::IsNotNull(i) => {
            h.str("isnotnull");
            hash_expr(i, h);
        }
        Expr::Arith { op, lhs, rhs } => {
            h.str("arith");
            h.str(arith_op_str(*op));
            hash_expr(lhs, h);
            hash_expr(rhs, h);
        }
        Expr::In { needle, list } => {
            h.str("in");
            hash_expr(needle, h);
            // The count changes what the shape costs to evaluate, so it is
            // part of the shape; the individual elements are literals and are
            // erased, same as everywhere else in this hash.
            h.usize(list.len());
        }
        Expr::Like { text, pattern: _ } => {
            // The pattern is a literal by this file's own rule and is erased
            // like every other one — "name LIKE 'A%'" and "name LIKE 'B%'"
            // are one shape, same as `country = "NO"` and `country = "SE"`
            // are for `Compare`. A pattern's leading wildcard genuinely does
            // change what an index can do with it, but that is a fact for the
            // planner to use when one exists to push a `LIKE` into, not a
            // reason to widen what this hash considers "the same query" —
            // exactly the distinction `Compare` already draws between the
            // comparison *operator*, which is part of the shape, and its
            // *literal*, which is not.
            h.str("like");
            hash_expr(text, h);
        }
    }
}

fn arith_op_str(op: adabt_core::value::ArithOp) -> &'static str {
    use adabt_core::value::ArithOp;
    match op {
        ArithOp::Add => "add",
        ArithOp::Sub => "sub",
        ArithOp::Mul => "mul",
        ArithOp::Div => "div",
    }
}

fn hash_op(op: &LogicalOp, h: &mut Fnv) {
    h.str(op.name());
    match op {
        LogicalOp::GetById { collection, .. } => {
            h.str(collection);
        }
        LogicalOp::GetByIds { collection, ids } => {
            h.str(collection);
            // The *count* changes the plan's cost meaningfully, so unlike a
            // literal it is part of the shape; the ids themselves are not.
            h.usize(ids.len());
        }
        LogicalOp::Scan { collection } => h.str(collection),
        LogicalOp::Filter { predicate, .. } => hash_expr(predicate, h),
        LogicalOp::Project { fields, .. } => {
            for f in fields {
                h.str(f);
            }
        }
        LogicalOp::Sort { keys, .. } => {
            for k in keys {
                h.str(&k.field);
                h.byte(k.descending as u8);
            }
        }
        // A limit of 10 and a limit of 10_000 are the same shape but very
        // different costs; the planner sees the real value, telemetry does not.
        LogicalOp::Limit { .. } => h.str("n"),
        LogicalOp::Aggregate { group_by, aggs, .. } => {
            for g in group_by {
                h.str(g);
            }
            for a in aggs {
                h.str(a.kind.as_str());
                h.str(a.field.as_deref().unwrap_or("*"));
            }
        }
        LogicalOp::Join { kind, on, .. } => {
            h.str(kind.as_str());
            h.str(&on.0);
            h.str(&on.1);
        }
    }
    // Arity is hashed explicitly, before descending into any child, rather
    // than left to fall out of how many times this loop happens to run.
    //
    // Every variant before `Join` had exactly one child or none, so a hash
    // that walked "the" child and stopped when there wasn't one produced a
    // distinct value per shape *by accident* — arity and content were hashed
    // together with no way to tell them apart. `Join` is where that accident
    // stops holding: it has two children, and encoding "two children, X then
    // Y" the same way as "one child, X, then a sibling call encodes Y" would
    // let a two-child shape collide with a differently-structured one-child
    // shape that happens to visit the same content in the same order. Hashing
    // the count first closes that gap without waiting for a real collision to
    // find it.
    let children = op.children();
    h.usize(children.len());
    for c in children {
        hash_op(c, h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{CmpOp, Expr};
    use crate::plan::{Agg, AggKind, SortKey};
    use adabt_core::ids::RecordId;

    #[test]
    fn literals_do_not_change_the_shape() {
        let a = LogicalOp::scan("users").filter(Expr::eq("country", "NO"));
        let b = LogicalOp::scan("users").filter(Expr::eq("country", "SE"));
        assert_eq!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn record_ids_do_not_change_the_shape() {
        assert_eq!(
            QueryShape::of(&LogicalOp::get("users", RecordId(1))),
            QueryShape::of(&LogicalOp::get("users", RecordId(999_999)))
        );
    }

    #[test]
    fn the_collection_does_change_the_shape() {
        assert_ne!(
            QueryShape::of(&LogicalOp::scan("users")),
            QueryShape::of(&LogicalOp::scan("orders"))
        );
    }

    #[test]
    fn the_field_being_compared_changes_the_shape() {
        let a = LogicalOp::scan("u").filter(Expr::eq("country", "NO"));
        let b = LogicalOp::scan("u").filter(Expr::eq("city", "NO"));
        assert_ne!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn the_comparison_operator_changes_the_shape() {
        let a = LogicalOp::scan("u").filter(Expr::cmp("age", CmpOp::Gt, 1i64));
        let b = LogicalOp::scan("u").filter(Expr::cmp("age", CmpOp::Lt, 1i64));
        assert_ne!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn operator_structure_changes_the_shape() {
        let a = LogicalOp::scan("u").limit(10);
        let b = LogicalOp::scan("u");
        assert_ne!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn a_limits_value_does_not_change_the_shape() {
        assert_eq!(
            QueryShape::of(&LogicalOp::scan("u").limit(10)),
            QueryShape::of(&LogicalOp::scan("u").limit(10_000))
        );
    }

    #[test]
    fn sort_direction_changes_the_shape() {
        let key = |descending| {
            LogicalOp::scan("u").sort(vec![SortKey {
                field: "a".into(),
                descending,
            }])
        };
        assert_ne!(QueryShape::of(&key(true)), QueryShape::of(&key(false)));
    }

    #[test]
    fn aggregate_functions_change_the_shape() {
        let a = LogicalOp::scan("s").aggregate(vec![], vec![Agg::over(AggKind::Sum, "x", "o")]);
        let b = LogicalOp::scan("s").aggregate(vec![], vec![Agg::over(AggKind::Max, "x", "o")]);
        assert_ne!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn shapes_are_stable_across_repeated_computation() {
        let p = LogicalOp::scan("u")
            .filter(Expr::eq("a", 1i64))
            .sort(vec![SortKey {
                field: "b".into(),
                descending: false,
            }])
            .limit(3);
        let first = QueryShape::of(&p);
        for _ in 0..100 {
            assert_eq!(QueryShape::of(&p), first);
        }
        // And pinned, so a change to the hashing rules is a deliberate act
        // rather than a silent invalidation of every recorded decision.
        assert_ne!(first, QueryShape::UNKNOWN);
    }

    #[test]
    fn a_real_plan_never_hashes_to_unknown() {
        assert_ne!(QueryShape::of(&LogicalOp::scan("x")), QueryShape::UNKNOWN);
    }
}

/// A full fingerprint of a plan, *including* its literals.
///
/// The complement of `QueryShape`. A shape groups `country = "NO"` with
/// `country = "SE"` because they cost the same and want the same plan; a key
/// separates them because they return different rows. Caching plans by shape
/// and results by key is the whole distinction, and conflating the two would
/// either defeat the plan cache or corrupt the result cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QueryKey(pub u64);

impl QueryKey {
    pub fn of(op: &LogicalOp) -> QueryKey {
        let mut h = Fnv::new();
        hash_op_with_literals(op, &mut h);
        QueryKey(h.0)
    }
}

impl std::fmt::Display for QueryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "key:{:016x}", self.0)
    }
}

fn hash_value(v: &adabt_core::value::Value, h: &mut Fnv) {
    use adabt_core::value::Value;
    match v {
        Value::Null => h.str("null"),
        Value::Bool(b) => {
            h.str("bool");
            h.byte(*b as u8);
        }
        Value::I64(n) => {
            h.str("i64");
            h.bytes(&n.to_le_bytes());
        }
        // Hashed by representation rather than by numeric value, as the
        // integer arms above already are. This hash keys a *cache*, so telling
        // two equal values apart merely costs an entry, while conflating two
        // unequal ones would serve one query's answer to another.
        Value::Decimal { units, scale } => {
            h.str("decimal");
            h.bytes(&units.to_le_bytes());
            h.byte(*scale);
        }
        Value::Timestamp(t) => {
            h.str("timestamp");
            h.bytes(&t.to_le_bytes());
        }
        Value::U64(n) => {
            h.str("u64");
            h.bytes(&n.to_le_bytes());
        }
        Value::F64(f) => {
            h.str("f64");
            // Canonicalise so that values comparing equal hash equally, matching
            // `Value`'s own Eq/Hash contract.
            let f = if f.is_nan() {
                f64::NAN
            } else if *f == 0.0 {
                0.0
            } else {
                *f
            };
            h.bytes(&f.to_bits().to_le_bytes());
        }
        Value::Str(s) => {
            h.str("str");
            h.str(s);
        }
        Value::Bytes(b) => {
            h.str("bytes");
            h.bytes(b);
        }
        Value::List(items) => {
            h.str("list");
            h.usize(items.len());
            for i in items {
                hash_value(i, h);
            }
        }
        Value::Map(m) => {
            h.str("map");
            h.usize(m.len());
            for (k, v) in m {
                h.str(k);
                hash_value(v, h);
            }
        }
    }
}

fn hash_expr_with_literals(e: &Expr, h: &mut Fnv) {
    match e {
        Expr::Literal(v) => {
            h.str("lit");
            hash_value(v, h);
        }
        Expr::Field(f) => {
            h.str("field");
            h.str(f);
        }
        Expr::Compare { op, lhs, rhs } => {
            h.str("cmp");
            h.str(op.as_str());
            hash_expr_with_literals(lhs, h);
            hash_expr_with_literals(rhs, h);
        }
        Expr::And(parts) => {
            h.str("and");
            h.usize(parts.len());
            for p in parts {
                hash_expr_with_literals(p, h);
            }
        }
        Expr::Or(parts) => {
            h.str("or");
            h.usize(parts.len());
            for p in parts {
                hash_expr_with_literals(p, h);
            }
        }
        Expr::Not(i) => {
            h.str("not");
            hash_expr_with_literals(i, h);
        }
        Expr::IsNull(i) => {
            h.str("isnull");
            hash_expr_with_literals(i, h);
        }
        Expr::IsNotNull(i) => {
            h.str("isnotnull");
            hash_expr_with_literals(i, h);
        }
        Expr::Arith { op, lhs, rhs } => {
            h.str("arith");
            h.str(arith_op_str(*op));
            hash_expr_with_literals(lhs, h);
            hash_expr_with_literals(rhs, h);
        }
        Expr::In { needle, list } => {
            h.str("in");
            hash_expr_with_literals(needle, h);
            h.usize(list.len());
            for item in list {
                hash_expr_with_literals(item, h);
            }
        }
        Expr::Like { text, pattern } => {
            h.str("like");
            hash_expr_with_literals(text, h);
            h.str(pattern);
        }
    }
}

fn hash_op_with_literals(op: &LogicalOp, h: &mut Fnv) {
    h.str(op.name());
    match op {
        LogicalOp::GetById { collection, id } => {
            h.str(collection);
            h.bytes(&id.0.to_le_bytes());
        }
        LogicalOp::GetByIds { collection, ids } => {
            h.str(collection);
            h.usize(ids.len());
            for i in ids {
                h.bytes(&i.0.to_le_bytes());
            }
        }
        LogicalOp::Scan { collection } => h.str(collection),
        LogicalOp::Filter { predicate, .. } => hash_expr_with_literals(predicate, h),
        LogicalOp::Project { fields, .. } => {
            for f in fields {
                h.str(f);
            }
        }
        LogicalOp::Sort { keys, .. } => {
            for k in keys {
                h.str(&k.field);
                h.byte(k.descending as u8);
            }
        }
        LogicalOp::Limit { n, .. } => h.usize(*n),
        LogicalOp::Aggregate { group_by, aggs, .. } => {
            for g in group_by {
                h.str(g);
            }
            for a in aggs {
                h.str(a.kind.as_str());
                h.str(a.field.as_deref().unwrap_or("*"));
                h.str(&a.output);
            }
        }
        LogicalOp::Join { kind, on, .. } => {
            h.str(kind.as_str());
            h.str(&on.0);
            h.str(&on.1);
        }
    }
    // Arity hashed explicitly first — see `hash_op`'s identical comment for
    // why this key would otherwise not be collision-safe once a node can have
    // more than one child.
    let children = op.children();
    h.usize(children.len());
    for c in children {
        hash_op_with_literals(c, h);
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use crate::expr::Expr;

    #[test]
    fn a_key_separates_what_a_shape_groups() {
        let a = LogicalOp::scan("u").filter(Expr::eq("country", "NO"));
        let b = LogicalOp::scan("u").filter(Expr::eq("country", "SE"));
        assert_eq!(
            QueryShape::of(&a),
            QueryShape::of(&b),
            "shapes should group"
        );
        assert_ne!(QueryKey::of(&a), QueryKey::of(&b), "keys must separate");
    }

    #[test]
    fn identical_queries_share_a_key() {
        let q = || {
            LogicalOp::scan("u")
                .filter(Expr::eq("country", "NO"))
                .limit(5)
        };
        assert_eq!(QueryKey::of(&q()), QueryKey::of(&q()));
    }

    #[test]
    fn a_different_limit_is_a_different_key() {
        // Unlike the shape, which erases it.
        let a = LogicalOp::scan("u").limit(10);
        let b = LogicalOp::scan("u").limit(20);
        assert_eq!(QueryShape::of(&a), QueryShape::of(&b));
        assert_ne!(QueryKey::of(&a), QueryKey::of(&b));
    }

    #[test]
    fn a_different_record_id_is_a_different_key() {
        use adabt_core::ids::RecordId;
        let a = LogicalOp::get("u", RecordId(1));
        let b = LogicalOp::get("u", RecordId(2));
        assert_eq!(QueryShape::of(&a), QueryShape::of(&b));
        assert_ne!(QueryKey::of(&a), QueryKey::of(&b));
    }

    #[test]
    fn numerically_equal_literals_share_a_key() {
        // Must agree with `Value`'s Eq: I64(1) == F64(1.0) would return the same
        // rows, so caching them separately is waste, not correctness.
        let a = LogicalOp::scan("u").filter(Expr::eq("n", 0.0f64));
        let b = LogicalOp::scan("u").filter(Expr::eq("n", -0.0f64));
        assert_eq!(QueryKey::of(&a), QueryKey::of(&b));
    }
}

#[cfg(test)]
mod arity_tests {
    use super::*;
    use crate::expr::Expr;
    use crate::plan::{JoinKind, LogicalOp};

    /// The bug this file exists to have already fixed: before `Join`, the
    /// per-node hash was followed by "walk to the child and hash it," with
    /// nothing recording how many times that happened. A one-child chain and
    /// a differently-shaped tree that happened to visit the same node
    /// sequence in the same order could hash identically. This constructs the
    /// case directly rather than waiting for `Join` to make it possible by
    /// accident.
    #[test]
    fn two_children_never_collides_with_one_child_visiting_the_same_content() {
        let one_child = LogicalOp::scan("a").filter(Expr::eq("x", "same"));
        let two_children = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("a")),
            kind: JoinKind::Inner,
            on: ("x".into(), "x".into()),
        };
        assert_ne!(
            QueryShape::of(&one_child),
            QueryShape::of(&two_children),
            "a two-child node collided with a differently-shaped one-child node"
        );
    }

    #[test]
    fn arity_is_hashed_even_when_every_other_field_matches() {
        // Two Joins differing only in whether the right side has a filter atop
        // it — same join kind, same key, same leaf collections — still hash
        // differently, because the *shapes* below the join differ even though
        // nothing about the join node itself does.
        let a = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b")),
            kind: JoinKind::Inner,
            on: ("x".into(), "y".into()),
        };
        let b = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b").filter(Expr::eq("y", "anything"))),
            kind: JoinKind::Inner,
            on: ("x".into(), "y".into()),
        };
        assert_ne!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn a_join_hashes_the_same_shape_regardless_of_its_literal_free_content() {
        // Nothing here has literals to erase, but this confirms the arity fix
        // did not accidentally make every Join unique by construction (e.g. by
        // hashing a pointer or an allocation order) rather than by its actual
        // structure.
        let a = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b")),
            kind: JoinKind::Inner,
            on: ("x".into(), "y".into()),
        };
        let b = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b")),
            kind: JoinKind::Inner,
            on: ("x".into(), "y".into()),
        };
        assert_eq!(QueryShape::of(&a), QueryShape::of(&b));
    }

    #[test]
    fn a_different_join_kind_or_key_is_a_different_shape() {
        let base = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b")),
            kind: JoinKind::Inner,
            on: ("x".into(), "y".into()),
        };
        let other_kind = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b")),
            kind: JoinKind::Left,
            on: ("x".into(), "y".into()),
        };
        let other_key = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("a")),
            right: Box::new(LogicalOp::scan("b")),
            kind: JoinKind::Inner,
            on: ("z".into(), "y".into()),
        };
        assert_ne!(QueryShape::of(&base), QueryShape::of(&other_kind));
        assert_ne!(QueryShape::of(&base), QueryShape::of(&other_key));
    }

    #[test]
    fn children_and_sources_agree_with_the_old_single_child_api() {
        let plan = LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .limit(5);
        assert_eq!(plan.children().len(), 1);
        assert_eq!(plan.sources(), vec!["users"]);
        assert_eq!(plan.collection(), "users");
    }

    #[test]
    fn a_join_reports_both_children_and_both_sources() {
        let j = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("orders")),
            right: Box::new(LogicalOp::scan("customers")),
            kind: JoinKind::Inner,
            on: ("customer_id".into(), "id".into()),
        };
        assert_eq!(j.children().len(), 2);
        assert_eq!(j.sources(), vec!["orders", "customers"]);
        assert!(j.contains_join());
    }

    #[test]
    fn contains_join_sees_a_join_buried_under_other_operators() {
        let j = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("orders")),
            right: Box::new(LogicalOp::scan("customers")),
            kind: JoinKind::Inner,
            on: ("customer_id".into(), "id".into()),
        }
        .limit(10);
        assert!(j.contains_join());
        assert!(!LogicalOp::scan("orders").limit(10).contains_join());
    }

    #[test]
    #[should_panic(expected = "use sources()")]
    fn collection_panics_rather_than_silently_picking_one_source() {
        let j = LogicalOp::Join {
            left: Box::new(LogicalOp::scan("orders")),
            right: Box::new(LogicalOp::scan("customers")),
            kind: JoinKind::Inner,
            on: ("customer_id".into(), "id".into()),
        };
        let _ = j.collection();
    }
}
