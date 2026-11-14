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
//! The struct and the types shared across its responsibilities live here;
//! the methods are split by responsibility into the child modules below.

mod action_sink;
mod constraints;
mod durability;
mod experiments;
mod indexes;
mod introspection;
mod layout;
mod lifecycle;
mod logical_store;
mod optimization;
mod query;
mod source;
mod writes;

use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_exec::exec::ExecStats;
use adabt_index::Index;
use adabt_opt::optimization::OptContext;
use adabt_opt::{OptimizationController, Registry};
use adabt_storage::heap::HeapStore;
use adabt_telemetry::{CollectingProbe, Snapshot};
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;
use std::time::Duration;

use crate::caches::{PlanCache, ResultCache};
use crate::column::ColumnStore;
use crate::direct::DirectArray;
use crate::experiment::{Candidates, LiveExperiment};

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
    store: HeapStore,
    indexes: HashMap<String, Vec<Box<dyn Index>>>,
    /// Directly-addressed arrays, where the schema and density allow.
    direct: HashMap<String, DirectArray>,
    /// Columnar copies, for scans and aggregates over a bounded field set.
    columns: HashMap<String, ColumnStore>,
    /// Grouped counts kept up to date on write, so an aggregate costs the number
    /// of groups rather than the number of rows.
    views: crate::matview::MaterializedViews,
    plan_cache: PlanCache,
    result_cache: ResultCache,
    /// Bumped on every write, so a cached result can never be stale.
    epochs: HashMap<String, u64>,
    /// Shapes specialised past the general query path.
    compiled: crate::compiled::CompiledPaths,
    /// Sampling stride for temperature observation. Feeding the sketch on
    /// every read would add a lock to the hot path to measure something only
    /// relative frequency is wanted from, and uniform sampling preserves that.
    touch_sample: std::cell::Cell<u32>,
    policy: Policy,
    registry: Registry,
    controller: OptimizationController,
    adaptive: adabt_opt::AdaptiveDriver,
    probe: Arc<CollectingProbe>,
    last_stats: ExecStats,
    /// The change currently being proved against live traffic.
    /// Experiments running right now, on scopes that do not overlap.
    ///
    /// A `Vec` rather than an `Option` because there is no reason two changes
    /// to *different* collections should have to be proved one after the
    /// other. The reason it was an `Option` was safety, not simplicity: with a
    /// single global candidate mask and a single global "candidates visible"
    /// flag, a second experiment would have had its unproven structures
    /// exposed to the first experiment's canary traffic, and each would have
    /// been measuring the other. Both of those are now per-experiment, so the
    /// remaining requirement is only that their scopes are disjoint.
    experiments: Vec<LiveExperiment>,
    /// The experiment whose query is being run right now, if any.
    ///
    /// Paired with `candidate_visible`: together they name *which* candidate
    /// is allowed to be seen, rather than merely that some candidate is.
    experiment_under_test: Option<u64>,
    /// Experiments that reached a verdict, kept so the reasoning survives them.
    finished: Vec<LiveExperiment>,
    /// Fields where two records may not share a value. See `crate::unique`.
    unique_constraints: crate::unique::UniqueConstraints,
    /// The next id `begin` hands out. Purely an in-memory label — see
    /// `crate::transaction` for why nothing about an open transaction needs to
    /// be durable until it commits.
    next_txn_id: u64,
    /// Structures built for an experiment and not yet trusted.
    ///
    /// Held here rather than on the experiment because the mask has to outlive
    /// a borrow of it: a shadow trial moves the experiment aside to record into
    /// it, and if the mask went with it both halves of the pair would take the
    /// candidate path and the comparison would be of a thing against itself.
    hidden: Candidates,
    /// Whether *this* query may use the hidden structures.
    candidate_visible: bool,
    /// Whether actions passing through the sink are part of a candidate build.
    /// The experiment whose candidate is being built right now, if any.
    ///
    /// Carries the id rather than a bare flag because what gets recorded is
    /// *whose* candidate a structure is. With one experiment the id could be
    /// looked up; with several running, "the experiment" is not a thing, and
    /// attributing a structure to the wrong one would mask it for the wrong
    /// trial.
    recording_candidate: Option<u64>,
    next_experiment_id: u64,
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
    /// Per-collection clustering hints, set with [`Database::declare_cluster_field`]:
    /// the field whose integer value steers *where* a record is placed. A hint,
    /// not a constraint — see that method for exactly what is and is not promised.
    cluster_fields: HashMap<String, String>,
    /// Whether delta-varint encoding is enabled for column stores.
    delta_encoding: bool,
    /// Whether thread-per-core execution is enabled.
    thread_per_core: bool,
    /// Whether join order optimization is enabled.
    join_order: bool,
    /// Whether data-driven partitioning is enabled.
    data_partitioning: bool,
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
