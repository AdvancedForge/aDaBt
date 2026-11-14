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
    /// The collection this path reads.
    pub fn collection(&self) -> &str {
        match self {
            CompiledPath::DirectById { collection } | CompiledPath::HeapById { collection } => {
                collection
            }
        }
    }

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

/// One compiled call in this many is timed. The rest are only counted: the
/// path is a couple of hundred nanoseconds, and reading the clock twice per
/// call measurably slowed it.
const SAMPLE_EVERY: u64 = 16;

/// Traffic one compiled shape has served since it was last reported.
///
/// A compiled lookup skips the general path's telemetry, and it has to report
/// somehow: a shape whose traffic disappears from telemetry the moment it gets
/// fast is a shape the adaptive driver stops reasoning about. Counting every
/// call and timing a sample keeps that evidence at a fraction of the cost of
/// an event per call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub calls: u64,
    pub rows: u64,
    sampled_nanos: u64,
    samples: u64,
}

impl Tally {
    /// Mean latency of the sampled calls; zero if none were sampled yet.
    pub fn mean_nanos(&self) -> u64 {
        self.sampled_nanos.checked_div(self.samples).unwrap_or(0)
    }
}

/// A compiled call in progress: whether to time it.
#[derive(Debug, Clone, Copy)]
pub struct Call {
    pub timed: bool,
}

struct Compiled {
    path: CompiledPath,
    tally: Tally,
}

/// What can be specialised, and what has been.
#[derive(Default)]
pub struct CompiledPaths {
    paths: HashMap<QueryShape, Compiled>,
    /// Shapes seen but not yet hot enough to specialise.
    counts: HashMap<QueryShape, u64>,
    /// Traffic of paths dropped by [`clear`](Self::clear) and not yet
    /// reported, so a layout change cannot erase evidence.
    retired: Vec<(QueryShape, String, Tally)>,
    pub hits: u64,
    pub compilations: u64,
}

impl CompiledPaths {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a call through the compiled path for `shape`, if there is one.
    pub fn enter(&mut self, shape: QueryShape) -> Option<Call> {
        let compiled = self.paths.get_mut(&shape)?;
        self.hits += 1;
        compiled.tally.calls += 1;
        Some(Call {
            timed: compiled.tally.calls % SAMPLE_EVERY == 1,
        })
    }

    /// Finish a call started with [`enter`](Self::enter): count its rows, and
    /// for a timed call, its latency.
    pub fn leave(&mut self, shape: QueryShape, call: Call, rows: u64, nanos: Option<u64>) {
        if let Some(compiled) = self.paths.get_mut(&shape) {
            compiled.tally.rows += rows;
            if let (true, Some(n)) = (call.timed, nanos) {
                compiled.tally.sampled_nanos += n;
                compiled.tally.samples += 1;
            }
        }
    }

    /// Take every shape's traffic since the last report, with the collection
    /// it reads. Shapes that served nothing are left out.
    pub fn drain_tallies(&mut self) -> Vec<(QueryShape, String, Tally)> {
        let live = self
            .paths
            .iter_mut()
            .filter(|(_, c)| c.tally.calls > 0)
            .map(|(shape, c)| {
                let tally = std::mem::take(&mut c.tally);
                (*shape, c.path.collection().to_string(), tally)
            });
        std::mem::take(&mut self.retired)
            .into_iter()
            .chain(live)
            .collect()
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
        self.paths.insert(
            shape,
            Compiled {
                path,
                tally: Tally::default(),
            },
        );
        self.counts.remove(&shape);
        self.compilations += 1;
    }

    /// Drop every specialisation.
    ///
    /// Called whenever the physical layout changes. A compiled path encodes a
    /// decision about what exists; if a direct array is dropped, a path that
    /// still reaches for it is wrong rather than merely stale.
    pub fn clear(&mut self) {
        let unreported = self.drain_tallies();
        self.retired = unreported;
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
        assert!(c.enter(s).is_some());
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
        assert!(c.enter(QueryShape(1)).is_none());
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

    #[test]
    fn every_call_is_counted_and_one_in_sample_every_is_timed() {
        let mut c = CompiledPaths::new();
        let s = QueryShape(9);
        c.install(
            s,
            CompiledPath::HeapById {
                collection: "users".into(),
            },
        );
        let mut timed = 0;
        for _ in 0..(SAMPLE_EVERY * 4) {
            let call = c.enter(s).unwrap();
            timed += call.timed as u64;
            c.leave(s, call, 1, call.timed.then_some(200));
        }
        assert_eq!(timed, 4);
        let drained = c.drain_tallies();
        assert_eq!(drained.len(), 1);
        let (_, collection, tally) = &drained[0];
        assert_eq!(collection, "users");
        assert_eq!(
            (tally.calls, tally.rows, tally.mean_nanos()),
            (SAMPLE_EVERY * 4, SAMPLE_EVERY * 4, 200)
        );
        assert!(
            c.drain_tallies().is_empty(),
            "a drained tally must not be reported twice"
        );
    }

    #[test]
    fn clearing_compiled_paths_keeps_their_unreported_traffic() {
        let mut c = CompiledPaths::new();
        let s = QueryShape(9);
        c.install(
            s,
            CompiledPath::HeapById {
                collection: "users".into(),
            },
        );
        let call = c.enter(s).unwrap();
        c.leave(s, call, 1, Some(100));
        c.clear();
        assert!(c.enter(s).is_none());
        assert_eq!(c.drain_tallies()[0].2.calls, 1);
    }
}
