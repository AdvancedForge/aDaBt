//! Compiled hot paths.
//!
//! The general query path does a lot per call: record which fields were
//! filtered, probe the result cache, probe the plan cache, rebuild a physical
//! plan, run it through batched operators, insert into the result cache, emit
//! telemetry. Every step earns its place across the range of queries the engine
//! must answer — and none of it is needed to fetch one record by identity from
//! a directly-addressed array.
//!
//! A compiled path is that observation made concrete: for a shape seen often
//! enough to be worth specialising, the general machinery is replaced by the
//! smallest sequence of operations that answers it.
//!
//! # Not a JIT
//!
//! Nothing is generated at runtime. "Compiled" here means *specialised*: a
//! precomputed decision about which minimal path applies, so the per-call work
//! is a match and a lookup rather than a plan construction. That is the honest
//! description, and it is where most of the win in a query compiler comes from
//! anyway — the general path is expensive because it is general, not because it
//! is interpreted.

use adabt_ir::plan::LogicalOp;
use adabt_ir::QueryShape;
use std::collections::HashMap;

/// A specialised way to answer one query shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompiledPath {
    /// One record by identity, straight from a directly-addressed array.
    ///
    /// The endpoint the whole project aims at: no planner, no operators, no
    /// batching, no cache probes. An address calculation and a decode.
    DirectById { collection: String },
    /// One record by identity through the page directory.
    HeapById { collection: String },
}

impl CompiledPath {
    pub fn describe(&self) -> String {
        match self {
            CompiledPath::DirectById { collection } => {
                format!("{collection}: id -> address -> record")
            }
            CompiledPath::HeapById { collection } => {
                format!("{collection}: id -> page directory -> record")
            }
        }
    }
}

/// Calls of one shape before specialising it.
///
/// Specialising a shape seen twice is wasted work; the threshold exists so the
/// cost of deciding is amortised over calls that will actually use it.
pub const HOT_THRESHOLD: u64 = 256;

/// What can be specialised, and what has been.
#[derive(Default)]
pub struct CompiledPaths {
    paths: HashMap<QueryShape, CompiledPath>,
    /// Shapes seen but not yet hot enough to specialise.
    counts: HashMap<QueryShape, u64>,
    pub hits: u64,
    pub compilations: u64,
}

impl CompiledPaths {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&mut self, shape: QueryShape) -> Option<&CompiledPath> {
        let found = self.paths.get(&shape);
        if found.is_some() {
            self.hits += 1;
        }
        found
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Note a call, reporting whether the shape has become worth specialising.
    pub fn observe(&mut self, shape: QueryShape) -> bool {
        if self.paths.contains_key(&shape) {
            return false;
        }
        let n = self.counts.entry(shape).or_default();
        *n += 1;
        *n >= HOT_THRESHOLD
    }

    pub fn install(&mut self, shape: QueryShape, path: CompiledPath) {
        self.paths.insert(shape, path);
        self.counts.remove(&shape);
        self.compilations += 1;
    }

    /// Drop every specialisation.
    ///
    /// Called whenever the physical layout changes. A compiled path encodes a
    /// decision about what exists; if a direct array is dropped, a path that
    /// still reaches for it is wrong rather than merely stale.
    pub fn clear(&mut self) {
        self.paths.clear();
        self.counts.clear();
    }

    /// The specialisation that fits a plan, if any.
    ///
    /// Deliberately narrow. Only shapes whose whole answer is a single record
    /// by identity qualify, because those are the only ones where the general
    /// path can be skipped outright rather than merely shortened.
    pub fn candidate(
        op: &LogicalOp,
        has_direct_array: impl Fn(&str) -> bool,
    ) -> Option<CompiledPath> {
        match op {
            LogicalOp::GetById { collection, .. } => {
                if has_direct_array(collection) {
                    Some(CompiledPath::DirectById {
                        collection: collection.clone(),
                    })
                } else {
                    Some(CompiledPath::HeapById {
                        collection: collection.clone(),
                    })
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::ids::RecordId;
    use adabt_ir::Expr;

    #[test]
    fn a_shape_is_specialised_only_once_it_is_hot() {
        let mut c = CompiledPaths::new();
        let s = QueryShape(1);
        for _ in 0..HOT_THRESHOLD - 1 {
            assert!(!c.observe(s), "specialised too early");
        }
        assert!(c.observe(s), "never became hot");
    }

    #[test]
    fn an_installed_path_stops_being_counted() {
        let mut c = CompiledPaths::new();
        let s = QueryShape(1);
        c.install(
            s,
            CompiledPath::HeapById {
                collection: "u".into(),
            },
        );
        assert!(!c.observe(s), "an installed shape was counted again");
        assert!(c.get(s).is_some());
        assert_eq!(c.hits, 1);
    }

    #[test]
    fn only_identity_lookups_are_specialised() {
        // A filter or a scan still has real work to do; skipping the general
        // path would mean reimplementing it.
        let get = LogicalOp::get("users", RecordId(1));
        assert!(CompiledPaths::candidate(&get, |_| false).is_some());

        let scan = LogicalOp::scan("users");
        assert!(CompiledPaths::candidate(&scan, |_| false).is_none());

        let filtered = LogicalOp::scan("users").filter(Expr::eq("a", 1i64));
        assert!(CompiledPaths::candidate(&filtered, |_| false).is_none());
    }

    #[test]
    fn a_direct_array_selects_the_shorter_path() {
        let get = LogicalOp::get("users", RecordId(1));
        assert_eq!(
            CompiledPaths::candidate(&get, |_| true),
            Some(CompiledPath::DirectById {
                collection: "users".into()
            })
        );
        assert_eq!(
            CompiledPaths::candidate(&get, |_| false),
            Some(CompiledPath::HeapById {
                collection: "users".into()
            })
        );
    }

    #[test]
    fn clearing_removes_paths_and_counts() {
        // A compiled path encodes what exists; if that changes, a path still
        // reaching for the old structure is wrong, not merely stale.
        let mut c = CompiledPaths::new();
        c.install(
            QueryShape(1),
            CompiledPath::DirectById {
                collection: "u".into(),
            },
        );
        c.observe(QueryShape(2));
        c.clear();
        assert!(c.is_empty());
        assert!(c.get(QueryShape(1)).is_none());
        // And the partial count is gone too, so a shape does not jump straight
        // back to hot against a layout that no longer exists.
        assert!(!c.observe(QueryShape(2)));
    }

    #[test]
    fn paths_describe_the_work_they_do() {
        let d = CompiledPath::DirectById {
            collection: "users".into(),
        };
        assert!(d.describe().contains("address"));
        let h = CompiledPath::HeapById {
            collection: "users".into(),
        };
        assert!(h.describe().contains("page directory"));
    }
}
