//! The database facade.
//!
//! Wires the primary representation (a heap), its derived representations
//! (indexes, caches, directly-addressed arrays), the planner, the executor and
//! the optimization controller into one object.
//!
//! Everything derived here is **rebuildable from the primary**. Indexes, caches
//! and direct arrays can all be dropped at any instant and reconstructed, which
//! is what makes it safe for the controller to switch them on and off under a
//! live workload — and why `Database` can implement `ActionSink` at all.
//!
//! `Database` is a composition of parts, each owning one concept: the derived
//! structures, the query caches, telemetry, the optimizer, experiments,
//! tuning switches and per-query execution state. The struct and the types
//! shared across its responsibilities live here; the methods are split by
//! responsibility into the child modules below.

mod action_sink;
mod constraints;
mod derived;
mod durability;
mod experiment_state;
mod experiments;
mod indexes;
mod introspection;
mod layout;
mod lifecycle;
mod logical_store;
mod optimization;
mod query;
mod query_caches;
mod source;
mod telemetry;
mod tuning;
mod writes;

use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_exec::exec::ExecStats;
use adabt_opt::optimization::OptContext;
use adabt_opt::{OptimizationController, Registry};
use adabt_storage::heap::HeapStore;
use adabt_telemetry::Snapshot;
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;
use std::time::Duration;

use derived::DerivedStructures;
use experiment_state::Experiments;
use query_caches::QueryCaches;
use telemetry::Telemetry;
use tuning::Tuning;

pub struct IndexSpec {
    pub collection: String,
    pub field: String,
    pub kind: IndexKind,
}

/// What a slow-query sink is handed: enough to say what ran and how it went,
/// not the query's own rows — a log meant to run in production has no
/// business holding an application's data in memory a second time just to
/// report that a query was slow.
pub struct SlowQueryEvent {
    pub elapsed: Duration,
    pub rows_scanned: u64,
    pub rows_returned: u64,
    /// `LogicalPlan::explain`'s output — the same text `Database::explain`
    /// already produces, reused rather than inventing a second description
    /// format for what is fundamentally the same question: what did this
    /// query ask for.
    pub explain: String,
}

type SlowQuerySink = Box<dyn FnMut(&SlowQueryEvent) + Send>;

pub struct Database {
    /// The primary representation: the one thing here that is not rebuildable.
    store: HeapStore,
    policy: Policy,
    telemetry: Telemetry,
    derived: DerivedStructures,
    caches: QueryCaches,
    optimizer: Optimizer,
    experiments: Experiments,
    /// Fields where two records may not share a value. See `crate::unique`.
    unique_constraints: crate::unique::UniqueConstraints,
    tuning: Tuning,
    execution: Execution,
    /// The next id `begin` hands out. Purely an in-memory label — see
    /// `crate::transaction` for why nothing about an open transaction needs to
    /// be durable until it commits.
    next_txn_id: u64,
}

/// The optimizer's machinery: which optimizations exist, what has been
/// applied, and the adaptive driver's memory across cycles.
struct Optimizer {
    registry: Registry,
    controller: OptimizationController,
    /// Kept across cycles — cooldowns, what it last changed — so it lives here
    /// rather than being constructed per call.
    adaptive: adabt_opt::AdaptiveDriver,
}

/// What surrounds each run of the executor: the cancel flag it is handed, the
/// slow-query sink it reports to, and the statistics the last run left behind.
struct Execution {
    last_stats: ExecStats,
    /// Set for the duration of one `query_cancellable` call, so `execute`
    /// (reached from many internal call sites — direct queries, canaries,
    /// experiment shadows) does not need a cancel token threaded through all
    /// of them individually. `None` outside that call, which is what makes
    /// every other query path uncancellable by default.
    pending_cancel: Option<Arc<AtomicBool>>,
    /// A threshold and a sink for queries that take at least that long.
    /// `None` is the default: logging every query's wall-clock time is a cost
    /// nobody asked for, so this opts in rather than the reverse.
    slow_query: Option<(Duration, SlowQuerySink)>,
}

/// Everything an `OptContext` borrows, owned.
///
/// `OptContext` is entirely references, so the values behind it must outlive the
/// borrow of `self` that produced them. Gathering them once here is what lets
/// the controller take `&mut self` as a sink while still being handed a
/// description of the database it is about to change.
struct OptInputs {
    snapshot: Snapshot,
    collections: Vec<(String, usize)>,
    filtered: Vec<(String, String, u64)>,
    fixed: Vec<String>,
    existing: Vec<(String, String, IndexKind)>,
    max_ids: Vec<(String, u64)>,
    current_bytes: u64,
    policy: Policy,
}

impl OptInputs {
    fn ctx(&self) -> OptContext<'_> {
        OptContext {
            policy: &self.policy,
            telemetry: &self.snapshot,
            collections: &self.collections,
            filtered_fields: &self.filtered,
            fixed_size_collections: &self.fixed,
            max_ids: &self.max_ids,
            existing_indexes: &self.existing,
            current_bytes: self.current_bytes,
        }
    }
}

/// How much of the general query machinery a call is allowed to use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryMode {
    /// Everything: compiled paths, result cache, plan cache, telemetry.
    Normal,
    /// A query served during a canary. Caches and compiled paths are skipped —
    /// a cached result measures the cache, not the representation, and the plan
    /// cache is keyed by shape while the two paths deliberately plan the same
    /// shape differently. Still counted as the real query it is.
    Served,
    /// One half of a shadow pair. As `Served`, and additionally uncounted: the
    /// pair is one logical query answered twice, and recording both would
    /// double every statistic the optimizer later reads back.
    Trial,
}

impl QueryMode {
    fn uses_caches(&self) -> bool {
        matches!(self, QueryMode::Normal)
    }
    fn is_counted(&self) -> bool {
        !matches!(self, QueryMode::Trial)
    }
}

/// The result of [`Database::verify`]: what was checked and what disagreed.
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    /// One human-readable line per divergence. Empty means consistent —
    /// and "empty" is the contract, so new checks add problems rather than
    /// inventing their own success encodings.
    pub problems: Vec<String>,
    pub records_checked: u64,
    pub indexes_checked: usize,
}
