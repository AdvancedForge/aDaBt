//! Physical plans.
//!
//! A physical plan names *how* each step happens. It is the layer the optimizer
//! actually rewrites: the logical plan is the user's intent and never changes,
//! while the physical plan may be replanned freely as indexes appear, caches
//! warm, or representations are specialised.

use adabt_core::ids::RecordId;
use adabt_core::value::Value;
use adabt_index::IndexKind;
use adabt_ir::plan::{Agg, JoinKind, SortKey};
use adabt_ir::Expr;
use std::ops::Bound;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalOp {
    /// One record by identity, through the page directory.
    GetById {
        collection: String,
        id: RecordId,
    },
    GetByIds {
        collection: String,
        ids: Vec<RecordId>,
    },
    /// Every record in the collection.
    HeapScan {
        collection: String,
    },
    /// Every record, read from a columnar representation, one column per field.
    ///
    /// Only legal when the plan above reads a bounded set of fields: a columnar
    /// read reconstructs exactly what it is asked for and nothing else.
    ColumnScan {
        collection: String,
        fields: Vec<String>,
    },
    /// Ids from an index, then a fetch per id.
    IndexLookup {
        collection: String,
        field: String,
        kind: IndexKind,
        key: Value,
    },
    /// The k smallest rows under a single-key order, read columnarly.
    ///
    /// A `Limit` over a `Sort` does not need a sorted collection — it needs k
    /// winners. This reads only the sort key out of the column store, keeps
    /// the k smallest under exactly `Sort`'s total order (key, then id), and
    /// fetches full records for those k alone. Legal only when the plan above
    /// reads whole records of the survivors, which fetching provides.
    ColumnarTopK {
        collection: String,
        key: String,
        descending: bool,
        k: usize,
    },
    IndexRange {
        collection: String,
        field: String,
        lo: Bound<Value>,
        hi: Bound<Value>,
    },
    /// Rows from a covering index, with no fetch at all.
    ///
    /// `needed` is what the plan above will actually read. The planner only
    /// emits this when the index carries every one of them, so the rows the
    /// index holds are a complete answer rather than a partial one that would
    /// have to be topped up from the heap.
    CoveringLookup {
        collection: String,
        field: String,
        key: Value,
        needed: Vec<String>,
    },
    /// Rows in a range, served from a covering index with no fetch.
    ///
    /// The b-tree-backed sibling of `CoveringLookup`: same projections beside
    /// the keys, answered through the inner index's range scan. The planner
    /// only emits it for a covering index whose backing can walk a range.
    CoveringRange {
        collection: String,
        field: String,
        needed: Vec<String>,
        lo: Bound<Value>,
        hi: Bound<Value>,
    },
    /// Ids from a composite index, then a fetch per id. `key` is the
    /// `Value::List` of the pinned field values, in the index's own field
    /// order.
    CompositeLookup {
        collection: String,
        fields: Vec<String>,
        key: Value,
    },
    Filter {
        input: Box<PhysicalOp>,
        predicate: Expr,
    },
    Project {
        input: Box<PhysicalOp>,
        fields: Vec<String>,
    },
    Sort {
        input: Box<PhysicalOp>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<PhysicalOp>,
        n: usize,
    },
    Aggregate {
        input: Box<PhysicalOp>,
        group_by: Vec<String>,
        aggs: Vec<Agg>,
    },
    /// A binary equi-join. See `adabt_exec::exec`'s `PhysicalOp::Join` arm for
    /// the algorithm — an indexed nested loop when `right` is an unfiltered
    /// `HeapScan` and the join field is indexed, a hash join otherwise.
    Join {
        left: Box<PhysicalOp>,
        right: Box<PhysicalOp>,
        kind: JoinKind,
        on: (String, String),
    },
}

impl PhysicalOp {
    /// The single child of a unary operator. Panics on `Join`, which has
    /// two — see `children()`. Mirrors `LogicalOp::child`/`children` exactly,
    /// for the same reason: a silent `None` here would let code that assumes
    /// a chain keep looking correct on the one input that violates it.
    pub fn child(&self) -> Option<&PhysicalOp> {
        match self {
            PhysicalOp::GetById { .. }
            | PhysicalOp::GetByIds { .. }
            | PhysicalOp::HeapScan { .. }
            | PhysicalOp::ColumnScan { .. }
            | PhysicalOp::ColumnarTopK { .. }
            | PhysicalOp::IndexLookup { .. }
            | PhysicalOp::IndexRange { .. }
            | PhysicalOp::CompositeLookup { .. }
            | PhysicalOp::CoveringLookup { .. }
            | PhysicalOp::CoveringRange { .. } => None,
            PhysicalOp::Filter { input, .. }
            | PhysicalOp::Project { input, .. }
            | PhysicalOp::Sort { input, .. }
            | PhysicalOp::Limit { input, .. }
            | PhysicalOp::Aggregate { input, .. } => Some(input),
            PhysicalOp::Join { .. } => {
                panic!("child() called on a Join; use children()")
            }
        }
    }

    /// Every direct child, in a fixed order (`Join`'s left before its right).
    pub fn children(&self) -> Vec<&PhysicalOp> {
        match self {
            PhysicalOp::Join { left, right, .. } => vec![left, right],
            other => other.child().into_iter().collect(),
        }
    }

    pub fn collection(&self) -> &str {
        match self {
            PhysicalOp::GetById { collection, .. }
            | PhysicalOp::GetByIds { collection, .. }
            | PhysicalOp::HeapScan { collection }
            | PhysicalOp::ColumnScan { collection, .. }
            | PhysicalOp::ColumnarTopK { collection, .. }
            | PhysicalOp::IndexLookup { collection, .. }
            | PhysicalOp::IndexRange { collection, .. }
            | PhysicalOp::CompositeLookup { collection, .. }
            | PhysicalOp::CoveringLookup { collection, .. }
            | PhysicalOp::CoveringRange { collection, .. } => collection,
            other => other.child().expect("non-leaf has a child").collection(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            PhysicalOp::GetById { .. } => "GetById",
            PhysicalOp::GetByIds { .. } => "GetByIds",
            PhysicalOp::HeapScan { .. } => "HeapScan",
            PhysicalOp::ColumnScan { .. } => "ColumnScan",
            PhysicalOp::ColumnarTopK { .. } => "ColumnarTopK",
            PhysicalOp::IndexLookup { .. } => "IndexLookup",
            PhysicalOp::IndexRange { .. } => "IndexRange",
            PhysicalOp::CompositeLookup { .. } => "CompositeLookup",
            PhysicalOp::CoveringLookup { .. } => "CoveringLookup",
            PhysicalOp::CoveringRange { .. } => "CoveringRange",
            PhysicalOp::Filter { .. } => "Filter",
            PhysicalOp::Project { .. } => "Project",
            PhysicalOp::Sort { .. } => "Sort",
            PhysicalOp::Limit { .. } => "Limit",
            PhysicalOp::Aggregate { .. } => "Aggregate",
            PhysicalOp::Join { .. } => "Join",
        }
    }

    /// Whether this plan reads the whole collection.
    ///
    /// The planner reports it and `EXPLAIN` shows it, because "did this turn
    /// into a full scan" is the single most useful thing to know about a plan.
    /// For a `Join`, true if either side does — the query touches a full
    /// collection somewhere if either input does.
    pub fn is_full_scan(&self) -> bool {
        match self {
            PhysicalOp::HeapScan { .. }
            | PhysicalOp::ColumnScan { .. }
            // Reads the whole key column: every row is touched, only the
            // winners are fetched. That distinction belongs in EXPLAIN's
            // operator name, not in a method named is_full_scan.
            | PhysicalOp::ColumnarTopK { .. } => true,
            PhysicalOp::Join { left, right, .. } => left.is_full_scan() || right.is_full_scan(),
            other => other.child().is_some_and(|c| c.is_full_scan()),
        }
    }

    /// The access path at the bottom of the plan.
    ///
    /// A `Join` is treated as the bottom itself rather than recursed through:
    /// it has two access paths below it, not one, and this method's return
    /// type can only ever name a single node. A caller that wants both sides'
    /// access paths inspects `Join`'s `left`/`right` directly.
    pub fn access_path(&self) -> &PhysicalOp {
        if matches!(self, PhysicalOp::Join { .. }) {
            return self;
        }
        match self.child() {
            Some(c) => c.access_path(),
            None => self,
        }
    }

    pub fn explain(&self) -> String {
        fn go(op: &PhysicalOp, depth: usize, out: &mut String) {
            let pad = "  ".repeat(depth);
            let line = match op {
                PhysicalOp::GetById { collection, id } => format!("GetById({collection}, {id})"),
                PhysicalOp::GetByIds { collection, ids } => {
                    format!("GetByIds({collection}, {} ids)", ids.len())
                }
                PhysicalOp::HeapScan { collection } => format!("HeapScan({collection})"),
                PhysicalOp::ColumnScan { collection, fields } => {
                    format!("ColumnScan({collection}: {})", fields.join(", "))
                }
                PhysicalOp::ColumnarTopK {
                    collection,
                    key,
                    descending,
                    k,
                } => format!(
                    "ColumnarTopK({collection}: top {k} by {key}{dir})",
                    dir = if *descending { " desc" } else { "" }
                ),
                PhysicalOp::IndexLookup {
                    collection,
                    field,
                    kind,
                    ..
                } => format!("IndexLookup({collection}.{field} via {})", kind.as_str()),
                PhysicalOp::CoveringLookup {
                    collection,
                    field,
                    needed,
                    ..
                } => format!(
                    "CoveringLookup({collection}.{field} covering {})",
                    needed.join(", ")
                ),
                PhysicalOp::CoveringRange {
                    collection,
                    field,
                    needed,
                    ..
                } => format!(
                    "CoveringRange({collection}.{field} covering {}, via btree)",
                    needed.join(", ")
                ),
                PhysicalOp::IndexRange {
                    collection, field, ..
                } => format!("IndexRange({collection}.{field} via btree)"),
                PhysicalOp::CompositeLookup {
                    collection, fields, ..
                } => format!("CompositeLookup({collection}: {})", fields.join(", ")),
                PhysicalOp::Filter { predicate, .. } => format!("Filter({predicate:?})"),
                PhysicalOp::Project { fields, .. } => format!("Project({})", fields.join(", ")),
                PhysicalOp::Sort { keys, .. } => format!(
                    "Sort({})",
                    keys.iter()
                        .map(|k| format!("{}{}", k.field, if k.descending { " desc" } else { "" }))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                PhysicalOp::Limit { n, .. } => format!("Limit({n})"),
                PhysicalOp::Aggregate { group_by, aggs, .. } => format!(
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
                PhysicalOp::Join { kind, on, .. } => {
                    format!("Join({}, on {} = {})", kind.as_str(), on.0, on.1)
                }
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
pub struct PhysicalPlan {
    pub root: PhysicalOp,
    /// Why the planner chose this access path, for `EXPLAIN` and the decision
    /// log. Recorded at plan time because reconstructing the reasoning
    /// afterwards is guesswork.
    pub rationale: String,
}

impl PhysicalPlan {
    pub fn explain(&self) -> String {
        format!(
            "{}\nrationale: {}\n",
            self.root.explain().trim_end(),
            self.rationale
        )
    }
    pub fn is_full_scan(&self) -> bool {
        self.root.is_full_scan()
    }
}
