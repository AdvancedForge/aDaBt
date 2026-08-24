//! Logical plans.
//!
//! A logical plan says *what* is wanted; a physical plan says how to get it.
//! The separation is the same one the whole project rests on, applied to
//! queries: the optimizer may replace any physical plan for a logical one
//! without the caller noticing.

use adabt_core::ids::RecordId;

use crate::expr::Expr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggKind {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AggKind::Count => "count",
            AggKind::Sum => "sum",
            AggKind::Min => "min",
            AggKind::Max => "max",
            AggKind::Avg => "avg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agg {
    pub kind: AggKind,
    /// The aggregated field. `None` only for `Count`, which needs no input.
    pub field: Option<String>,
    pub output: String,
}

impl Agg {
    pub fn count(output: impl Into<String>) -> Self {
        Agg {
            kind: AggKind::Count,
            field: None,
            output: output.into(),
        }
    }
    pub fn over(kind: AggKind, field: impl Into<String>, output: impl Into<String>) -> Self {
        Agg {
            kind,
            field: Some(field.into()),
            output: output.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    pub field: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalOp {
    /// The canonical hot path: one record by identity.
    GetById {
        collection: String,
        id: RecordId,
    },
    GetByIds {
        collection: String,
        ids: Vec<RecordId>,
    },
    Scan {
        collection: String,
    },
    Filter {
        input: Box<LogicalOp>,
        predicate: Expr,
    },
    Project {
        input: Box<LogicalOp>,
        fields: Vec<String>,
    },
    Sort {
        input: Box<LogicalOp>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<LogicalOp>,
        n: usize,
    },
    Aggregate {
        input: Box<LogicalOp>,
        group_by: Vec<String>,
        aggs: Vec<Agg>,
    },
    /// **Reserved, not implemented.** The planner and executor return
    /// `Error::Unsupported` for any plan containing one. It exists in the enum
    /// now — ahead of the algorithm that will fill it in at M23 — because
    /// widening the IR from a chain to a tree is best done before anything is
    /// frozen on a wire, and while the widening can be exercised by a small
    /// number of match arms rather than by every arm the working feature will
    /// eventually need. The equi-join key is a single field pair for now,
    /// which every join algorithm this project would build first needs and
    /// nothing more elaborate has to be threaded through yet.
    Join {
        left: Box<LogicalOp>,
        right: Box<LogicalOp>,
        kind: JoinKind,
        on: (String, String),
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

impl JoinKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
        }
    }
}

impl LogicalOp {
    pub fn scan(collection: impl Into<String>) -> Self {
        LogicalOp::Scan {
            collection: collection.into(),
        }
    }
    pub fn get(collection: impl Into<String>, id: RecordId) -> Self {
        LogicalOp::GetById {
            collection: collection.into(),
            id,
        }
    }
    pub fn filter(self, predicate: Expr) -> Self {
        LogicalOp::Filter {
            input: Box::new(self),
            predicate,
        }
    }
    pub fn project(self, fields: Vec<String>) -> Self {
        LogicalOp::Project {
            input: Box::new(self),
            fields,
        }
    }
    pub fn sort(self, keys: Vec<SortKey>) -> Self {
        LogicalOp::Sort {
            input: Box::new(self),
            keys,
        }
    }
    pub fn limit(self, n: usize) -> Self {
        LogicalOp::Limit {
            input: Box::new(self),
            n,
        }
    }
    pub fn aggregate(self, group_by: Vec<String>, aggs: Vec<Agg>) -> Self {
        LogicalOp::Aggregate {
            input: Box::new(self),
            group_by,
            aggs,
        }
    }
    pub fn join(
        self,
        right: LogicalOp,
        kind: JoinKind,
        on: (impl Into<String>, impl Into<String>),
    ) -> Self {
        LogicalOp::Join {
            left: Box::new(self),
            right: Box::new(right),
            kind,
            on: (on.0.into(), on.1.into()),
        }
    }

    /// Every direct child, in a fixed order (`Join`'s left before its right).
    /// Empty for a leaf (`GetById`, `GetByIds`, `Scan`).
    pub fn children(&self) -> Vec<&LogicalOp> {
        match self {
            LogicalOp::GetById { .. } | LogicalOp::GetByIds { .. } | LogicalOp::Scan { .. } => {
                Vec::new()
            }
            LogicalOp::Filter { input, .. }
            | LogicalOp::Project { input, .. }
            | LogicalOp::Sort { input, .. }
            | LogicalOp::Limit { input, .. }
            | LogicalOp::Aggregate { input, .. } => vec![input],
            LogicalOp::Join { left, right, .. } => vec![left, right],
        }
    }

    /// The single child of a linear (non-branching) node.
    ///
    /// Every variant except `Join` has at most one child, so this is the
    /// common case spelled without a `Vec`. Panics on `Join`, deliberately: a
    /// `Join` reaching this call means something walked the tree assuming a
    /// chain without checking first, which the planner and executor are
    /// supposed to have ruled out already by rejecting the plan outright — see
    /// `Error::Unsupported`. A silent `None` here would let that assumption
    /// keep looking correct on the one input that violates it.
    pub fn child(&self) -> Option<&LogicalOp> {
        match self {
            LogicalOp::Join { .. } => {
                panic!("child() called on a Join; use children(), or reject Join earlier")
            }
            other => other.children().into_iter().next(),
        }
    }

    /// Every collection this plan reads, leaves first, left before right at a
    /// `Join`. Length 1 for every plan that can execute today — `Join` is
    /// reserved but the planner refuses it before anything downstream would
    /// need to ask this question of one.
    pub fn sources(&self) -> Vec<&str> {
        match self {
            LogicalOp::GetById { collection, .. }
            | LogicalOp::GetByIds { collection, .. }
            | LogicalOp::Scan { collection } => vec![collection],
            LogicalOp::Join { left, right, .. } => {
                let mut out = left.sources();
                out.extend(right.sources());
                out
            }
            other => other
                .children()
                .into_iter()
                .flat_map(|c| c.sources())
                .collect(),
        }
    }

    /// Whether this plan or anything beneath it is a `Join`.
    ///
    /// The one thing a caller needs to know about `Join` before doing anything
    /// else with a plan: whether it is safe to hand to the planner at all. See
    /// `Error::Unsupported` and its call site in `adabt-engine`.
    pub fn contains_join(&self) -> bool {
        matches!(self, LogicalOp::Join { .. }) || self.children().iter().any(|c| c.contains_join())
    }

    /// The collection this plan reads, for a plan with exactly one.
    ///
    /// Panics on a plan with more than one source — today, only a tree
    /// containing `Join`, which cannot reach here anyway since the planner and
    /// executor reject `Join` before consulting this. Kept rather than removed
    /// because most of this codebase legitimately has exactly one collection to
    /// ask about and `sources()[0]` at every call site would be noise, not
    /// rigor.
    pub fn collection(&self) -> &str {
        let s = self.sources();
        assert_eq!(
            s.len(),
            1,
            "collection() called on a plan with {} sources; use sources()",
            s.len()
        );
        s[0]
    }

    pub fn name(&self) -> &'static str {
        match self {
            LogicalOp::GetById { .. } => "get_by_id",
            LogicalOp::GetByIds { .. } => "get_by_ids",
            LogicalOp::Scan { .. } => "scan",
            LogicalOp::Filter { .. } => "filter",
            LogicalOp::Project { .. } => "project",
            LogicalOp::Sort { .. } => "sort",
            LogicalOp::Limit { .. } => "limit",
            LogicalOp::Aggregate { .. } => "aggregate",
            LogicalOp::Join { .. } => "join",
        }
    }

    /// Number of operators, for cost estimation and plan display.
    pub fn node_count(&self) -> usize {
        1 + self
            .children()
            .iter()
            .map(|c| c.node_count())
            .sum::<usize>()
    }

    /// Render as an indented tree, for `EXPLAIN`.
    pub fn explain(&self) -> String {
        fn go(op: &LogicalOp, depth: usize, out: &mut String) {
            let pad = "  ".repeat(depth);
            let line = match op {
                LogicalOp::GetById { collection, id } => {
                    format!("GetById({collection}, {id})")
                }
                LogicalOp::GetByIds { collection, ids } => {
                    format!("GetByIds({collection}, {} ids)", ids.len())
                }
                LogicalOp::Scan { collection } => format!("Scan({collection})"),
                LogicalOp::Filter { predicate, .. } => format!("Filter({predicate:?})"),
                LogicalOp::Project { fields, .. } => format!("Project({})", fields.join(", ")),
                LogicalOp::Sort { keys, .. } => format!(
                    "Sort({})",
                    keys.iter()
                        .map(|k| format!("{}{}", k.field, if k.descending { " desc" } else { "" }))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                LogicalOp::Join { kind, on, .. } => {
                    format!("Join({}, on {} = {})", kind.as_str(), on.0, on.1)
                }
                LogicalOp::Limit { n, .. } => format!("Limit({n})"),
                LogicalOp::Aggregate { group_by, aggs, .. } => format!(
                    "Aggregate(by [{}], {})",
                    group_by.join(", "),
                    aggs.iter()
                        .map(|a| format!(
                            "{}({})",
                            a.kind.as_str(),
                            a.field.as_deref().unwrap_or("*")
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            out.push_str(&format!("{pad}{line}\n"));
            for c in op.children() {
                go(c, depth + 1, out);
            }
        }
        let mut s = String::new();
        go(self, 0, &mut s);
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalPlan {
    pub root: LogicalOp,
}

impl LogicalPlan {
    pub fn new(root: LogicalOp) -> Self {
        Self { root }
    }
    pub fn collection(&self) -> &str {
        self.root.collection()
    }
    pub fn explain(&self) -> String {
        self.root.explain()
    }
    pub fn shape(&self) -> crate::shape::QueryShape {
        crate::shape::QueryShape::of(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{CmpOp, Expr};

    fn plan() -> LogicalOp {
        LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["name".into(), "balance".into()])
            .sort(vec![SortKey {
                field: "balance".into(),
                descending: true,
            }])
            .limit(10)
    }

    #[test]
    fn collection_is_found_through_the_operator_chain() {
        assert_eq!(plan().collection(), "users");
        assert_eq!(LogicalOp::get("orders", RecordId(1)).collection(), "orders");
    }

    #[test]
    fn node_count_counts_the_whole_chain() {
        assert_eq!(plan().node_count(), 5);
        assert_eq!(LogicalOp::scan("x").node_count(), 1);
    }

    #[test]
    fn explain_renders_an_indented_tree_outermost_first() {
        let e = plan().explain();
        let lines: Vec<&str> = e.lines().collect();
        assert!(lines[0].starts_with("Limit"), "{e}");
        assert!(lines[1].starts_with("  Sort"), "{e}");
        assert!(
            lines.last().unwrap().trim_start().starts_with("Scan"),
            "{e}"
        );
        // Indentation increases monotonically down the chain.
        let indents: Vec<usize> = lines
            .iter()
            .map(|l| l.len() - l.trim_start().len())
            .collect();
        assert!(indents.windows(2).all(|w| w[1] > w[0]), "{indents:?}");
    }

    #[test]
    fn aggregate_plans_describe_their_functions() {
        let p = LogicalOp::scan("sales").aggregate(
            vec!["region".into()],
            vec![Agg::count("n"), Agg::over(AggKind::Sum, "amount", "total")],
        );
        let e = p.explain();
        assert!(e.contains("count(*)"), "{e}");
        assert!(e.contains("sum(amount)"), "{e}");
        assert!(e.contains("by [region]"), "{e}");
    }

    #[test]
    fn builder_methods_nest_in_call_order() {
        let p = LogicalOp::scan("c")
            .filter(Expr::cmp("a", CmpOp::Gt, 1i64))
            .limit(5);
        assert_eq!(p.name(), "limit");
        assert_eq!(p.child().unwrap().name(), "filter");
        assert_eq!(p.child().unwrap().child().unwrap().name(), "scan");
    }
}

impl LogicalOp {
    /// The fields a plan actually reads, or `None` when it returns whole
    /// records.
    ///
    /// This is what makes a columnar access path legal. A columnar read can
    /// only reconstruct the fields it is asked for, so using one under a plan
    /// that returns whole records would silently drop everything else. `None`
    /// means "cannot be answered columnar" — a deliberately conservative
    /// answer, because the failure mode is missing data rather than a slow
    /// query.
    pub fn required_fields(&self) -> Option<Vec<String>> {
        let mut out = Vec::new();
        if self.collect_required(&mut out) {
            out.sort();
            out.dedup();
            Some(out)
        } else {
            None
        }
    }

    /// Returns false as soon as any operator needs the whole record.
    fn collect_required(&self, out: &mut Vec<String>) -> bool {
        match self {
            // A projection bounds everything beneath it: nothing below can
            // contribute a field that survives.
            LogicalOp::Project { input, fields } => {
                out.extend(fields.iter().cloned());
                let mut below = Vec::new();
                // Operators below may still *read* fields the projection drops.
                if input.collect_required_below(&mut below) {
                    out.extend(below);
                }
                true
            }
            LogicalOp::Aggregate {
                input,
                group_by,
                aggs,
            } => {
                out.extend(group_by.iter().cloned());
                out.extend(aggs.iter().filter_map(|a| a.field.clone()));
                let mut below = Vec::new();
                if input.collect_required_below(&mut below) {
                    out.extend(below);
                }
                true
            }
            // Everything else hands whole records upward.
            LogicalOp::Limit { input, .. } => input.collect_required(out),
            LogicalOp::Sort { input, keys } => {
                out.extend(keys.iter().map(|k| k.field.clone()));
                input.collect_required(out)
            }
            _ => false,
        }
    }

    /// Fields read by operators beneath a projection or aggregate. Unlike
    /// `collect_required`, reaching a leaf here is success.
    fn collect_required_below(&self, out: &mut Vec<String>) -> bool {
        match self {
            LogicalOp::Scan { .. } | LogicalOp::GetById { .. } | LogicalOp::GetByIds { .. } => true,
            LogicalOp::Filter { input, predicate } => {
                predicate.referenced_fields(out);
                input.collect_required_below(out)
            }
            LogicalOp::Sort { input, keys } => {
                out.extend(keys.iter().map(|k| k.field.clone()));
                input.collect_required_below(out)
            }
            LogicalOp::Limit { input, .. } => input.collect_required_below(out),
            LogicalOp::Project { input, fields } => {
                out.extend(fields.iter().cloned());
                input.collect_required_below(out)
            }
            LogicalOp::Aggregate {
                input,
                group_by,
                aggs,
            } => {
                out.extend(group_by.iter().cloned());
                out.extend(aggs.iter().filter_map(|a| a.field.clone()));
                input.collect_required_below(out)
            }
            // Unreachable through an executable plan today — the planner
            // rejects `Join` before field-pushdown analysis would ever see
            // one — but a two-input pass-through is the right shape to have
            // waiting rather than a wildcard that silently drops one side.
            LogicalOp::Join { left, right, .. } => {
                let l = left.collect_required_below(out);
                let r = right.collect_required_below(out);
                l && r
            }
        }
    }
}

#[cfg(test)]
mod required_field_tests {
    use super::*;
    use crate::expr::{CmpOp, Expr};

    #[test]
    fn a_plan_returning_whole_records_requires_everything() {
        // Conservative on purpose: the failure mode of guessing wrong is
        // missing data, not a slow query.
        assert_eq!(LogicalOp::scan("users").required_fields(), None);
        assert_eq!(
            LogicalOp::scan("users")
                .filter(Expr::eq("country", "NO"))
                .required_fields(),
            None
        );
        assert_eq!(LogicalOp::get("users", RecordId(1)).required_fields(), None);
    }

    #[test]
    fn a_projection_bounds_the_fields() {
        let p = LogicalOp::scan("users").project(vec!["id".into(), "name".into()]);
        assert_eq!(
            p.required_fields(),
            Some(vec!["id".to_string(), "name".to_string()])
        );
    }

    #[test]
    fn a_filter_below_a_projection_contributes_its_fields() {
        // The filter still has to read `country` even though the projection
        // drops it; a columnar read that skipped it would filter on nothing.
        let p = LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["id".into()]);
        let got = p.required_fields().unwrap();
        assert!(got.contains(&"id".to_string()));
        assert!(got.contains(&"country".to_string()));
    }

    #[test]
    fn an_aggregate_requires_its_grouping_and_aggregated_fields() {
        let p = LogicalOp::scan("sales").aggregate(
            vec!["region".into()],
            vec![Agg::count("n"), Agg::over(AggKind::Sum, "amount", "total")],
        );
        let got = p.required_fields().unwrap();
        assert!(got.contains(&"region".to_string()));
        assert!(got.contains(&"amount".to_string()));
        // COUNT(*) contributes no field.
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn a_sort_above_a_projection_contributes_its_key() {
        let p = LogicalOp::scan("users")
            .project(vec!["id".into()])
            .sort(vec![SortKey {
                field: "balance".into(),
                descending: false,
            }]);
        let got = p.required_fields().unwrap();
        assert!(got.contains(&"balance".to_string()), "{got:?}");
        assert!(got.contains(&"id".to_string()));
    }

    #[test]
    fn a_limit_is_transparent() {
        let p = LogicalOp::scan("users").project(vec!["id".into()]).limit(5);
        assert_eq!(p.required_fields(), Some(vec!["id".to_string()]));
        assert_eq!(LogicalOp::scan("users").limit(5).required_fields(), None);
    }

    #[test]
    fn fields_are_deduplicated() {
        let p = LogicalOp::scan("users")
            .filter(Expr::cmp("id", CmpOp::Gt, 1i64))
            .project(vec!["id".into(), "id".into()]);
        assert_eq!(p.required_fields(), Some(vec!["id".to_string()]));
    }
}
