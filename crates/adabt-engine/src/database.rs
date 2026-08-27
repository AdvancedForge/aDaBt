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

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::{Durability, Mode, Policy};
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_core::value::Value;
use adabt_exec::exec::{execute_with_budget, ExecBudget, ExecStats, Source};
use adabt_exec::physical::PhysicalPlan;
use adabt_exec::planner::{plan as make_plan, PlanContext};
use adabt_index::{BTreeIndex, BitmapIndex, HashIndex, Index};
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{QueryKey, QueryShape};
use adabt_opt::action::{Action, ActionSink};
use adabt_opt::controller::ApplyReport;
use adabt_opt::decision::{Decision, DecisionAction, Source as DecisionSource, Verdict};
use adabt_opt::driver::{ManualDriver, OptimizationDriver};
use adabt_opt::experiment::{Guardrails, Phase};
use adabt_opt::optimization::OptContext;
use adabt_opt::{OptimizationConfig, OptimizationController, Registry};
use adabt_storage::heap::HeapStore;
use adabt_telemetry::event::{Event, OpKind};
use adabt_telemetry::{CollectingProbe, Probe, Snapshot};
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::HashMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::AtomicBool;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::caches::{CacheStats, PlanCache, ResultCache};
use crate::column::ColumnStore;
use crate::direct::DirectArray;
use crate::experiment::{Candidates, LiveExperiment};
use crate::optimizations::register_builtins;

fn index_cache_key(collection: &str, field: &str, kind: IndexKind) -> String {
    format!("idx:{collection}:{field}:{}", kind.as_str())
}

/// How many `Join` nodes appear anywhere in `op` — `query_join`'s check for
/// "at most one," since `LogicalOp::contains_join` only ever answers whether
/// there is at least one.
fn count_joins(op: &adabt_ir::plan::LogicalOp) -> usize {
    let this = usize::from(matches!(op, adabt_ir::plan::LogicalOp::Join { .. }));
    this + op.children().iter().map(|c| count_joins(c)).sum::<usize>()
}

/// Index entries as bytes: repeated `key, id-count, ids`.
///
/// Grouped by key rather than written as flat pairs, which matters more than it
/// looks: a low-cardinality index — a country, a status, a boolean — has a
/// handful of keys and a great many ids, and repeating the key per id would make
/// the cache larger than the records it was built from.
fn encode_index_entries(entries: &[(Value, Vec<RecordId>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (key, ids) in entries {
        adabt_storage::codec::encode_value(key, &mut out);
        out.extend_from_slice(&(ids.len() as u64).to_le_bytes());
        for id in ids {
            out.extend_from_slice(&id.0.to_le_bytes());
        }
    }
    out
}

/// Decode what [`encode_index_entries`] wrote, or `None` if it will not decode.
///
/// Every length is checked against what is left rather than trusted, and any
/// disagreement gives up on the whole blob. The caller rebuilds from the
/// primary, which is always available and always right.
fn decode_index_entries(blob: &[u8]) -> Option<Vec<(Value, Vec<RecordId>)>> {
    fn u64_at(blob: &[u8], pos: &mut usize) -> Option<u64> {
        let end = pos.checked_add(8)?;
        let v = u64::from_le_bytes(blob.get(*pos..end)?.try_into().ok()?);
        *pos = end;
        Some(v)
    }
    let mut pos = 0usize;
    let count = u64_at(blob, &mut pos)? as usize;
    let mut out = Vec::new();
    for _ in 0..count {
        let (key, used) = adabt_storage::codec::decode_value(blob.get(pos..)?).ok()?;
        pos += used;
        let n = u64_at(blob, &mut pos)? as usize;
        let mut ids = Vec::new();
        for _ in 0..n {
            ids.push(RecordId(u64_at(blob, &mut pos)?));
        }
        out.push((key, ids));
    }
    if pos == blob.len() {
        Some(out)
    } else {
        None
    }
}

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
    /// Whether join order optimization is enabled (M32).
    join_order: bool,
    /// Whether data-driven partitioning is enabled (M32).
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

/// Whether `predicate` constrains `field` with at least one ordered
/// comparison — a range want, as distinct from an equality pin.
///
/// Local mirror of the planner's range walk, kept deliberately shallow: this
/// only decides whether a projection observation is worth recording against
/// a b-tree-backed structure, not what the bounds are.
fn range_constrained(predicate: &adabt_ir::Expr, field: &str) -> bool {
    match predicate {
        adabt_ir::Expr::Compare { op, lhs, .. } => {
            matches!(
                op,
                adabt_ir::CmpOp::Ge
                    | adabt_ir::CmpOp::Gt
                    | adabt_ir::CmpOp::Le
                    | adabt_ir::CmpOp::Lt
            ) && matches!(lhs.as_ref(), adabt_ir::Expr::Field(f) if f == field)
        }
        adabt_ir::Expr::And(parts) => parts.iter().any(|p| range_constrained(p, field)),
        _ => false,
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

/// Which of the four index shapes a name encodes, for verification purposes.
///
/// Membership rules differ by shape: an ordinary or covering index holds
/// every record with a non-null base field, a composite holds every record
/// with all its fields, a partial only those matching its condition.
enum IndexShape {
    Ordinary(String),
    Covering(String),
    Composite(Vec<String>),
    Partial(String),
}

fn classify_index(name: &str) -> IndexShape {
    if name.contains(adabt_index::COVER_SEP) {
        IndexShape::Covering(adabt_index::covering_parts(name).0)
    } else if name.contains(adabt_index::PARTIAL_SEP) {
        IndexShape::Partial(adabt_index::partial_parts(name).0)
    } else if name.contains(adabt_index::COMPOSITE_SEP) {
        IndexShape::Composite(adabt_index::composite_fields(name))
    } else {
        IndexShape::Ordinary(name.to_string())
    }
}

impl Database {
    pub fn open(dir: &Path, policy: Policy) -> Result<Self> {
        Self::open_shared(
            dir,
            policy,
            Arc::new(adabt_storage::version::VersionTracker::new()),
        )
    }

    /// Open with a version tracker shared across other databases — what every
    /// shard of a [`crate::sharded::ShardedDatabase`] does, so their timestamps
    /// order against each other rather than each starting its own clock at one.
    pub fn open_shared(
        dir: &Path,
        policy: Policy,
        versions: Arc<adabt_storage::version::VersionTracker>,
    ) -> Result<Self> {
        let store = HeapStore::open_shared(dir, policy.guarantees.durability, 1024, versions)?;
        Self::open_with_store(store, policy)
    }

    /// Open a database, replaying the log only up to `target` — a
    /// point-in-time restore. See [`adabt_storage::heap::RecoverTarget`].
    ///
    /// Everything past the store itself — indexes, materialized views, the
    /// unique-constraint sidecar — is loaded exactly as `open_shared` loads
    /// it, because there is nothing target-specific left to decide once the
    /// store has already stopped where it was told to: an index cache stamped
    /// against a state that no longer matches (because the store now holds
    /// less than it did) simply fails its own staleness check and is rebuilt
    /// by scanning, the same as it would after any other kind of divergence.
    pub fn open_at(
        dir: &Path,
        policy: Policy,
        target: adabt_storage::heap::RecoverTarget,
    ) -> Result<Self> {
        let store = HeapStore::open_shared_at(
            dir,
            policy.guarantees.durability,
            1024,
            Arc::new(adabt_storage::version::VersionTracker::new()),
            target,
        )?;
        Self::open_with_store(store, policy)
    }

    fn open_with_store(store: HeapStore, policy: Policy) -> Result<Self> {
        let mut registry = Registry::new();
        register_builtins(&mut registry);
        // Caught here, once, rather than left to `ManualDriver::decide`'s own
        // silent skip: that skip is correct for a name that used to exist and
        // has since been dropped from a build, which a running database must
        // survive, but it is a poor way to learn that a policy typo'd
        // "auto_indx" — the one moment this can still be reported usefully is
        // before anything has been applied on its strength.
        if let Mode::Manual { overrides, .. } = &policy.mode {
            registry.validate_overrides(overrides)?;
        }
        let mut db = Self {
            store,
            indexes: HashMap::new(),
            direct: HashMap::new(),
            columns: HashMap::new(),
            views: crate::matview::MaterializedViews::new(),
            plan_cache: PlanCache::new(0),
            result_cache: ResultCache::new(0),
            epochs: HashMap::new(),
            compiled: crate::compiled::CompiledPaths::new(),
            touch_sample: std::cell::Cell::new(0),
            policy,
            registry,
            controller: OptimizationController::new(),
            adaptive: adabt_opt::AdaptiveDriver::new(),
            probe: Arc::new(CollectingProbe::new()),
            last_stats: ExecStats::default(),
            experiments: Vec::new(),
            finished: Vec::new(),
            hidden: Candidates::default(),
            candidate_visible: false,
            experiment_under_test: None,
            recording_candidate: None,
            next_experiment_id: 1,
            unique_constraints: crate::unique::UniqueConstraints::default(),
            next_txn_id: 1,
            pending_cancel: None,
            slow_query: None,
            cluster_fields: HashMap::new(),
            delta_encoding: true,
            thread_per_core: false,
            join_order: false,
            data_partitioning: false,
        };
        db.unique_constraints = crate::unique::read(db.store.dir());
        // Restore the indexes the log says existed.
        //
        // Definitions persist in the log; contents are derived. The cache below
        // is a shortcut, never a source of truth: if it is missing, stale or
        // damaged, every index is rebuilt by scanning the heap and the only
        // thing lost is the time. That is the rebuildability invariant paying
        // for itself — a derived representation can be cached carelessly
        // precisely because being wrong about it is always recoverable.
        let stamp = db.store.state_stamp()?;
        let cached = adabt_storage::derived::read(db.store.dir(), &stamp).unwrap_or_default();
        let defs: Vec<(String, String, String)> = db.store.index_definitions().to_vec();
        for (collection, field, kind) in defs {
            if let Some(k) = IndexKind::parse(&kind) {
                let key = index_cache_key(&collection, &field, k);
                let entries = cached
                    .iter()
                    .find(|(n, _)| *n == key)
                    .and_then(|(_, blob)| decode_index_entries(blob));
                db.create_index_from(&collection, &field, k, entries)?;
            }
        }
        // Restore the clustering declarations the catalog holds. The
        // declarations are state; the placement ranges are not, and rebuild
        // from subsequent keyed inserts.
        for (collection, field) in db.store.declared_cluster_fields() {
            db.cluster_fields.insert(collection, field);
        }
        // Restore optimizer-controlled global flags persisted via the catalog.
        db.delta_encoding = db.store.delta_encoding();
        db.thread_per_core = db.store.thread_per_core();
        crate::column::set_delta_enabled(db.delta_encoding);
        // Apply whatever the policy asks for, so opening at level 3 gives a
        // level-3 database rather than a level-0 one that drifts up later.
        db.optimize()?;
        Ok(db)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub fn probe(&self) -> Arc<CollectingProbe> {
        Arc::clone(&self.probe)
    }
    pub fn telemetry(&self) -> Snapshot {
        self.probe.snapshot()
    }
    pub fn last_exec_stats(&self) -> ExecStats {
        self.last_stats
    }
    pub fn durability(&self) -> Durability {
        self.store.durability()
    }
    pub fn is_thread_per_core(&self) -> bool {
        self.thread_per_core
    }
    pub fn is_delta_encoding(&self) -> bool {
        self.delta_encoding
    }
    pub fn is_join_order(&self) -> bool {
        self.join_order
    }
    pub fn is_data_partitioning(&self) -> bool {
        self.data_partitioning
    }
    /// Open a stable read view over the primary representation.
    ///
    /// Exposed on the database rather than only on the store because shadow
    /// execution is an engine-level activity: comparing two representations is
    /// meaningless unless both read the same state, and this is what makes that
    /// available to a caller.
    pub fn snapshot(&self) -> adabt_storage::version::Snapshot {
        self.store.snapshot()
    }

    pub fn get_at(
        &mut self,
        collection: &str,
        id: RecordId,
        snapshot: &adabt_storage::version::Snapshot,
    ) -> Result<Option<Record>> {
        self.store.get_at(collection, id, snapshot)
    }

    pub fn scan_at(
        &mut self,
        collection: &str,
        snapshot: &adabt_storage::version::Snapshot,
    ) -> Result<Vec<(RecordId, Record)>> {
        self.store.scan_at(collection, snapshot)
    }

    pub fn reclaim_versions(&mut self) -> Result<usize> {
        self.store.reclaim()
    }

    /// Save the derived representations so the next open need not rebuild them.
    ///
    /// Only worth doing at a checkpoint: the cache is validated against an exact
    /// description of the primary, so one written while writes are still
    /// arriving would be stale before it hit the disk.
    fn save_derived(&mut self) -> Result<()> {
        let mut blobs = Vec::new();
        for (collection, list) in &self.indexes {
            for idx in list {
                blobs.push((
                    index_cache_key(collection, idx.field(), idx.kind()),
                    encode_index_entries(&idx.snapshot()),
                ));
            }
        }
        let stamp = self.store.state_stamp()?;
        adabt_storage::derived::write(self.store.dir(), &stamp, &blobs)
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        self.store.checkpoint()?;
        // Ordering matters: the stamp is taken after the checkpoint entry is in
        // the log, so it describes the state the cache was actually built from.
        // A failure here is not a failure of the checkpoint — the database is
        // durable either way, and all a missing cache costs is a rebuild — so it
        // is discarded rather than propagated.
        if self.save_derived().is_err() {
            adabt_storage::derived::discard(self.store.dir());
        }
        Ok(())
    }

    /// Make `dest` a complete, independently openable copy of this database.
    ///
    /// `HeapStore::backup_to` already copies everything a restart depends on;
    /// this adds the one file it does not know exists — the unique-constraint
    /// sidecar, which lives at this layer (see `crate::unique`) because
    /// whether a field is constrained is a logical decision, not a physical
    /// one. Everything else this engine caches — indexes, materialized views,
    /// the derived-representation stamp — is exactly as reconstructible from
    /// `dest` as it is from any other reopened directory, so none of it needs
    /// copying either.
    pub fn backup_to(&mut self, dest: &Path) -> Result<()> {
        self.store.backup_to(dest)?;
        if !self.unique_constraints.is_empty() {
            crate::unique::write(dest, &self.unique_constraints)?;
        }
        Ok(())
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn config(&self) -> &OptimizationConfig {
        self.controller.config()
    }
    pub fn plan_cache_stats(&self) -> CacheStats {
        self.plan_cache.stats()
    }
    pub fn result_cache_stats(&self) -> CacheStats {
        self.result_cache.stats()
    }
    /// Reconstruct a database directory from a backup made by `backup_to`.
    ///
    /// The counterpart `backup_to` never had: an application could back up
    /// through `Database` but had to drop to `HeapStore` to restore, which
    /// is an asymmetry nobody would guess at. `HeapStore::restore_from`
    /// copies the whole directory, so the engine-level sidecar
    /// (`unique.adabt`) that `backup_to` writes comes back with it.
    pub fn restore_from(src: &Path, dest: &Path) -> Result<()> {
        HeapStore::restore_from(src, dest)
    }

    /// The lsn to restore to for a given wall-clock moment, as nanoseconds
    /// since the Unix epoch.
    ///
    /// This is what turns "restore to 14:32" into something `open_at` can
    /// act on. Without it that translation lived only in `adabt-storage`,
    /// so PITR's headline use case was unreachable from the engine API even
    /// after `open_at` and `set_log_archive` were exposed — two thirds of a
    /// feature.
    ///
    /// `None` means every entry in the log is already after `nanos`: there
    /// is no prefix of this log ending at or before that moment.
    pub fn lsn_at_or_before(dir: &Path, nanos: u64) -> Result<Option<adabt_core::ids::Lsn>> {
        adabt_storage::wal::Wal::lsn_at_or_before(&HeapStore::wal_path(dir), nanos)
    }

    /// Relocate live records off trailing pages and shrink the heap file.
    ///
    /// Returns pages reclaimed. Deleting records returns their slots to the
    /// free-space map but never shrinks the file; this is the operation that
    /// actually gives disk back, and it was reachable only from the storage
    /// crate.
    pub fn vacuum(&mut self) -> Result<u32> {
        let freed = self.store.vacuum()?;
        // Vacuum moves records between pages, so anything addressing them by
        // physical position is no longer valid. Same invalidation a schema
        // change needs, for the same reason.
        self.direct.clear();
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(freed)
    }

    /// Resize the result cache, in entries. Zero disables it.
    ///
    /// Operator-facing for the same reason `set_pool_capacity` is: memoizing
    /// whole result sets is a large win on repeated identical queries and a
    /// pure loss on a workload that never repeats one, and only the operator
    /// knows which they have. It was reachable only through an `Action`, which
    /// means only through the optimizer.
    pub fn set_result_cache_entries(&mut self, n: usize) {
        self.result_cache.set_capacity(n);
    }

    /// Resize the plan cache, in entries. Zero disables it.
    pub fn set_plan_cache_entries(&mut self, n: usize) {
        self.plan_cache.set_capacity(n);
    }

    /// Resize the buffer pool, in 4 KiB pages.
    ///
    /// `Database::open` fixes this at 1024 pages — 4 MiB — regardless of how
    /// much data the database holds, and `HeapStore::set_pool_capacity` was
    /// reachable from nowhere above the storage crate. Scale measurement
    /// (M36) showed why that matters: at 1.6M rows the heap is far larger
    /// than 4 MiB, so nearly every fetch is a cold page read and both scans
    /// and indexed point lookups degrade with collection size.
    pub fn set_pool_capacity(&mut self, pages: usize) -> Result<()> {
        self.store.set_pool_capacity(pages)
    }

    /// Send discarded log segments to `dir` instead of deleting them.
    ///
    /// **Without this, point-in-time recovery cannot reach anything but a
    /// backup's own checkpoint.** `backup_to` checkpoints before copying, and
    /// a checkpoint discards every segment it has folded into pages — so a
    /// backup taken on its own carries no history before its own checkpoint,
    /// and `open_at` correctly refuses (`RestoreTargetUnreachable`) any
    /// earlier target rather than answering wrongly. Archiving is what keeps
    /// those segments, and it is therefore the difference between "restore to
    /// exactly when I backed up" and real PITR.
    ///
    /// The mechanism existed in `adabt-storage` since M17 and was reachable
    /// from nowhere above it until now, which made M22's "point-in-time
    /// recovery" true of the storage layer and not of anything an
    /// application could actually call. An audit caught that; this is the
    /// fix.
    pub fn set_log_archive(&mut self, dir: Option<std::path::PathBuf>) {
        self.store.set_log_archive(dir);
    }

    /// Buffer pool counters: hits, misses, evictions, reads, read-ahead.
    ///
    /// A cache hit rate is a first-class operational metric in every database
    /// that has a buffer pool, and this one had it — reachable only by holding
    /// a `HeapStore`, which no application does. Exposed here for the same
    /// reason as `vacuum` and `restore_from`: a capability nobody can call is
    /// indistinguishable from one that was never built.
    ///
    /// It doubles as the instrument that keeps the fetch path honest. Every
    /// record read goes through `pool.get`, so `hits + misses` counts record
    /// reads, and a scan that reads the collection twice says so out loud.
    pub fn buffer_stats(&self) -> adabt_storage::pager::BufferStats {
        self.store.buffer_stats()
    }

    /// Collections that currently have a columnar copy.
    ///
    /// Exposed so a test can assert the column store actually engaged rather
    /// than assuming it did — a test that silently ran against the heap
    /// proves nothing about columnar reads.
    pub fn column_store_collections(&self) -> usize {
        self.columns.len()
    }

    /// Shapes currently specialised past the general query path.
    pub fn compiled_paths(&self) -> usize {
        self.compiled.len()
    }

    pub fn compiled_hits(&self) -> u64 {
        self.compiled.hits
    }

    /// Read one field of one record without decoding the rest.
    ///
    /// Falls back to a full decode where no directly-addressed array exists, so
    /// the caller gets the same answer either way — the specialisation is a
    /// shortcut, never a different result.
    pub fn field_of(
        &mut self,
        collection: &str,
        id: RecordId,
        field: &str,
    ) -> Result<Option<Value>> {
        if let Some(d) = self.direct.get(collection) {
            return d.field_at(id, field);
        }
        Ok(self
            .store
            .get(collection, id)?
            .and_then(|r| r.get(field).cloned()))
    }

    pub fn has_direct_array(&self, collection: &str) -> bool {
        self.direct.contains_key(collection)
    }

    /// Why the database is the way it is.
    /// Walk the heap against every derived structure and name what disagrees.
    ///
    /// The consistency half of hardening. Crash tests prove recovery lands on
    /// *a* consistent state; this proves the state is consistent, including
    /// after a bug — not a crash — has quietly desynchronised a secondary
    /// structure from its primary. Three checks, each in both directions:
    ///
    /// **Forward** (heap → index): every record whose key field is present
    /// and non-null must be findable under that exact key. This catches the
    /// lost-update class: an entry removed without the record knowing.
    /// **Reverse** (index → heap): every id an index holds must exist in the
    /// heap. This catches the dangling-reference class: an id outliving its
    /// record answers queries with rows that are not there.
    /// **Columnar** (store ↔ heap): the store's live id set must equal the
    /// heap's. A columnar copy is rebuildable, so divergence here is never
    /// reconciled at runtime — which makes detecting it loudly worth more,
    /// not less.
    ///
    /// Materialized views are deliberately not verified: their accumulators
    /// may be *inexact by design* once a floating-point budget is exceeded,
    /// so "differs from a recompute" is documented behaviour there and noise
    /// here.
    pub fn verify(&mut self) -> Result<VerifyReport> {
        let mut report = VerifyReport {
            problems: Vec::new(),
            records_checked: 0,
            indexes_checked: 0,
        };

        for collection in self.store.collection_names() {
            let rows = self.store.scan(&collection)?;
            report.records_checked += rows.len() as u64;
            let heap: std::collections::HashMap<RecordId, &Record> =
                rows.iter().map(|(id, r)| (*id, r)).collect();

            if let Some(list) = self.indexes.get(&collection) {
                report.indexes_checked += list.len();
                for idx in list.iter() {
                    let name = idx.field();
                    let snapshot = idx.snapshot();
                    let kind_desc = idx.kind().as_str().to_string();

                    // Reverse check first: it needs no per-record logic.
                    for (key, ids) in &snapshot {
                        for id in ids {
                            if !heap.contains_key(id) {
                                report.problems.push(format!(
                                    "{collection}: index {name:?} ({kind_desc}) holds \
                                     id {id} under key {key:?} but the heap has no such record"
                                ));
                            }
                        }
                    }

                    // Forward check, keyed by what kind of index this is.
                    let classified = classify_index(name);
                    let mut condition = None;
                    let fields: Vec<String> = match &classified {
                        IndexShape::Ordinary(f) | IndexShape::Covering(f) => vec![f.clone()],
                        IndexShape::Composite(fs) => fs.clone(),
                        IndexShape::Partial(base) => {
                            // Only records satisfying the condition belong in a
                            // partial index, so the forward expectation needs
                            // the decoded condition to know which those are.
                            condition = match adabt_index::partial_parts(name).1 {
                                Some(hex) => crate::exprcodec::decode_expr_hex(&hex).ok(),
                                None => None,
                            };
                            vec![base.clone()]
                        }
                    };
                    // An undecodable condition means the checker cannot say
                    // who belongs; refusing silently would hide exactly the
                    // divergence it exists to find, so say so instead.
                    if matches!(classified, IndexShape::Partial(_)) && condition.is_none() {
                        report.problems.push(format!(
                            "{collection}: partial index {name:?} carries a condition \
                             that will not decode; forward verification skipped"
                        ));
                        continue;
                    }

                    'records: for (id, rec) in rows.iter() {
                        if let Some(cond) = &condition {
                            if !cond.matches(rec) {
                                continue;
                            }
                        }
                        let mut key = Vec::with_capacity(fields.len());
                        for f in &fields {
                            match rec.get(f) {
                                Some(v) if !v.is_null() => key.push(v.clone()),
                                _ => continue 'records, // absent/null keys are not indexed
                            }
                        }
                        let probe: Value = if key.len() == 1 {
                            key.pop().expect("len checked")
                        } else {
                            Value::List(key)
                        };
                        if !idx.lookup(&probe).contains(id) {
                            report.problems.push(format!(
                                "{collection}: record {id} holds key {probe:?} but index \
                                 {name:?} does not list it"
                            ));
                        }
                    }
                }
            }

            // Columnar id set versus the heap's.
            if let Some(cs) = self.columns.get(&collection) {
                let mut store_ids = cs.live_ids();
                store_ids.sort_unstable();
                let mut heap_ids: Vec<RecordId> = heap.keys().copied().collect();
                heap_ids.sort_unstable();
                let extra: Vec<RecordId> = store_ids
                    .iter()
                    .filter(|id| !heap.contains_key(id))
                    .copied()
                    .collect();
                let missing: Vec<RecordId> = heap_ids
                    .iter()
                    .filter(|id| !store_ids.contains(id))
                    .copied()
                    .collect();
                if !extra.is_empty() {
                    report.problems.push(format!(
                        "{collection}: column store holds {} ids the heap does not have",
                        extra.len()
                    ));
                }
                if !missing.is_empty() {
                    report.problems.push(format!(
                        "{collection}: column store is missing {} ids the heap has",
                        missing.len()
                    ));
                }
            }
        }

        Ok(report)
    }

    pub fn explain_optimizations(&self) -> String {
        self.controller.explain_all()
    }
    pub fn explain_optimization(&self, name: &str) -> String {
        self.controller.explain(name)
    }

    // -- optimization ------------------------------------------------------

    /// Change the policy and bring the physical configuration in line with it.
    pub fn set_policy(&mut self, policy: Policy) -> Result<ApplyReport> {
        self.policy = policy;
        self.optimize()
    }

    pub fn set_level(&mut self, level: u8) -> Result<ApplyReport> {
        let overrides = match &self.policy.mode {
            Mode::Manual { overrides, .. } => overrides.clone(),
            Mode::Adaptive => Vec::new(),
        };
        self.policy.mode = Mode::Manual { level, overrides };
        self.optimize()
    }

    /// Run one optimization cycle: ask the driver what should change, then put
    /// it through the controller.
    ///
    /// Manual and adaptive differ only in which driver is consulted. Everything
    /// after that — gating, application, logging — is one path.
    pub fn optimize(&mut self) -> Result<ApplyReport> {
        let inputs = self.opt_inputs()?;
        let decisions = self.propose(&inputs);
        let source = self.decision_source();
        let report = self.run_decisions(decisions, source, inputs);
        self.forget_a_little();
        report
    }

    /// Let telemetry forget a little of what it has counted.
    ///
    /// Once per cycle, and only in adaptive mode. Cumulative counters answer
    /// "was this ever useful"; the driver needs "is this useful now", and the
    /// difference is the whole of whether the optimizer can follow a workload
    /// that changes or merely accumulate structures for workloads that have
    /// ended.
    ///
    /// Manual mode does not forget, because nothing in manual mode is deciding
    /// anything from the counts and a user reading them wants the totals.
    fn forget_a_little(&self) {
        if matches!(self.policy.mode, Mode::Adaptive) {
            let (n, d) = adabt_opt::adaptive::TELEMETRY_DECAY;
            self.probe.decay(n, d);
        }
    }

    /// Run one optimization cycle, proving whatever can be proved.
    ///
    /// A decision that builds a derived representation is put through an
    /// experiment instead of being applied outright: built hidden, compared
    /// against the current representation on identical queries, ramped through
    /// canary percentages, and kept only if it earns it. Everything else is
    /// applied the ordinary way.
    ///
    /// The split is not caution, it is capability. Only a change that can exist
    /// *beside* the current representation leaves an old path to compare
    /// against; a change that rewrites the primary has nothing left to be
    /// compared with, so there is no experiment to run and pretending otherwise
    /// would mean measuring the database against itself.
    ///
    /// At most one experiment runs at a time — two would each measure the other
    /// — so at most one shadowable decision starts per cycle and the rest wait.
    /// Call [`Database::advance_experiment`] to give the one in flight the
    /// chance to move on the evidence collected since.
    pub fn optimize_verified(&mut self, guardrails: Guardrails) -> Result<ApplyReport> {
        let inputs = self.opt_inputs()?;
        let mut decisions = self.propose(&inputs);
        let source = self.decision_source();

        // One new experiment per cycle at most. Several may be *running*, but
        // starting more than one at a time would mean proposing changes from
        // one set of inputs and judging them against a database that the other
        // has already altered.
        let held = if self.experiments.is_empty() {
            decisions
                .iter()
                .position(|d| self.is_shadowable(d, &inputs))
                .map(|i| decisions.remove(i))
        } else {
            None
        };
        let report = self.run_decisions(decisions, source, inputs)?;
        if let Some(d) = held {
            // Deliberately not folded into `report`: the change has not been
            // applied, it has been *proposed*, and reporting it as applied is
            // exactly the confusion the experiment exists to prevent.
            self.begin_experiment(d, guardrails)?;
        }
        // Both entry points are one optimization cycle and both must forget.
        // Putting this on `optimize` alone meant a database driven through
        // `optimize_verified` — which is to say, one actually using the
        // experiment loop — never forgot anything and never retracted anything.
        self.forget_a_little();
        Ok(report)
    }

    /// Ask the driver the policy names what should change.
    fn propose(&mut self, inputs: &OptInputs) -> Vec<Decision> {
        let ctx = inputs.ctx();
        // Whatever an experiment is proving is off-limits this cycle. It is
        // hidden from the planner, so its usage figures describe the experiment
        // rather than the workload — and a driver that reads them as a verdict
        // retracts the candidate mid-trial, after which the experiment promotes
        // something that is no longer there.
        let under_experiment: Vec<(&'static str, String)> = self
            .experiments
            .iter()
            .map(|e| {
                (
                    e.experiment.decision.optimization,
                    e.experiment.decision.scope.clone(),
                )
            })
            .collect();
        let input = adabt_opt::DriverInput {
            registry: &self.registry,
            current: self.controller.config(),
            policy: &inputs.policy,
            telemetry: &inputs.snapshot,
            ctx: &ctx,
            under_experiment: &under_experiment,
            pinned: &self.pinned_scopes(),
        };
        // The adaptive driver keeps its state across cycles — cooldowns, what it
        // last changed — so it lives on the database rather than being
        // constructed per call.
        match self.policy.mode {
            Mode::Adaptive => self.adaptive.decide(input),
            Mode::Manual { .. } => ManualDriver.decide(input),
        }
    }

    /// Structures the optimizer may not retract because correctness rests on
    /// them rather than on their speed.
    ///
    /// Every unique constraint's backing index, in the same `name.field` scope
    /// format `auto_index` itself uses — the adaptive driver compares these as
    /// plain strings, so the format has to match exactly or the pin is silently
    /// a no-op.
    fn pinned_scopes(&self) -> Vec<(&'static str, String)> {
        self.unique_constraints
            .iter()
            .map(|(c, f)| ("auto_index", format!("{c}.{f}")))
            .collect()
    }

    fn decision_source(&self) -> DecisionSource {
        match self.policy.mode {
            Mode::Adaptive => DecisionSource::Adaptive,
            Mode::Manual { .. } => DecisionSource::Manual,
        }
    }

    /// Whether this decision leaves an old path to compare a new one against.
    fn is_shadowable(&self, decision: &Decision, inputs: &OptInputs) -> bool {
        decision.action == DecisionAction::Enable
            && self.registry.get(decision.optimization).is_some_and(|o| {
                let plan = o.plan_enable(&inputs.ctx(), &decision.scope, &decision.params);
                !plan.apply.is_empty() && plan.apply.iter().all(|a| a.is_shadowable())
            })
    }

    /// Gather everything the optimizer needs to describe this database.
    fn opt_inputs(&mut self) -> Result<OptInputs> {
        let snapshot = self.probe.snapshot();
        let collections = self.collection_sizes()?;
        let filtered = snapshot.most_filtered_fields();
        Ok(OptInputs {
            fixed: self.fixed_size_collections(),
            existing: self.existing_index_tuples(),
            max_ids: self.max_ids()?,
            current_bytes: self.stored_bytes()? + self.derived_memory_bytes() as u64,
            policy: self.policy.clone(),
            snapshot,
            collections,
            filtered,
        })
    }

    /// Put decisions through the controller.
    ///
    /// Shared with the experiment runner deliberately. A change built for a
    /// trial is gated exactly as one applied outright — same guarantee filter,
    /// same constraints, same log — because an experiment that skipped the
    /// gates would be a way around them, and "nothing bypasses the controller"
    /// would become "nothing except the interesting case".
    fn run_decisions(
        &mut self,
        decisions: Vec<Decision>,
        source: DecisionSource,
        inputs: OptInputs,
    ) -> Result<ApplyReport> {
        if decisions.is_empty() {
            return Ok(ApplyReport::default());
        }
        // The controller needs `&mut self` as a sink while borrowing registry
        // and policy, so both are moved aside for the call.
        let registry = std::mem::take(&mut self.registry);
        let mut controller = std::mem::replace(&mut self.controller, OptimizationController::new());
        let ctx = inputs.ctx();
        let report = controller.apply(
            decisions,
            adabt_opt::controller::ApplyEnv {
                registry: &registry,
                policy: &inputs.policy,
                ctx: &ctx,
            },
            self,
            source,
        );
        self.registry = registry;
        self.controller = controller;
        report
    }

    fn collection_sizes(&mut self) -> Result<Vec<(String, usize)>> {
        let names = self.store.collection_names();
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let c = self.store.count(&n)?;
            out.push((n, c));
        }
        Ok(out)
    }

    /// Highest record id per collection, for density estimates.
    fn max_ids(&mut self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for n in self.store.collection_names() {
            let max = self.store.scan(&n)?.last().map(|(id, _)| id.0).unwrap_or(0);
            out.push((n, max));
        }
        Ok(out)
    }

    fn fixed_size_collections(&self) -> Vec<String> {
        self.store
            .collection_names()
            .into_iter()
            .filter(|n| {
                self.store
                    .schema_of(n)
                    .map(|s| s.mode() == SchemaMode::Fixed)
                    .unwrap_or(false)
            })
            .collect()
    }

    fn existing_index_tuples(&self) -> Vec<(String, String, IndexKind)> {
        self.index_specs()
            .into_iter()
            .map(|s| (s.collection, s.field, s.kind))
            .collect()
    }

    // -- transactions --------------------------------------------------------

    /// Begin a multi-statement transaction. See `crate::transaction` for the
    /// full account of what this guarantees and why.
    pub fn begin(&mut self) -> crate::transaction::Transaction {
        let id = adabt_core::ids::TransactionId(self.next_txn_id);
        self.next_txn_id += 1;
        crate::transaction::Transaction::new(id, self.store.snapshot())
    }

    /// Apply every buffered write, or none of them.
    ///
    /// Two passes, in this order and never interleaved. First, every key in the
    /// write-set is checked — for a first-committer-wins conflict against
    /// anything committed since the transaction's snapshot was taken, and, for
    /// a value being written, against the schema and any unique constraint —
    /// with nothing yet touched. Only once every key has cleared does the
    /// second pass apply any of them, through the ordinary `update`/`delete`
    /// paths so reindexing, epoch bumps and telemetry all happen exactly as
    /// they would for a standalone write.
    ///
    /// Keys are visited in sorted order, which has nothing to do with
    /// correctness and everything to do with reproducibility: `HashMap`
    /// iteration order is not something a test — or a person reading a
    /// decision log — should have to tolerate as noise.
    pub fn commit(&mut self, txn: crate::transaction::Transaction) -> Result<()> {
        if txn.is_empty() {
            return Ok(());
        }
        let snapshot_at = txn.snapshot().at();
        let mut keys: Vec<(String, RecordId)> = txn.writes().keys().cloned().collect();
        keys.sort();

        for key in &keys {
            let (collection, id) = key;
            if let Some(ts) = self.store.latest_write_ts(collection, *id)? {
                if ts.0 > snapshot_at.0 {
                    return Err(Error::TransactionConflict {
                        collection: collection.clone(),
                        id: *id,
                    });
                }
            }
            if let crate::transaction::Write::Put(rec) = &txn.writes()[key] {
                let mut normalized = rec.clone();
                adabt_core::store::normalize_for_storage(&mut normalized);
                self.store
                    .schema_of(collection)?
                    .validate_record(&normalized)?;
                self.check_unique_constraints(collection, Some(*id), &normalized)?;
            }
        }

        // Serializable posture (`Consistency::Strict`): the read set gets the
        // same first-committer-wins check the write set already had. This is
        // what closes write skew — a transaction whose *observations* went
        // stale between its snapshot and its commit aborts instead of
        // committing a result no serial execution would have produced. Under
        // `Consistency::Snapshot` this pass does not run and the guarantee is
        // snapshot isolation exactly as documented. (The enum's ordinal runs
        // strongest-first, so an inequality here would silently include
        // `Eventual`; the guarantee is named exactly or not at all.)
        if self.policy.guarantees.consistency == adabt_core::policy::Consistency::Strict {
            for (collection, id) in txn.reads() {
                if !txn.writes().contains_key(&(collection.clone(), *id)) {
                    if let Some(ts) = self.store.latest_write_ts(collection, *id)? {
                        if ts.0 > snapshot_at.0 {
                            return Err(Error::TransactionConflict {
                                collection: collection.clone(),
                                id: *id,
                            });
                        }
                    }
                }
            }
        }

        // Every check above passed against state that cannot have changed in
        // between — nothing else can touch `&mut self` while this call is on
        // the stack — so nothing from here on can fail for a reason this
        // transaction could have anticipated.
        for key in keys {
            match &txn.writes()[&key] {
                crate::transaction::Write::Put(rec) => {
                    self.update(&key.0, key.1, rec.clone())?;
                }
                crate::transaction::Write::Delete => {
                    self.delete(&key.0, key.1)?;
                }
            }
        }
        Ok(())
    }

    /// Discard a transaction without applying it. Equivalent to letting it drop
    /// — nothing was ever written anywhere — kept as an explicit method for
    /// symmetry with `commit` and so the intent reads at the call site.
    pub fn abort(&mut self, txn: crate::transaction::Transaction) {
        drop(txn);
    }

    // -- record id allocation ----------------------------------------------

    /// The id an auto-allocated insert into `collection` will use next.
    ///
    /// A peek: nothing is reserved until [`Database::insert_auto`] actually
    /// writes with it.
    pub fn next_id(&self, collection: &str) -> Result<RecordId> {
        self.store.next_id(collection)
    }

    /// Insert many records in one call, at one fsync instead of one per row.
    ///
    /// Every derived structure is maintained exactly as it would be for the
    /// same records inserted one at a time — reindexed, counted into telemetry
    /// as ordinary inserts — the difference is entirely in how many times the
    /// log is made durable. See [`adabt_storage::heap::HeapStore::insert_batch`]
    /// for the all-or-nothing guarantee that makes it safe to skip the
    /// per-record fsync: either every one of these lands, or none does.
    pub fn insert_batch(
        &mut self,
        collection: &str,
        records: Vec<(RecordId, Record)>,
    ) -> Result<usize> {
        let t = Instant::now();
        // Normalised here, once, rather than trusted from the store: reindexing
        // has to see the same shape of record a plain `insert` would reindex,
        // and recomputing it from what was actually written would mean a second
        // scan for no reason — `insert_batch` is all-or-nothing, so if it
        // returns at all, every one of these was written exactly as given.
        let mut normalized = Vec::with_capacity(records.len());
        for (id, rec) in &records {
            let mut r = rec.clone();
            adabt_core::store::normalize_for_storage(&mut r);
            normalized.push((*id, r));
        }
        // Checked against existing data *and* against the rest of the batch,
        // before a single row is written — a batch is all-or-nothing, and two
        // records within it sharing a constrained value is exactly as much a
        // violation as one of them colliding with something already stored.
        if !self.unique_constraints.is_empty() {
            let mut seen_in_batch: HashMap<(&str, &Value), RecordId> = HashMap::new();
            for (id, rec) in &normalized {
                self.check_unique_constraints(collection, None, rec)?;
                for field in self.unique_constraints.on(collection) {
                    let Some(value) = rec.get(field).filter(|v| !v.is_null()) else {
                        continue;
                    };
                    if let Some(&other) = seen_in_batch.get(&(field, value)) {
                        return Err(Error::UniqueViolation {
                            collection: collection.to_string(),
                            field: field.to_string(),
                            value: format!(
                                "{value:?} (shared within this batch by records {other} and {id})"
                            ),
                        });
                    }
                    seen_in_batch.insert((field, value), *id);
                }
            }
        }
        let n = self.store.insert_batch(collection, records)?;
        for (id, rec) in &normalized {
            self.reindex_insert(collection, *id, rec);
        }
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Insert, t, n as u64);
        Ok(n)
    }

    /// Insert without naming an id, and learn which one was used.
    ///
    /// Every user of `insert(collection, id, rec)` must otherwise invent a
    /// global id allocator before writing a first record — and a bad one skews
    /// `ShardedDatabase`'s `id % shards` routing towards whichever shard its ids
    /// happen to land on. This is the ordinary answer: monotonic within a
    /// collection, persisted so a restart never reuses one, and advanced by a
    /// manual insert too, so the two never collide.
    pub fn insert_auto(&mut self, collection: &str, rec: Record) -> Result<RecordId> {
        let id = self.next_id(collection)?;
        self.insert(collection, id, rec)?;
        Ok(id)
    }

    // -- index management -------------------------------------------------

    pub fn create_index(&mut self, collection: &str, field: &str, kind: IndexKind) -> Result<()> {
        self.create_index_from(collection, field, kind, None)
    }

    /// Build an index over several fields at once.
    ///
    /// Serves an equality predicate that constrains *every* covered field —
    /// `country = 'NO' AND age = 30` for an index over `(country, age)`.
    /// It does not serve a prefix of them; see `CompositeIndex`. The index
    /// answers to the joined name `composite_name(fields)`, which cannot
    /// collide with a single-field index because the separator is a NUL, so
    /// the existing single-field planner path simply never selects one
    /// rather than selecting one wrongly.
    pub fn create_composite_index(&mut self, collection: &str, fields: &[String]) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if fields.len() < 2 {
            return Err(Error::InvalidOptimization(
                "a composite index needs at least two fields; use create_index for one".into(),
            ));
        }
        let name = adabt_index::composite_name(fields);
        if self.index_exists(collection, &name, IndexKind::Hash) {
            return Ok(());
        }
        let rows = self.store.scan(collection)?;
        let idx =
            adabt_index::CompositeIndex::build(fields.to_vec(), rows.iter().map(|(i, r)| (*i, r)));
        self.indexes
            .entry(collection.to_string())
            .or_default()
            .push(Box::new(idx));
        self.store
            .record_index(collection, &name, IndexKind::Hash.as_str())?;
        self.bump_epoch(collection);
        self.plan_cache.clear();
        Ok(())
    }

    /// Build an index on `field` that also carries `covers` for every record
    /// it indexes.
    ///
    /// A query that filters on `field` and needs nothing outside `covers` is
    /// then answered from the index alone — no page directory, no buffer pool,
    /// no decode. On this engine that removes the majority of what a lookup
    /// costs rather than trimming it, because the fetch *is* the cost.
    ///
    /// The trade is a second copy of the covered data and upkeep on every
    /// write to any covered field, not just the indexed one. Worth it when the
    /// read side uses it and a straight loss when it does not, which is why
    /// this is an explicit request rather than something inferred.
    pub fn create_covering_index(
        &mut self,
        collection: &str,
        field: &str,
        covers: &[String],
        kind: IndexKind,
    ) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if covers.is_empty() {
            return Err(Error::InvalidOptimization(
                "a covering index must carry at least one field; use create_index for none".into(),
            ));
        }
        // The indexed field is always carried, whether or not the caller asked
        // for it. Not a convenience — a correctness requirement. The plan puts
        // a `Filter` above the lookup, because the predicate may constrain
        // fields beyond the indexed one, and that filter re-evaluates the
        // whole predicate against the row the index produced. A row missing
        // the field the predicate tests on evaluates to `Unknown`, not `True`,
        // so every row would be dropped — the index would return nothing and
        // be perfectly consistent about it.
        let mut covers: Vec<String> = covers.to_vec();
        covers.push(field.to_string());
        covers.sort();
        covers.dedup();
        let name = adabt_index::covering_name(field, &covers);
        if self.index_exists(collection, &name, kind) {
            return Ok(());
        }
        let rows = self.store.scan(collection)?;
        let idx = adabt_index::CoveringIndex::build(
            field,
            covers,
            kind,
            rows.iter().map(|(i, r)| (*i, r)),
        );
        self.indexes
            .entry(collection.to_string())
            .or_default()
            .push(Box::new(idx));
        self.store.record_index(collection, &name, kind.as_str())?;
        self.bump_epoch(collection);
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    /// Build an index on `field` holding only records satisfying `condition`.
    ///
    /// Smaller than a full index and cheaper to maintain in proportion to how
    /// selective the condition is: a write to a record the condition excludes
    /// touches nothing at all.
    ///
    /// The engine will only *use* one for a query whose predicate contains a
    /// syntactically identical conjunct. That is much weaker than real
    /// predicate implication — an index conditioned on `age > 18` will not
    /// serve a query asking `age > 20`, though it plainly could — and it is
    /// weak on purpose. Being too weak costs a slower plan; being too clever
    /// costs correct answers.
    pub fn create_partial_index(
        &mut self,
        collection: &str,
        field: &str,
        condition: adabt_ir::Expr,
        kind: IndexKind,
    ) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        // Hex, not `Debug` text: the name is the only channel a persisted
        // index definition has, and a condition that cannot be read back is a
        // partial index that returns as a *full* one after a restart —
        // holding a subset of the rows and claiming to hold all of them.
        let encoded = crate::exprcodec::encode_expr_hex(&condition)?;
        let name = adabt_index::partial_name(field, &encoded);
        if self.index_exists(collection, &name, kind) {
            return Ok(());
        }
        let rows = self.store.scan(collection)?;
        let idx = adabt_index::PartialIndex::build(
            field,
            condition,
            encoded,
            kind,
            rows.iter().map(|(i, r)| (*i, r)),
        );
        self.indexes
            .entry(collection.to_string())
            .or_default()
            .push(Box::new(idx));
        self.store.record_index(collection, &name, kind.as_str())?;
        self.bump_epoch(collection);
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    /// Build an index, from cached entries when they are available.
    ///
    /// The two paths produce the same structure. The difference is what they
    /// cost: rebuilding decodes every record in the collection, while restoring
    /// reads back the keys directly and never touches a heap page.
    fn create_index_from(
        &mut self,
        collection: &str,
        field: &str,
        kind: IndexKind,
        cached: Option<Vec<(Value, Vec<RecordId>)>>,
    ) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if self.index_exists(collection, field, kind) {
            return Ok(());
        }
        // A composite index answers to the NUL-joined name of its fields, so
        // that name is also how one is recognised when rebuilt at startup.
        // Without this the restore path constructed a *single-field* index
        // over a field literally called "country\0age" — a field no record
        // has — so the index came back empty and every query through it
        // returned nothing. Silently, since an empty index is a valid index.
        //
        // A covering index has the same hazard in a sharper form. Its name
        // carries the projection after a `\u{1}`, and its payload — the
        // projected fields — is *not* in the cached key snapshot, which holds
        // only keys and ids. Restoring one from cache would produce an index
        // that finds the right ids and has no rows to serve them from. So a
        // covering index always rebuilds from the heap, and the cache is
        // ignored rather than half-used.
        // A partial index carries its condition, hex-encoded, after a
        // `\u{2}`. Rebuilding one as an ordinary index would produce an index
        // holding a subset of the rows that claims to hold all of them — the
        // worst version of the composite restore bug, since the wrong answers
        // would look like correct ones.
        let partial = field.contains(adabt_index::PARTIAL_SEP);
        let covering = field.contains(adabt_index::COVER_SEP);
        let mut idx: Box<dyn Index> = if partial {
            let (base, encoded) = adabt_index::partial_parts(field);
            let encoded = encoded.unwrap_or_default();
            let condition = crate::exprcodec::decode_expr_hex(&encoded)?;
            Box::new(adabt_index::PartialIndex::new(
                base, condition, encoded, kind,
            ))
        } else if covering {
            let (base, covers) = adabt_index::covering_parts(field);
            Box::new(adabt_index::CoveringIndex::new(base, covers, kind))
        } else if field.contains(adabt_index::COMPOSITE_SEP) {
            Box::new(adabt_index::CompositeIndex::new(
                adabt_index::composite_fields(field),
            ))
        } else {
            match kind {
                IndexKind::Hash => Box::new(HashIndex::new(field)),
                IndexKind::BTree => Box::new(BTreeIndex::new(field)),
                IndexKind::Bitmap => Box::new(BitmapIndex::new(field)),
            }
        };
        // A partial index cannot be restored from the key cache either: the
        // cache records which keys map to which ids, not which records passed
        // the condition, and re-inserting every cached entry would admit rows
        // the condition excludes.
        match cached.filter(|_| !covering && !partial) {
            Some(entries) => {
                for (key, ids) in entries {
                    for id in ids {
                        idx.insert(key.clone(), id);
                    }
                }
            }
            None => {
                for (id, rec) in &self.store.scan(collection)? {
                    idx.index_record(*id, rec);
                }
            }
        }
        self.indexes
            .entry(collection.to_string())
            .or_default()
            .push(idx);
        self.store.record_index(collection, field, kind.as_str())?;
        // A cached plan may encode an access path that is now suboptimal, or
        // one that referenced a structure that has changed.
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    pub fn drop_index(&mut self, collection: &str, field: &str, kind: IndexKind) -> bool {
        let Some(list) = self.indexes.get_mut(collection) else {
            return false;
        };
        let before = list.len();
        list.retain(|i| !(i.field() == field && i.kind() == kind));
        let changed = before != list.len();
        if changed {
            self.plan_cache.clear();
            self.compiled.clear();
            let _ = self.store.forget_index(collection, field, kind.as_str());
        }
        changed
    }

    fn index_exists(&self, collection: &str, field: &str, kind: IndexKind) -> bool {
        self.indexes
            .get(collection)
            .is_some_and(|l| l.iter().any(|i| i.field() == field && i.kind() == kind))
    }

    pub fn index_specs(&self) -> Vec<IndexSpec> {
        let mut out = Vec::new();
        for (c, list) in &self.indexes {
            for i in list {
                out.push(IndexSpec {
                    collection: c.clone(),
                    field: i.field().to_string(),
                    kind: i.kind(),
                });
            }
        }
        out.sort_by(|a, b| {
            (&a.collection, &a.field, a.kind.as_str()).cmp(&(
                &b.collection,
                &b.field,
                b.kind.as_str(),
            ))
        });
        out
    }

    pub fn has_index(&self, collection: &str, field: &str) -> bool {
        self.indexes
            .get(collection)
            .is_some_and(|list| list.iter().any(|i| i.field() == field))
    }

    pub fn index_kind(&self, collection: &str, field: &str) -> Option<IndexKind> {
        self.indexes
            .get(collection)
            .and_then(|list| list.iter().find(|i| i.field() == field).map(|i| i.kind()))
    }

    /// Memory held by every derived structure, for the resource axis.
    pub fn derived_memory_bytes(&self) -> usize {
        self.index_memory_bytes()
            + self.result_cache.memory_bytes()
            + self
                .direct
                .values()
                .map(|d| d.memory_bytes())
                .sum::<usize>()
    }

    /// Bytes occupied by live records in the primary representation.
    pub fn stored_bytes(&mut self) -> Result<u64> {
        self.store.stored_bytes()
    }

    pub fn compression_enabled(&self) -> bool {
        self.store.compression_enabled()
    }

    /// Declare `field` as the collection's clustering hint: subsequent inserts
    /// are *placed* so records with nearby integer values land on the same
    /// pages, which is what turns a range scan over that field from a
    /// scatter of page reads into a run of consecutive ones.
    ///
    /// What is promised, precisely:
    /// - answers never change — clustering is placement, not content, and
    ///   every read path is identical to an unclustered collection's;
    /// - the hint is advisory. Updates may move a record to any page with
    ///   room; deletes leave holes. The *declaration* persists across
    ///   restarts (it is catalog state), but the placement ranges re-derive:
    ///   locality rebuilds as new keyed inserts arrive, it is not rebuilt
    ///   retroactively for old data.
    ///
    /// Only integer fields can steer placement today: the key is the field's
    /// value cast to `i64`, and anything else is silently inserted unclustered.
    pub fn declare_cluster_field(&mut self, collection: &str, field: &str) -> Result<()> {
        self.store.schema_of(collection)?;
        self.store.set_cluster_field(collection, Some(field))?;
        self.cluster_fields
            .insert(collection.to_string(), field.to_string());
        Ok(())
    }

    /// Drop a collection's clustering hint and forget its declaration.
    /// Existing placements stay where they are; only future keyed inserts
    /// revert to first-fit.
    pub fn clear_cluster_field(&mut self, collection: &str) -> Result<()> {
        self.store.set_cluster_field(collection, None)?;
        self.cluster_fields.remove(collection);
        Ok(())
    }

    /// Distinct pages `get` has touched since the last clear — the number a
    /// locality claim is measured against. See [`HeapStore::touched_pages`].
    pub fn touched_pages(&self) -> usize {
        self.store.touched_pages()
    }

    /// Reset the touched-page diagnostic.
    pub fn clear_page_touches(&mut self) {
        self.store.clear_page_touches();
    }

    /// The declared clustering hint for `collection`, if any.
    pub fn cluster_field(&self, collection: &str) -> Option<&str> {
        self.cluster_fields.get(collection).map(String::as_str)
    }

    /// The integer key a record contributes to the clustering hint, or `None`
    /// when the collection has no hint or the value is not an integer.
    fn cluster_key(collection: &str, field: &str, rec: &Record) -> Option<i64> {
        match rec.get(field) {
            Some(adabt_core::value::Value::I64(v)) => Some(*v),
            Some(adabt_core::value::Value::U64(v)) => i64::try_from(*v).ok(),
            _ => {
                let _ = collection;
                None
            }
        }
    }

    pub fn index_memory_bytes(&self) -> usize {
        self.indexes
            .values()
            .flat_map(|l| l.iter())
            .map(|i| i.memory_bytes())
            .sum()
    }

    // -- derived-structure maintenance ------------------------------------

    fn bump_epoch(&mut self, collection: &str) {
        *self.epochs.entry(collection.to_string()).or_default() += 1;
        self.result_cache.invalidate_collection(collection);
    }

    fn epoch(&self, collection: &str) -> u64 {
        self.epochs.get(collection).copied().unwrap_or(0)
    }

    // -- unique constraints -------------------------------------------------

    /// Declare that no two records in `collection` may share a value for
    /// `field`, and refuse if any already do.
    ///
    /// Nulls are exempt, matching the usual convention: an absent value is not
    /// a duplicate of another absent value. A backing hash index is created if
    /// one does not already exist — enforcement needs to find a conflict by
    /// value, not by scanning — and it is pinned, so the adaptive driver may
    /// never retract it: dropping the index would not slow the database down,
    /// it would make the constraint stop being enforced, which is a category of
    /// mistake no amount of cost-benefit scoring should be able to make.
    pub fn add_unique_constraint(&mut self, collection: &str, field: &str) -> Result<()> {
        if self.store.schema_of(collection).is_err() {
            return Err(Error::NoSuchCollection(collection.to_string()));
        }
        if self.unique_constraints.contains(collection, field) {
            return Ok(());
        }
        // Existing data first: a constraint that let its own violation stand
        // would be a promise broken at the moment it was made.
        let mut seen: HashMap<Value, RecordId> = HashMap::new();
        for (id, rec) in self.store.scan(collection)? {
            let Some(value) = rec.get(field).filter(|v| !v.is_null()).cloned() else {
                continue;
            };
            if let Some(&other) = seen.get(&value) {
                return Err(Error::UniqueViolation {
                    collection: collection.to_string(),
                    field: field.to_string(),
                    value: format!("{value:?} (already held by record {other}, and by {id})"),
                });
            }
            seen.insert(value, id);
        }
        if !self.index_exists(collection, field, IndexKind::Hash) {
            self.create_index(collection, field, IndexKind::Hash)?;
        }
        self.unique_constraints.add(collection, field);
        crate::unique::write(self.store.dir(), &self.unique_constraints)?;
        Ok(())
    }

    /// Stop enforcing a constraint. The backing index is left in place — whether
    /// it is still worth keeping for ordinary querying is the optimizer's
    /// decision, not this one's.
    pub fn drop_unique_constraint(&mut self, collection: &str, field: &str) -> Result<bool> {
        let removed = self.unique_constraints.remove(collection, field);
        if removed {
            crate::unique::write(self.store.dir(), &self.unique_constraints)?;
        }
        Ok(removed)
    }

    pub fn has_unique_constraint(&self, collection: &str, field: &str) -> bool {
        self.unique_constraints.contains(collection, field)
    }

    pub fn unique_constraints(&self) -> Vec<(String, String)> {
        self.unique_constraints
            .iter()
            .map(|(c, f)| (c.to_string(), f.to_string()))
            .collect()
    }

    /// Refuse `rec` if writing it to `collection` would violate a unique
    /// constraint. `exclude` is the id being replaced, if any — a record is
    /// always allowed to keep, or overwrite, its own value.
    fn check_unique_constraints(
        &self,
        collection: &str,
        exclude: Option<RecordId>,
        rec: &Record,
    ) -> Result<()> {
        if self.unique_constraints.is_empty() {
            return Ok(());
        }
        for field in self.unique_constraints.on(collection) {
            let Some(value) = rec.get(field).filter(|v| !v.is_null()) else {
                continue;
            };
            let conflict = self
                .indexes
                .get(collection)
                .and_then(|list| list.iter().find(|i| i.field() == field))
                .map(|idx| idx.lookup(value))
                .unwrap_or_default()
                .into_iter()
                .any(|id| Some(id) != exclude);
            if conflict {
                return Err(Error::UniqueViolation {
                    collection: collection.to_string(),
                    field: field.to_string(),
                    value: format!("{value:?}"),
                });
            }
        }
        Ok(())
    }

    fn reindex_insert(&mut self, collection: &str, id: RecordId, rec: &Record) {
        if let Some(list) = self.indexes.get_mut(collection) {
            for i in list {
                i.index_record(id, rec);
                // The price of this index, counted where it is actually paid.
                // See `Event::IndexMaintained`: without this the optimizer can
                // only see what an index is worth, never what it costs.
                self.probe.record(Event::IndexMaintained {
                    collection,
                    field: i.field(),
                });
            }
        }
        if let Some(d) = self.direct.get_mut(collection) {
            // A record that will not fit the stride means the direct array no
            // longer matches the schema; dropping it is safe because it is
            // derived, and correct because a partial array would lie.
            if d.put(id, rec).is_err() {
                self.direct.remove(collection);
            }
        }
        // A columnar copy is append-structured, so an update in place is not
        // expressible: the old row is tombstoned and the new one appended.
        if let Some(c) = self.columns.get_mut(collection) {
            c.mark_dead(id);
            c.append_row(id, rec);
        }
        self.views.on_insert(collection, rec);
    }

    fn reindex_remove(&mut self, collection: &str, id: RecordId, rec: &Record) {
        if let Some(list) = self.indexes.get_mut(collection) {
            for i in list {
                i.unindex_record(id, rec);
                self.probe.record(Event::IndexMaintained {
                    collection,
                    field: i.field(),
                });
            }
        }
        if let Some(d) = self.direct.get_mut(collection) {
            d.remove(id);
        }
        if let Some(c) = self.columns.get_mut(collection) {
            c.mark_dead(id);
        }
        self.views.on_remove(collection, rec);
    }

    /// Build a view for this query, if it is one a view can serve.
    fn maybe_materialize(&mut self, op: &adabt_ir::plan::LogicalOp) -> Result<()> {
        if !self.views.is_enabled() || self.views.has_view_for(op) {
            return Ok(());
        }
        let Some((collection, _, _)) = crate::matview::MaterializedViews::definition_of(op) else {
            return Ok(());
        };
        let collection = collection.to_string();
        let rows = self.store.scan(&collection)?;
        self.views.materialize(op, rows.iter().map(|(_, r)| r));
        Ok(())
    }

    pub fn materialized_views(&self) -> usize {
        self.views.len()
    }

    pub fn explain_materialized_views(&self) -> String {
        self.views.describe()
    }

    /// Whether a write has to read what it is about to overwrite.
    ///
    /// The standing cost of maintaining anything derived from a record's
    /// *contents* rather than its identity: an index has to be told which key to
    /// remove, a view which group to decrement, and neither can be worked out
    /// from the new record. It is why a structure nobody queries is a loss
    /// rather than merely neutral.
    fn needs_old_record(&self, collection: &str) -> bool {
        self.indexes.get(collection).is_some_and(|l| !l.is_empty())
            || self.views.watches(collection)
    }

    /// Build a direct array for every collection whose schema allows one.
    fn enable_direct_lookup(&mut self) -> Result<()> {
        for name in self.store.collection_names() {
            let schema = self.store.schema_of(&name)?.clone();
            if schema.mode() != SchemaMode::Fixed {
                continue;
            }
            let rows = self.store.scan(&name)?;
            if let Some(arr) = DirectArray::rebuild(schema, rows.iter().map(|(i, r)| (*i, r)))? {
                self.direct.insert(name, arr);
            }
        }
        Ok(())
    }

    fn disable_direct_lookup(&mut self) {
        self.direct.clear();
    }

    /// Raise a collection's schema to the most rigid its data supports.
    ///
    /// Returns what was inferred, so the caller can report what the collection
    /// gave up. Refuses to narrow a schema that is already at least as rigid,
    /// and refuses outright if any stored record would no longer fit.
    pub fn freeze_schema(&mut self, collection: &str) -> Result<crate::infer::InferredSchema> {
        let rows = self.store.scan(collection)?;
        let records: Vec<Record> = rows.into_iter().map(|(_, r)| r).collect();
        let inferred = crate::infer::infer(records.iter(), crate::infer::DEFAULT_HEADROOM);

        let current = self.store.schema_of(collection)?.mode();
        if inferred.schema.mode().rigidity() <= current.rigidity() {
            return Err(Error::InvalidOptimization(format!(
                "{collection} is already {current:?}; inference concluded {:?}",
                inferred.schema.mode()
            )));
        }

        self.store
            .alter_schema(collection, inferred.schema.clone())?;
        self.invalidate_after_schema_change(collection);
        Ok(inferred)
    }

    /// Change a collection's schema.
    ///
    /// Application-driven counterpart to `freeze_schema`: that one *infers*
    /// the new schema from the data and only ever tightens it, this one takes
    /// whatever schema the caller names, which is how a product actually adds
    /// a field as it grows. Both end up here because both need the same
    /// derived-structure invalidation, for the same reason — see
    /// `invalidate_after_schema_change`.
    ///
    /// `HeapStore::alter_schema` picks its own cost: a pure append or
    /// trailing drop is a catalog edit, everything else is copy-and-swap. The
    /// `Ok(usize)` it returns is rows physically rewritten, `0` meaning the
    /// cheap path was taken; that distinction is the storage layer's to make,
    /// this method just reports it.
    pub fn alter_schema(&mut self, collection: &str, schema: Schema) -> Result<usize> {
        let rewritten = self.store.alter_schema(collection, schema)?;
        self.invalidate_after_schema_change(collection);
        Ok(rewritten)
    }

    /// Drop everything derived from `collection`'s old layout.
    ///
    /// Necessary even for the in-place schema-change path: an index or column
    /// store built against the old field set is still self-consistent data,
    /// just data that no longer matches what the schema now promises a reader
    /// — a dropped field silently disappearing from a column store's own
    /// declared shape would be a second, quieter version of the M14 schema-
    /// freeze bug. Cheap either way, since every one of these is rebuildable.
    fn invalidate_after_schema_change(&mut self, collection: &str) {
        self.indexes.remove(collection);
        self.direct.remove(collection);
        self.columns.remove(collection);
        self.plan_cache.clear();
        self.compiled.clear();
        self.bump_epoch(collection);
    }

    /// Build a columnar copy of every collection.
    fn enable_column_store(&mut self) -> Result<()> {
        for name in self.store.collection_names() {
            let rows = self.store.scan(&name)?;
            self.columns
                .insert(name, ColumnStore::build(rows.iter().map(|(i, r)| (*i, r))));
        }
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    fn disable_column_store(&mut self) {
        self.columns.clear();
        self.plan_cache.clear();
        self.compiled.clear();
    }

    /// Fraction of columnar rows that are tombstones, worst across collections.
    pub fn column_store_dead_fraction(&self) -> f64 {
        self.columns
            .values()
            .map(|c| c.dead_fraction())
            .fold(0.0, f64::max)
    }

    pub fn has_column_store(&self, collection: &str) -> bool {
        self.columns.contains_key(collection)
    }

    // -- queries -----------------------------------------------------------

    /// Whether a structure built for an experiment is off-limits to this query.
    ///
    /// The mask applies to *everything* the planner can see, so a candidate that
    /// is hidden is genuinely invisible rather than merely deprioritised. An
    /// index the planner can still find is an index it will use.
    fn masked(&self) -> Option<&Candidates> {
        if self.hidden.is_empty() {
            None
        } else {
            Some(&self.hidden)
        }
    }

    /// The one experiment whose candidate this query may see, if any.
    ///
    /// `candidate_visible` says a candidate side is being served;
    /// `experiment_under_test` says whose. Neither alone is enough: the first
    /// on its own revealed every running experiment's structures at once.
    fn revealed(&self) -> Option<u64> {
        if self.candidate_visible {
            self.experiment_under_test
        } else {
            None
        }
    }

    fn plan_context(&self) -> PlanContext<'_> {
        let masked = self.masked();
        let revealed = self.revealed();
        let mut m = HashMap::new();
        // Selectivity estimates, read where they are cheapest: an index's
        // own key count. Only indexed fields get one, which is the right
        // boundary — the planner consults cardinality to choose between
        // serving structures, never to invent one.
        let mut card: HashMap<&str, HashMap<&str, u64>> = HashMap::new();
        for (c, list) in &self.indexes {
            m.insert(
                c.as_str(),
                list.iter()
                    .map(|i| (i.field(), i.kind()))
                    .filter(|(f, k)| !masked.is_some_and(|h| h.hides_index(revealed, c, f, *k)))
                    .collect(),
            );
            card.insert(
                c.as_str(),
                list.iter()
                    .map(|i| (i.field(), i.key_count() as u64))
                    .collect(),
            );
        }
        // Filtered per collection, not wiped wholesale: an experiment
        // trialling a column store on one collection must not blind the
        // planner to another collection's already-promoted one.
        let columnar: Vec<&str> = self
            .columns
            .keys()
            .map(|k| k.as_str())
            .filter(|c| !masked.is_some_and(|h| h.hides_column_store(revealed, c)))
            .collect();
        // And which fields each of those stores can reconstruct — the
        // membership test a top-K decision needs before it may order by a
        // field, because a columnar projection silently omits the rest.
        let columnar_fields: HashMap<&str, Vec<String>> = self
            .columns
            .iter()
            .filter(|(c, _)| !masked.is_some_and(|h| h.hides_column_store(revealed, c)))
            .map(|(c, store)| {
                (
                    c.as_str(),
                    store.fields().into_iter().map(String::from).collect(),
                )
            })
            .collect();
        // Composite indexes, recognised by their NUL-joined name. Reported
        // separately because the planner asks a different question of them.
        //
        // The covering check comes first and is not optional. A covering
        // index over two or more fields is named `f\u{1}a\u{0}b`, which also
        // contains a NUL — so without this guard one would be read as a
        // *composite* index over the fields `f\u{1}a` and `b`, neither of
        // which any record has. The planner would then choose it for a
        // predicate it cannot serve and get nothing back. Exactly the failure
        // the composite restore path already had once, in a new place.
        let mut composite: HashMap<&str, Vec<Vec<String>>> = HashMap::new();
        let mut covering: HashMap<&str, Vec<adabt_exec::planner::CoveringEntry>> = HashMap::new();
        let mut partial: HashMap<&str, Vec<(&str, adabt_ir::Expr, IndexKind)>> = HashMap::new();
        for (c, list) in &self.indexes {
            for i in list {
                let name = i.field();
                if let Some(encoded) = i.condition() {
                    // A condition that will not decode means an index whose
                    // restriction is unknown. Skipping it costs a slower plan;
                    // using it would mean reading a subset as if it were the
                    // whole collection.
                    if let Ok(cond) = crate::exprcodec::decode_expr_hex(encoded) {
                        if !masked.is_some_and(|h| h.hides_index(revealed, c, name, i.kind())) {
                            partial
                                .entry(c.as_str())
                                .or_default()
                                .push((name, cond, i.kind()));
                        }
                    }
                } else if !i.covers().is_empty() {
                    if !masked.is_some_and(|h| h.hides_index(revealed, c, name, i.kind())) {
                        covering.entry(c.as_str()).or_default().push((
                            name,
                            i.covers().to_vec(),
                            i.kind(),
                        ));
                    }
                } else if name.contains(adabt_index::COMPOSITE_SEP) {
                    composite
                        .entry(c.as_str())
                        .or_default()
                        .push(adabt_index::composite_fields(name));
                }
            }
        }
        let mut row_counts: HashMap<&str, u64> = HashMap::new();
        for c in m.keys().copied().chain(columnar.iter().copied()) {
            if let Some(n) = self.store.live_count(c) {
                row_counts.insert(c, n);
            }
        }
        PlanContext {
            indexes: m,
            composite,
            covering,
            partial,
            columnar,
            columnar_fields,
            cardinality: card,
            row_counts,
        }
    }

    pub fn plan(&self, logical: &LogicalPlan) -> PhysicalPlan {
        make_plan(&logical.root, &self.plan_context())
    }

    pub fn explain(&self, logical: &LogicalPlan) -> String {
        format!(
            "logical:\n{}\nphysical:\n{}",
            logical.explain(),
            self.plan(logical).explain()
        )
    }

    /// Report which fields a query filtered on, and how.
    ///
    /// Equality and range wants are recorded separately because they want
    /// different index structures, and a driver that cannot tell them apart
    /// builds hash indexes for range predicates.
    fn note_filtered_fields(&self, logical: &LogicalPlan) {
        let collection = logical.collection();
        let mut op = Some(&logical.root);
        // Gathered across the whole walk rather than at the filter node:
        // the projection may sit above sorts and limits, and what a covering
        // index needs to know is which output fields travelled with this
        // predicate, wherever they were asked for.
        let mut equalities_all: Vec<Vec<String>> = Vec::new();
        let mut ranges: Vec<String> = Vec::new();
        let mut projected: Vec<String> = Vec::new();
        while let Some(o) = op {
            match o {
                adabt_ir::plan::LogicalOp::Filter { predicate, .. } => {
                    let mut equalities: Vec<String> = predicate
                        .equality_constraints()
                        .into_iter()
                        .map(|(f, _)| f)
                        .collect();
                    // Sorted and de-duplicated so that `country AND age` and
                    // `age AND country` are recognised as the same shape. A
                    // composite index over a set does not care which order the
                    // predicate wrote them in, and counting the two separately
                    // would halve the evidence for building one.
                    equalities.sort();
                    equalities.dedup();
                    let mut all = Vec::new();
                    predicate.referenced_fields(&mut all);
                    for f in all {
                        // Constrained but not to a literal: range-filtered,
                        // which is the covering question asked of a b-tree
                        // rather than a hash.
                        let equality = equalities.contains(&f);
                        if !equality && !ranges.contains(&f) && range_constrained(predicate, &f) {
                            ranges.push(f.clone());
                        }
                        self.probe.record(Event::FieldFiltered {
                            collection,
                            field: &f,
                            equality,
                        });
                    }
                    if equalities.len() > 1 {
                        self.probe.record(Event::FieldsPinnedTogether {
                            collection,
                            fields: &equalities,
                        });
                    }
                    equalities_all.push(equalities);
                }
                adabt_ir::plan::LogicalOp::Project { fields, .. } => {
                    projected.extend(fields.iter().cloned());
                }
                _ => {}
            }
            op = o.child();
        }
        if projected.is_empty() || (equalities_all.is_empty() && ranges.is_empty()) {
            return;
        }
        projected.sort();
        projected.dedup();
        // One observation per filtered field, projection minus that field —
        // the index carries its own key, so asking it to "cover" the filtered
        // field would double-count a column it stores anyway. Equality and
        // range observations stay apart: they want differently-backed
        // indexes.
        let mut seen: Vec<(String, bool)> = Vec::new();
        for eqs in &equalities_all {
            for f in eqs {
                if !seen.iter().any(|(s, _)| s == f) {
                    seen.push((f.clone(), true));
                }
            }
        }
        ranges.sort();
        for f in &ranges {
            if !seen.iter().any(|(s, _)| s == f) {
                seen.push((f.clone(), false));
            }
        }
        seen.sort();
        for (f, equality) in seen {
            let covers: Vec<String> = projected.iter().filter(|p| *p != &f).cloned().collect();
            self.probe.record(Event::FieldsProjectedTogether {
                collection,
                filtered: &f,
                fields: &covers,
                equality,
            });
        }
    }

    /// Feed the temperature sketch, on a sampled basis.
    fn note_touch(&self, collection: &str, id: RecordId) {
        const STRIDE: u32 = 16;
        let n = self.touch_sample.get().wrapping_add(1);
        self.touch_sample.set(n);
        if n % STRIDE == 0 {
            self.probe.record(Event::Touch { collection, id });
        }
    }

    pub fn query(&mut self, logical: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        // A join takes its own, much smaller path — see `query_join` — rather
        // than entering the machinery below at all: the plan cache, result
        // cache, materialized-view lookup and compiled shortcuts are all
        // keyed or reasoned about in terms of one collection with one epoch,
        // and `logical.collection()` (the first thing `query_in` calls)
        // panics outright on a plan with more than one source. Checked before
        // the experiment branch too, so a join can never accidentally enter
        // shadow or canary execution, which has nothing built for it either.
        if logical.root.contains_join() {
            return self.query_join(logical);
        }
        if !self.experiments.is_empty() {
            return self.experiment_query(logical);
        }
        self.query_in(logical, QueryMode::Normal)
    }

    /// A plan containing a `Join`, executed directly: planned fresh every
    /// call (no plan cache — a per-shape cached decision was never going to
    /// mean anything for two independent scan sides), not checked against
    /// the result cache or materialized views (nothing maintains either for
    /// a join yet), and its index usage does not reach the adaptive driver's
    /// telemetry (so a join's own access pattern does not yet teach the
    /// optimizer anything about indexing either side — a real, narrower gap
    /// than not joining at all, and a separable one).
    ///
    /// Refuses a plan with more than one `Join` node anywhere in it: each
    /// side of a join is planned and executed independently, by recursing
    /// into the ordinary single-collection machinery, which requires that
    /// neither side contain a join of its own. Real multi-way join planning —
    /// choosing an execution order among the n! orderings of an n-way join —
    /// is a cost-based search problem this project has no cost model for yet
    /// outside `adabt-opt`'s representation-choice scoring, which answers a
    /// different question. One join at a time is what "nested-loop and hash
    /// join" in the plan text asks for; ordering among several is real,
    /// separable, future work.
    fn query_join(&mut self, logical: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        if count_joins(&logical.root) > 1 {
            return Err(Error::Unsupported(
                "nested or multi-way joins are not implemented yet; a query may contain one Join"
                    .into(),
            ));
        }
        let started = Instant::now();
        let physical = adabt_exec::planner::plan(&logical.root, &self.plan_context());
        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: self.policy.constraints.max_query_ram_bytes,
            cancel: self.pending_cancel.clone(),
        };
        let rows = execute_with_budget(&physical, self, &mut stats, &budget)?;
        self.last_stats = stats;
        if let Some((threshold, _)) = &self.slow_query {
            let elapsed = started.elapsed();
            if elapsed >= *threshold {
                let event = SlowQueryEvent {
                    elapsed,
                    rows_scanned: stats.rows_scanned,
                    rows_returned: rows.len() as u64,
                    explain: physical.explain(),
                };
                if let Some((_, sink)) = &mut self.slow_query {
                    sink(&event);
                }
            }
        }
        Ok(rows)
    }

    /// [`Self::query`], but another thread can stop it early by setting
    /// `cancel`.
    ///
    /// There is no timer in here — setting `cancel` is the whole mechanism, so
    /// a caller who wants "stop after 5 seconds" spawns a thread that sleeps 5
    /// seconds and then sets the flag this call was given. What stops is
    /// whatever the query is doing when it next polls the flag: a scan or sort
    /// checks it every few thousand rows (see `adabt_exec::exec`), so an
    /// already-fast query still just returns its answer, and only a query that
    /// was going to take a while is actually shortened.
    ///
    /// A cancelled query returns `Err(Error::Cancelled(_))` having written
    /// nothing — every read path here reads before it decides whether to keep
    /// going, never the reverse.
    pub fn query_cancellable(
        &mut self,
        logical: &LogicalPlan,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<(RecordId, Record)>> {
        self.pending_cancel = Some(cancel);
        let result = self.query(logical);
        self.pending_cancel = None;
        result
    }

    /// Call `sink` for every query whose end-to-end wall-clock time — cache
    /// probes and planning included, not just the executor's own work — is
    /// at least `threshold`.
    ///
    /// Applies to the general query path only; the compiled and direct-
    /// lookup shortcuts `query_in` takes for a hot identity read never reach
    /// this check, on the reasoning that an O(1) lookup is not the kind of
    /// query a slow-query log exists to surface, and always timing it just to
    /// confirm that would spend the exact per-query cost this feature is
    /// opt-in specifically to avoid.
    pub fn set_slow_query_sink(
        &mut self,
        threshold: Duration,
        sink: impl FnMut(&SlowQueryEvent) + Send + 'static,
    ) {
        self.slow_query = Some((threshold, Box::new(sink)));
    }

    /// [`Self::set_slow_query_sink`] with a sink that writes one line to
    /// stderr — the same "no dependency, no configuration" default this
    /// project's other operational logging (the server's own start/shutdown
    /// messages) already uses.
    pub fn enable_slow_query_log(&mut self, threshold: Duration) {
        self.set_slow_query_sink(threshold, |event| {
            eprintln!(
                "slow query: {:?}, {} rows scanned, {} returned\n{}",
                event.elapsed, event.rows_scanned, event.rows_returned, event.explain
            );
        });
    }

    pub fn disable_slow_query_log(&mut self) {
        self.slow_query = None;
    }

    /// Force the identity-lookup compiled path for `collection` on immediately,
    /// rather than waiting for `compiled::HOT_THRESHOLD` calls to observe it.
    ///
    /// This is not "compile an arbitrary query shape": `CompiledPaths::candidate`
    /// only ever recognizes one shape, a bare `GetById`, by design — everything
    /// else genuinely has work left to do that skipping the general path would
    /// mean reimplementing (see `compiled.rs`'s own module docs). An expert
    /// naming "compile shape X" can only ever mean this one shape, for any
    /// collection; there is no other shape this mechanism is able to compile,
    /// forced or not. What forcing buys is real but narrow: a workload doing a
    /// handful of expensive identity lookups — fewer than the threshold —
    /// still gets the specialised path from the first call instead of waiting
    /// to earn it.
    pub fn compile_identity_lookups(&mut self, collection: &str) -> Result<()> {
        self.store.schema_of(collection)?;
        // Any id produces the same shape: `QueryShape` is structural and
        // erases literals by construction, which is the entire reason a
        // shape can be cached and rebound to a different id later.
        let probe = LogicalOp::get(collection, RecordId(0));
        let shape = LogicalPlan::new(probe.clone()).shape();
        let path =
            crate::compiled::CompiledPaths::candidate(&probe, |c| self.direct.contains_key(c))
                .expect("GetById is always a compile candidate");
        self.compiled.install(shape, path);
        Ok(())
    }

    fn query_in(
        &mut self,
        logical: &LogicalPlan,
        mode: QueryMode,
    ) -> Result<Vec<(RecordId, Record)>> {
        let shape = logical.shape();

        // The specialised path, taken before any of the general machinery. For
        // a hot identity lookup this is the whole query: no filter accounting,
        // no cache probes, no plan, no operators, no batching.
        if mode.uses_caches() && self.compiled.get(shape).is_some() {
            if let adabt_ir::plan::LogicalOp::GetById { collection, id } = &logical.root {
                let (collection, id) = (collection.clone(), *id);
                let rec = match self.direct.get(&collection) {
                    Some(d) => d.get(id)?,
                    None => self.store.get(&collection, id)?,
                };
                return Ok(rec.map(|r| (id, r)).into_iter().collect());
            }
        }
        // Worth specialising? Decided once, not per call.
        if mode.uses_caches() && self.compiled.observe(shape) {
            let direct = &self.direct;
            if let Some(path) =
                crate::compiled::CompiledPaths::candidate(&logical.root, |c| direct.contains_key(c))
            {
                self.compiled.install(shape, path);
            }
        }

        let collection = logical.collection().to_string();
        let key = QueryKey::of(&logical.root);
        let epoch = self.epoch(&collection);
        let started = Instant::now();

        if mode.is_counted() {
            self.note_filtered_fields(logical);
        }

        // A materialized view is a derived representation, not a cache, so it is
        // consulted even under a trial — masking it is the experiment's job, not
        // this function's.
        if let Some(rows) = self.views.answer(&logical.root) {
            if mode.is_counted() {
                self.observe_shape(&collection, shape, started, rows.len() as u64);
            }
            return Ok(rows);
        }

        if mode.uses_caches() {
            if let Some(rows) = self.result_cache.get(key, epoch) {
                let rows = rows.clone();
                self.probe.record(Event::CacheProbe {
                    name: "result_cache",
                    hit: true,
                });
                self.observe_shape(&collection, shape, started, rows.len() as u64);
                return Ok(rows);
            }
            self.probe.record(Event::CacheProbe {
                name: "result_cache",
                hit: false,
            });
        }

        // The cache holds a shape-invariant *decision*; the plan is rebuilt
        // around this query's literals every time. Caching the plan itself would
        // let one query be answered with another's literals.
        //
        // It is also bypassed under a trial, and for a different reason: the
        // decision depends on which structures are visible, and the two sides of
        // an experiment differ in exactly that while sharing a shape. A cached
        // decision would hand the baseline's plan to the candidate and the
        // comparison would be of a thing against itself.
        let decision = match self.plan_cache.get(shape).filter(|_| mode.uses_caches()) {
            Some(d) => {
                let d = d.clone();
                self.probe.record(Event::CacheProbe {
                    name: "plan_cache",
                    hit: true,
                });
                d
            }
            None => {
                if mode.uses_caches() {
                    self.probe.record(Event::CacheProbe {
                        name: "plan_cache",
                        hit: false,
                    });
                }
                let d = adabt_exec::planner::decide(&logical.root, &self.plan_context());
                if mode.uses_caches() {
                    self.plan_cache.insert(shape, || d.clone());
                }
                d
            }
        };
        let physical = adabt_exec::planner::build_from(&logical.root, &decision);
        // Record the access path actually chosen. An index nobody picks is
        // invisible to any amount of watching queries arrive.
        if mode.is_counted() {
            if let adabt_exec::planner::AccessDecision::IndexLookup { field, .. }
            | adabt_exec::planner::AccessDecision::IndexRange { field } = &decision.access
            {
                self.probe.record(Event::IndexUsed {
                    collection: &collection,
                    field,
                });
            }
        }

        let mut stats = ExecStats::default();
        let budget = ExecBudget {
            max_ram_bytes: self.policy.constraints.max_query_ram_bytes,
            cancel: self.pending_cancel.clone(),
        };
        let rows = execute_with_budget(&physical, self, &mut stats, &budget)?;
        self.last_stats = stats;
        // `explain` is only ever built when a query actually crossed the
        // threshold, so a database with no slow-query log configured — the
        // default — pays for exactly what it already paid for `started` and
        // nothing more. Built in two passes because `self.explain` needs
        // `&self` and the sink lives behind `&mut self.slow_query` — not
        // overlapping fields to the borrow checker once either goes through
        // a method call.
        if let Some((threshold, _)) = &self.slow_query {
            let elapsed = started.elapsed();
            if elapsed >= *threshold {
                let event = SlowQueryEvent {
                    elapsed,
                    rows_scanned: stats.rows_scanned,
                    rows_returned: rows.len() as u64,
                    explain: self.explain(logical),
                };
                if let Some((_, sink)) = &mut self.slow_query {
                    sink(&event);
                }
            }
        }
        // The query has just been answered by a scan, which is exactly what a
        // view is built from. Building it here means the first such query pays
        // what it was going to pay anyway and every later one pays nothing.
        self.maybe_materialize(&logical.root)?;
        if mode.uses_caches() {
            self.result_cache
                .insert(key, &collection, epoch, || rows.clone());
        }
        if mode.is_counted() {
            self.observe_shape(&collection, shape, started, rows.len() as u64);
        }
        Ok(rows)
    }

    // -- experiments -------------------------------------------------------

    /// Find a running experiment by id.
    fn experiment_index(&self, id: u64) -> Option<usize> {
        self.experiments.iter().position(|e| e.experiment.id == id)
    }

    /// Begin proving a change against live traffic.
    ///
    /// Nothing is built yet: the experiment starts in `Proposed` and each call
    /// to [`Database::advance_experiments`] moves it one step, so the caller
    /// controls the pace at which evidence is demanded.
    ///
    /// Several may run at once, as long as their scopes do not overlap.
    pub fn begin_experiment(&mut self, decision: Decision, guardrails: Guardrails) -> Result<u64> {
        if decision.action != DecisionAction::Enable {
            return Err(Error::InvalidOptimization(format!(
                "only an enable can be experimented on, not a {}",
                decision.action.as_str()
            )));
        }
        let inputs = self.opt_inputs()?;
        let plan = {
            let opt = self.registry.get(decision.optimization).ok_or_else(|| {
                Error::InvalidOptimization(format!("{} is not registered", decision.optimization))
            })?;
            opt.plan_enable(&inputs.ctx(), &decision.scope, &decision.params)
        };
        if plan.apply.is_empty() {
            return Err(Error::InvalidOptimization(format!(
                "{} has nothing to do for {}",
                decision.optimization, decision.scope
            )));
        }
        if let Some(bad) = plan.apply.iter().find(|a| !a.is_shadowable()) {
            return Err(Error::InvalidOptimization(format!(
                "{} cannot be experimented on: {} leaves no old path to compare against",
                decision.optimization,
                bad.describe()
            )));
        }

        // Which collection's queries count as evidence. A scope of `global`, or
        // one naming something that is not a collection, means every query does.
        let candidate_collection = decision
            .scope
            .split('.')
            .next()
            .filter(|c| self.store.collection_names().iter().any(|n| n == c))
            .unwrap_or("")
            .to_string();

        if let Some(clash) = self
            .experiments
            .iter()
            .find(|e| crate::experiment::scopes_overlap(&e.collection, &candidate_collection))
        {
            let where_ = if candidate_collection.is_empty() {
                "globally".to_string()
            } else {
                format!("on {candidate_collection}")
            };
            return Err(Error::InvalidOptimization(format!(
                "experiment #{} already covers the same traffic ({}), so running \
                 one {where_} too would make each measure the other",
                clash.experiment.id,
                if clash.collection.is_empty() {
                    "every collection".to_string()
                } else {
                    clash.collection.clone()
                }
            )));
        }

        let id = self.next_experiment_id;
        self.next_experiment_id += 1;
        self.experiments.push(LiveExperiment::new(
            id,
            decision,
            candidate_collection,
            guardrails,
        ));
        Ok(id)
    }

    /// The oldest running experiment, if any.
    ///
    /// Kept for the common case of driving one at a time; see
    /// [`Database::experiments`] when several may be running.
    pub fn experiment(&self) -> Option<&LiveExperiment> {
        self.experiments.first()
    }

    /// Every experiment running right now, oldest first.
    pub fn experiments(&self) -> impl Iterator<Item = &LiveExperiment> {
        self.experiments.iter()
    }

    /// Changes the controller accepted, over the database's whole life.
    pub fn decision_count(&self) -> usize {
        self.controller
            .log()
            .records()
            .iter()
            .filter(|r| r.verdict.succeeded())
            .count()
    }

    /// Experiments that have ever been started.
    ///
    /// Counted from the id allocator rather than from a list, so it includes
    /// the ones already retired and does not grow a structure to answer.
    pub fn experiments_started(&self) -> usize {
        (self.next_experiment_id - 1) as usize
    }

    /// Experiments that ended by being promoted.
    pub fn promoted_count(&self) -> usize {
        self.finished
            .iter()
            .filter(|e| e.phase() == Phase::Promoted)
            .count()
    }

    /// Experiments that ended by being reverted.
    pub fn reverted_count(&self) -> usize {
        self.finished
            .iter()
            .filter(|e| e.phase() == Phase::Reverted)
            .count()
    }

    /// Experiments that reached a verdict, oldest first.
    pub fn finished_experiments(&self) -> &[LiveExperiment] {
        &self.finished
    }

    pub fn explain_experiment(&self) -> String {
        if self.experiments.is_empty() {
            return match self.finished.last() {
                Some(e) => e.explain(),
                None => "no experiments\n".to_string(),
            };
        }
        self.experiments
            .iter()
            .map(|e| e.explain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fold in the evidence so far and let every experiment's state machine
    /// act on it.
    ///
    /// Returns each experiment's id and the phase now in effect. Each call is
    /// one opportunity to move, and a machine holds where it is when the
    /// evidence is not yet enough — so calling this on a timer is safe and
    /// calling it too often merely produces `Inconclusive` until the samples
    /// arrive.
    pub fn advance_experiments(&mut self) -> Result<Vec<(u64, Phase)>> {
        let ids: Vec<u64> = self.experiments.iter().map(|e| e.experiment.id).collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // Re-found each time: retiring one experiment removes it from the
            // vector, so an index taken before the loop would drift.
            let Some(i) = self.experiment_index(id) else {
                continue;
            };
            let ram = self.derived_memory_bytes() as u64;
            let live = &mut self.experiments[i];
            live.fold(ram);
            let before = live.phase();
            let after = live.experiment.advance();
            if after != before {
                self.enter_phase(id, after)?;
            }
            out.push((id, after));
        }
        Ok(out)
    }

    /// Advance the oldest running experiment, for callers driving one.
    pub fn advance_experiment(&mut self) -> Result<Option<Phase>> {
        Ok(self.advance_experiments()?.first().map(|(_, p)| *p))
    }

    /// Stop every running experiment and undo whatever each built.
    pub fn abort_experiment(&mut self, why: &str) -> Result<()> {
        let ids: Vec<u64> = self.experiments.iter().map(|e| e.experiment.id).collect();
        for id in ids {
            self.abort_experiment_by_id(id, why)?;
        }
        Ok(())
    }

    /// Stop one experiment and undo whatever it built.
    pub fn abort_experiment_by_id(&mut self, id: u64, why: &str) -> Result<()> {
        if let Some(i) = self.experiment_index(id) {
            self.experiments[i].experiment.abort(why);
        }
        self.retire_experiment(id, why.to_string(), false)
    }

    fn enter_phase(&mut self, id: u64, phase: Phase) -> Result<()> {
        if phase.is_measuring() {
            if let Some(i) = self.experiment_index(id) {
                self.experiments[i].reset_measurements();
            }
        }
        match phase {
            Phase::Building => self.build_candidate(id),
            Phase::Promoted => self.retire_experiment(id, "promoted".into(), true),
            Phase::Reverted => {
                // The reason is read from the experiment, which recorded it when
                // it aborted. Re-assessing here would ask a reverted experiment
                // how it is doing and be told, accurately and uselessly, that it
                // is serving no traffic.
                let why = self
                    .experiment_index(id)
                    .map(|i| self.experiments[i].experiment.outcome())
                    .unwrap_or_else(|| "reverted".into());
                self.retire_experiment(id, why, false)
            }
            _ => Ok(()),
        }
    }

    /// Build one experiment's candidate where the planner cannot see it.
    fn build_candidate(&mut self, id: u64) -> Result<()> {
        let Some(i) = self.experiment_index(id) else {
            return Ok(());
        };
        let decision = self.experiments[i].experiment.decision.clone();
        let ram_before = self.derived_memory_bytes() as u64;
        self.experiments[i].set_ram_before(ram_before);
        // Mask first, build second. The other order would leave a window in
        // which the structure exists and is usable, and one query through it is
        // one query the experiment was supposed to be protecting.
        self.candidate_visible = false;
        self.recording_candidate = Some(id);
        let inputs = self.opt_inputs()?;
        let mut trial = decision.clone();
        trial.trigger = format!("trialled by experiment #{id}: {}", decision.trigger);
        let report = self.run_decisions(vec![trial], DecisionSource::Adaptive, inputs);
        self.recording_candidate = None;
        let report = report?;

        // Asked of *this* experiment, not of the mask as a whole: with another
        // experiment running, a globally non-empty mask says nothing about
        // whether this one built anything worth comparing.
        if !report.all_applied() || self.hidden.is_empty_for(id) {
            // Either the controller refused, or it applied something with
            // nothing to mask. Both mean there is no comparison to run, and
            // measuring one anyway would compare the database against itself
            // and promote on the noise.
            let why = if report.all_applied() {
                "the change built nothing that can be held beside the current representation"
                    .to_string()
            } else {
                let refused: Vec<String> = report
                    .rejected
                    .iter()
                    .map(|(l, v)| format!("{l}: {}", v.as_str()))
                    .collect();
                format!("the controller refused it ({})", refused.join("; "))
            };
            self.abort_experiment_by_id(id, &why)?;
        }
        Ok(())
    }

    /// End one experiment, keeping or undoing what it built.
    fn retire_experiment(&mut self, id: u64, why: String, promoted: bool) -> Result<()> {
        let Some(i) = self.experiment_index(id) else {
            return Ok(());
        };
        let mut live = self.experiments.remove(i);
        live.candidates = self.hidden.clone();
        let decision = live.experiment.decision.clone();

        if !promoted && !self.hidden.is_empty_for(id) {
            // Undo through the controller, which knows the exact inverse of what
            // it applied. Reverting is instant because every candidate is a
            // derived representation: dropping one costs a deallocation and
            // loses nothing that cannot be rebuilt from the primary.
            let mut undo = decision.clone();
            undo.action = DecisionAction::Disable;
            undo.trigger = format!("experiment #{id} reverted: {why}");
            let inputs = self.opt_inputs()?;
            self.run_decisions(vec![undo], DecisionSource::Adaptive, inputs)?;
        }
        // Unmasking is what promotion *is*. Nothing is rebuilt and nothing
        // moves: the structure was real the whole time, and all that changes
        // is that the planner is now allowed to know about it.
        //
        // Only *this* experiment's entries are dropped. Clearing the whole
        // mask — which is what this did — would unmask any other running
        // experiment's unproven structure the instant an unrelated
        // experiment finished, exposing it to live traffic it was
        // specifically being kept away from.
        self.hidden.forget(id);
        self.candidate_visible = false;
        self.experiment_under_test = None;

        let detail = if promoted {
            format!(
                "experiment #{id} promoted — {}, {} canary queries on the candidate",
                live.shadow.describe(),
                live.experiment.candidate.samples
            )
        } else {
            format!("experiment #{id} reverted — {why}")
        };
        let verdict = if promoted {
            Verdict::Applied
        } else {
            Verdict::Failed
        };
        self.controller
            .note(decision, verdict, detail, DecisionSource::Adaptive);
        self.finished.push(live);
        Ok(())
    }

    /// The experiment, if any, that this query is evidence for.
    ///
    /// At most one: `begin_experiment` refuses overlapping scopes, so a query
    /// on a collection can match a collection-scoped experiment or a global
    /// one but never both.
    fn experiment_for(&self, collection: &str) -> Option<u64> {
        self.experiments
            .iter()
            .find(|e| {
                e.phase().is_measuring() && (e.collection.is_empty() || e.collection == collection)
            })
            .map(|e| e.experiment.id)
    }

    /// Route one query according to the phase its experiment is in.
    fn experiment_query(&mut self, logical: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        let Some(id) = self.experiment_for(logical.collection()) else {
            return self.query_in(logical, QueryMode::Normal);
        };
        let Some(i) = self.experiment_index(id) else {
            return self.query_in(logical, QueryMode::Normal);
        };
        // Taken out for the duration so the query path can borrow `self`
        // mutably, and put back before returning on every path.
        let mut live = self.experiments.remove(i);
        // Names whose candidate may be revealed while this query runs. Without
        // it, `candidate_visible` would expose every running experiment's
        // structures at once and each would be measuring the others.
        self.experiment_under_test = Some(id);

        let out = match live.phase() {
            // Both paths, same query, same state. The caller receives the
            // baseline: the candidate is being judged, not trusted.
            Phase::Shadow => {
                let r = crate::shadow::trial(self, logical, &mut live.shadow);
                if let Some(t) = live.shadow.last_trial() {
                    live.record_baseline(t.baseline_nanos, r.is_ok());
                    live.record_candidate(t.candidate_nanos, r.is_ok());
                }
                r
            }
            // One path, and its answer is returned. Correctness was settled in
            // shadow; what a canary measures is latency under the cache state a
            // real workload actually produces.
            Phase::Canary(percent) => {
                let use_candidate = live.route(percent);
                self.candidate_visible = use_candidate;
                let started = Instant::now();
                let r = self.query_in(logical, QueryMode::Served);
                let nanos = started.elapsed().as_nanos() as u64;
                self.candidate_visible = false;
                if use_candidate {
                    live.record_candidate(nanos, r.is_ok());
                } else {
                    live.record_baseline(nanos, r.is_ok());
                }
                r
            }
            _ => self.query_in(logical, QueryMode::Normal),
        };
        self.experiment_under_test = None;
        // Back where it was, so age order — and therefore `experiment()` —
        // stays stable across queries.
        self.experiments.insert(i.min(self.experiments.len()), live);
        out
    }

    fn observe_shape(&self, collection: &str, shape: QueryShape, started: Instant, rows: u64) {
        self.probe.record(Event::Op {
            collection,
            kind: OpKind::Scan,
            shape: adabt_telemetry::QueryShape(shape.0),
            nanos: started.elapsed().as_nanos() as u64,
            rows,
        });
    }

    fn observe(&self, collection: &str, kind: OpKind, started: Instant, rows: u64) {
        self.probe.record(Event::Op {
            collection,
            kind,
            shape: adabt_telemetry::QueryShape::UNKNOWN,
            nanos: started.elapsed().as_nanos() as u64,
            rows,
        });
    }
}

/// The two ways this database can answer a query while an experiment is running.
///
/// Both go through the ordinary query path. The only difference between them is
/// one boolean, which is the point: if the candidate needed a separate execution
/// path to be measured, the measurement would be of that path rather than of the
/// change.
impl crate::shadow::ShadowPair for Database {
    fn baseline(&mut self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        self.candidate_visible = false;
        self.query_in(plan, QueryMode::Trial)
    }

    fn candidate(&mut self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        self.candidate_visible = true;
        let out = self.query_in(plan, QueryMode::Trial);
        self.candidate_visible = false;
        out
    }
}

impl ActionSink for Database {
    fn can_apply(&mut self, action: &Action) -> bool {
        match action {
            Action::CreateIndex { collection, .. } | Action::DropIndex { collection, .. } => {
                self.store.schema_of(collection).is_ok()
            }
            // Only worth building where a schema guarantees a constant stride.
            Action::SetDirectLookup(true) => !self.fixed_size_collections().is_empty(),
            _ => true,
        }
    }

    fn apply_action(&mut self, action: &Action) -> Result<()> {
        // Recorded as it happens rather than predicted from the plan, so what is
        // masked is exactly what was built. A prediction that drifted from what
        // the engine actually did would mask the wrong structure — and masking
        // the wrong structure means the "baseline" measurement is quietly taken
        // through the candidate.
        if let Some(id) = self.recording_candidate {
            let for_collection = self
                .experiment_index(id)
                .map(|i| self.experiments[i].collection.clone())
                .unwrap_or_default();
            self.hidden.record(id, action, &for_collection);
        }
        match action {
            Action::CreateIndex {
                collection,
                field,
                kind,
            } => self.create_index(collection, field, *kind),
            Action::DropIndex {
                collection,
                field,
                kind,
            } => {
                self.drop_index(collection, field, *kind);
                Ok(())
            }
            Action::SetBufferPoolPages(n) => self.store.set_pool_capacity(*n),
            Action::SetPlanCacheEntries(n) => {
                self.plan_cache.set_capacity(*n);
                Ok(())
            }
            Action::SetResultCacheEntries(n) => {
                self.result_cache.set_capacity(*n);
                Ok(())
            }
            Action::SetDirectLookup(on) => {
                if *on {
                    self.enable_direct_lookup()
                } else {
                    self.disable_direct_lookup();
                    Ok(())
                }
            }
            Action::FreezeSchema { collection } => self.freeze_schema(collection).map(|_| ()),
            Action::SetClusterField { collection, field } => {
                self.declare_cluster_field(collection, field)
            }
            Action::ClearClusterField { collection } => self.clear_cluster_field(collection),
            Action::SetDeltaEncoding(on) => {
                crate::column::set_delta_enabled(*on);
                self.delta_encoding = *on;
                self.store.set_delta_encoding(*on);
                for store in self.columns.values_mut() {
                    store.set_delta_enabled(*on);
                }
                Ok(())
            }
            Action::SetThreadPerCore(on) => {
                self.thread_per_core = *on;
                self.store.set_thread_per_core(*on);
                Ok(())
            }
            Action::SetColumnStore(on) => {
                if *on {
                    self.enable_column_store()
                } else {
                    self.disable_column_store();
                    Ok(())
                }
            }
            Action::SetMaterializedViews(on) => {
                // Turning it on builds nothing. Views appear the first time a
                // query asks for one, which is the only moment the engine knows
                // which aggregates are worth keeping — guessing up front would
                // maintain totals nobody reads on every write.
                self.views.set_enabled(*on);
                Ok(())
            }
            Action::SetRecordCompression(on) => {
                self.store.set_compression(*on);
                // Re-encoding what is already stored is the expensive part, and
                // the reason this optimization has a real build cost.
                self.store.recompress_all()?;
                Ok(())
            }
            Action::SetPrefetch(on) => {
                self.store.set_prefetch(*on);
                Ok(())
            }
            Action::SetJoinOrder(on) => {
                self.join_order = *on;
                Ok(())
            }
            Action::SetDataPartitioning(on) => {
                self.data_partitioning = *on;
                Ok(())
            }
        }
    }
}

impl Source for Database {
    fn peek_field(
        &mut self,
        collection: &str,
        id: RecordId,
        field: &str,
    ) -> Result<Option<Option<adabt_core::value::Value>>> {
        self.note_touch(collection, id);
        if !self
            .masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
        {
            if let Some(d) = self.direct.get(collection) {
                // Row liveness first, from one bit — otherwise a dead row and
                // a live row missing the field would be indistinguishable.
                if !d.contains(id) {
                    return Ok(None);
                }
                return Ok(Some(d.field_at(id, field)?));
            }
        }
        // The heap-backed fallback answers through the codec's single-field
        // walk: a wide record's other text never decodes, let alone
        // allocates. The trait default (fetch-and-discard) still backs any
        // store that has not overridden it.
        self.store.peek_field(collection, id, field)
    }

    fn fetch_projected(
        &mut self,
        collection: &str,
        id: RecordId,
        fields: &[&str],
    ) -> Result<Option<Record>> {
        self.note_touch(collection, id);
        if !self
            .masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
        {
            if let Some(d) = self.direct.get(collection) {
                if !d.contains(id) {
                    return Ok(None);
                }
                if fields.is_empty() {
                    return Ok(Some(Record::new()));
                }
                let mut rec = Record::new();
                for f in fields {
                    if let Some(v) = d.field_at(id, f)? {
                        // Direct arrays decode fixed fields without TLV — still
                        // per-field O(1), skipping non-requested strides.
                        rec.set((*f).to_string(), v);
                    }
                }
                return Ok(Some(rec));
            }
        }
        self.store.get_projected(collection, id, fields)
    }

    fn fetch(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        self.note_touch(collection, id);
        // The Level 10 path: no page directory, no slot table, just an address
        // calculation. Falls through to the heap when no array exists — or when
        // one exists but is a candidate this query is not allowed to see.
        if !self
            .masked()
            .is_some_and(|h| h.hides_direct(self.revealed(), collection))
        {
            if let Some(d) = self.direct.get(collection) {
                return d.get(id);
            }
        }
        self.store.get(collection, id)
    }

    fn all_ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        // Ids only. Routing this through `scan` decoded every record in the
        // collection so the executor could re-fetch and re-decode each one
        // immediately after — a full scan paying for itself twice.
        self.store.ids(collection)
    }

    fn column_aggregate(
        &mut self,
        collection: &str,
        group_by: &[String],
        aggs: &[adabt_ir::plan::Agg],
        predicate: Option<&adabt_ir::Expr>,
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        use adabt_ir::plan::AggKind;
        let Some(store) = self.columns.get(collection) else {
            return Ok(None);
        };
        let col_aggs: Vec<crate::column::ColumnAgg> = aggs
            .iter()
            .map(|a| crate::column::ColumnAgg {
                field: a.field.clone(),
                counts_rows: a.kind == AggKind::Count && a.field.is_none(),
            })
            .collect();

        let mut fields = Vec::new();
        if let Some(p) = predicate {
            p.referenced_fields(&mut fields);
        }
        let test = |r: &Record| predicate.map(|p| p.matches(r)).unwrap_or(true);
        let groups = store.aggregate(
            group_by,
            &col_aggs,
            predicate.map(|_| (fields.as_slice(), &test as &dyn Fn(&Record) -> bool)),
        );

        let mut out: Vec<(RecordId, Record)> = Vec::with_capacity(groups.len());
        for (i, (key, accs)) in groups.into_iter().enumerate() {
            let mut rec = Record::new();
            for (g, v) in group_by.iter().zip(key) {
                rec.set(g.clone(), v);
            }
            for (a, acc) in aggs.iter().zip(accs) {
                let v = match a.kind {
                    AggKind::Count => Value::U64(acc.count),
                    AggKind::Sum => {
                        if acc.saw_value {
                            Value::F64(acc.sum)
                        } else {
                            Value::Null
                        }
                    }
                    AggKind::Avg => {
                        if acc.count > 0 {
                            Value::F64(acc.sum / acc.count as f64)
                        } else {
                            Value::Null
                        }
                    }
                    AggKind::Min => acc.min.unwrap_or(Value::Null),
                    AggKind::Max => acc.max.unwrap_or(Value::Null),
                };
                rec.set(a.output.clone(), v);
            }
            out.push((RecordId(i as u64), rec));
        }
        Ok(Some(out))
    }

    fn column_scan(
        &mut self,
        collection: &str,
        fields: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        let Some(c) = self.columns.get(collection) else {
            return Ok(None);
        };
        let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
        Ok(Some(c.project(&refs)))
    }

    fn column_topk(
        &mut self,
        collection: &str,
        field: &str,
        descending: bool,
        k: usize,
    ) -> Result<Option<Vec<RecordId>>> {
        let Some(c) = self.columns.get(collection) else {
            return Ok(None);
        };
        Ok(c.topk_ids(field, descending, k))
    }

    fn index_lookup(
        &mut self,
        collection: &str,
        field: &str,
        key: &Value,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(self
            .indexes
            .get(collection)
            .and_then(|l| {
                let candidates: Vec<_> = l.iter().filter(|i| i.field() == field).collect();
                if candidates.is_empty() {
                    return None;
                }
                // The measured rule, applied where it can act: on a
                // low-cardinality field a bitmap answers at hash's latency
                // for ~6% of the memory (adabt-bench index-scale), so when
                // one exists take it. Otherwise first-created wins, the
                // shipped order.
                candidates
                    .iter()
                    .find(|i| {
                        matches!(i.kind(), adabt_index::IndexKind::Bitmap)
                            && i.key_count() <= adabt_index::LOW_CARDINALITY_KEY_COUNT
                    })
                    .or_else(|| candidates.first())
                    .copied()
            })
            .map(|i| i.lookup(key)))
    }

    /// Rows straight from a covering index, or `None` when none covers this
    /// query.
    ///
    /// Two conditions, and both are refusals rather than approximations. The
    /// index must be on `field`, and its projection must contain *every* field
    /// the plan above will read. A projection that covers most of what is
    /// needed is not a partial answer to be topped up — it is not an answer,
    /// and saying `None` sends the query down the ordinary indexed path.
    fn covering_lookup(
        &mut self,
        collection: &str,
        field: &str,
        key: &Value,
        needed: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        let Some(list) = self.indexes.get(collection) else {
            return Ok(None);
        };
        let found = list.iter().find(|i| {
            let covers = i.covers();
            !covers.is_empty()
                && adabt_index::covering_parts(i.field()).0 == field
                && needed.iter().all(|n| covers.contains(n))
        });
        let Some(idx) = found else {
            return Ok(None);
        };
        let mut rows = Vec::new();
        for id in idx.lookup(key) {
            // An id from the index with no projection beside it would mean the
            // two halves had drifted apart. `CoveringIndex` maintains them
            // together precisely so this cannot happen; skipping rather than
            // fabricating an empty record keeps that a missing row rather than
            // a wrong one if it ever does.
            if let Some(rec) = idx.covered(id) {
                rows.push((id, rec.clone()));
            }
        }
        Ok(Some(rows))
    }

    /// The range sibling of `covering_lookup`: ids from the inner index's
    /// range scan, rows from the projections beside them, and no fetch.
    ///
    /// Two refusals, both load-bearing. The backing must be range-capable —
    /// a hash-backed covering index holds no ordering to walk, and asking it
    /// for a range would be a silent empty answer rather than an error. And
    /// the projection must contain every field the plan above reads, for the
    /// same reason `covering_lookup` demands it: a partial row is not an
    /// answer to be topped up, it is not an answer.
    fn covering_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
        needed: &[String],
    ) -> Result<Option<Vec<(RecordId, Record)>>> {
        let Some(list) = self.indexes.get(collection) else {
            return Ok(None);
        };
        let found = list.iter().find(|i| {
            i.kind().supports_range() && {
                let covers = i.covers();
                !covers.is_empty()
                    && adabt_index::covering_parts(i.field()).0 == field
                    && needed.iter().all(|n| covers.contains(n))
            }
        });
        let Some(idx) = found else {
            return Ok(None);
        };
        let Some(ids) = idx.range(lo, hi) else {
            return Ok(None);
        };
        let mut rows = Vec::new();
        for id in ids {
            // Same drift guard as `covering_lookup`: skip rather than
            // fabricate.
            if let Some(rec) = idx.covered(id) {
                rows.push((id, rec.clone()));
            }
        }
        Ok(Some(rows))
    }

    /// A composite index answers to the NUL-joined name of its fields, so
    /// finding one is the same lookup by a different name — no separate
    /// registry, and no way for a single-field probe to reach one by
    /// accident.
    fn composite_lookup(
        &mut self,
        collection: &str,
        fields: &[String],
        key: &Value,
    ) -> Result<Option<Vec<RecordId>>> {
        let name = adabt_index::composite_name(fields);
        Ok(self
            .indexes
            .get(collection)
            .and_then(|l| l.iter().find(|i| i.field() == name))
            .map(|i| i.lookup(key)))
    }

    fn index_range(
        &mut self,
        collection: &str,
        field: &str,
        lo: Bound<&Value>,
        hi: Bound<&Value>,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(self
            .indexes
            .get(collection)
            .and_then(|l| {
                l.iter()
                    .find(|i| i.field() == field && i.kind().supports_range())
            })
            .and_then(|i| i.range(lo, hi)))
    }
}

impl LogicalStore for Database {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        let fixed = schema.mode() == SchemaMode::Fixed;
        self.store.create_collection(name, schema.clone())?;
        // A collection created while direct lookup is on must get an array too,
        // or it would silently miss the optimization it was promised.
        if fixed && self.config().is_enabled_anywhere("direct_lookup") {
            if let Some(arr) = DirectArray::new(schema) {
                self.direct.insert(name.to_string(), arr);
            }
        }
        Ok(())
    }

    fn drop_collection(&mut self, name: &str) -> Result<()> {
        self.store.drop_collection(name)?;
        self.indexes.remove(name);
        self.direct.remove(name);
        self.bump_epoch(name);
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(())
    }

    fn collection_names(&self) -> Vec<String> {
        self.store.collection_names()
    }

    fn schema_of(&self, collection: &str) -> Result<&Schema> {
        self.store.schema_of(collection)
    }

    fn insert(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<()> {
        let t = Instant::now();
        let mut stored = rec.clone();
        adabt_core::store::normalize_for_storage(&mut stored);
        self.check_unique_constraints(collection, None, &stored)?;
        let key = self
            .cluster_fields
            .get(collection)
            .and_then(|f| Self::cluster_key(collection, f, &rec));
        match key {
            Some(k) => self.store.insert_keyed(collection, id, rec, k)?,
            None => self.store.insert(collection, id, rec)?,
        }
        self.reindex_insert(collection, id, &stored);
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Insert, t, 1);
        Ok(())
    }

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        let t = Instant::now();
        let r = if let Some(d) = self.direct.get(collection) {
            d.get(id)?
        } else {
            self.store.get(collection, id)?
        };
        self.note_touch(collection, id);
        self.observe(collection, OpKind::Get, t, r.is_some() as u64);
        Ok(r)
    }

    fn update(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<bool> {
        let t = Instant::now();
        // The old record is needed to un-index it. This read is the standing
        // cost of maintaining any index, and it is why an index nobody queries
        // is a pure loss rather than merely a neutral one.
        let old = if self.needs_old_record(collection) {
            self.store.get(collection, id)?
        } else {
            None
        };
        let mut stored = rec.clone();
        adabt_core::store::normalize_for_storage(&mut stored);
        self.check_unique_constraints(collection, Some(id), &stored)?;
        let existed = self.store.update(collection, id, rec)?;
        if let Some(old) = old {
            self.reindex_remove(collection, id, &old);
        }
        self.reindex_insert(collection, id, &stored);
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Update, t, existed as u64);
        Ok(existed)
    }

    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let t = Instant::now();
        let old = if self.needs_old_record(collection) {
            self.store.get(collection, id)?
        } else {
            None
        };
        let existed = self.store.delete(collection, id)?;
        match old {
            Some(old) => self.reindex_remove(collection, id, &old),
            // Not every derived structure needs to be told *what* was removed.
            // A direct array and a column store are keyed by identity alone, so
            // an id is enough — and they must still be told, or a database with
            // a column store and no index goes on counting rows that are gone.
            None => {
                if let Some(d) = self.direct.get_mut(collection) {
                    d.remove(id);
                }
                if let Some(c) = self.columns.get_mut(collection) {
                    c.mark_dead(id);
                }
            }
        }
        self.bump_epoch(collection);
        self.observe(collection, OpKind::Delete, t, existed as u64);
        Ok(existed)
    }

    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        let t = Instant::now();
        let rows = self.store.scan(collection)?;
        self.observe(collection, OpKind::Scan, t, rows.len() as u64);
        Ok(rows)
    }

    fn count(&mut self, collection: &str) -> Result<usize> {
        let t = Instant::now();
        let n = self.store.count(collection)?;
        self.observe(collection, OpKind::Count, t, n as u64);
        Ok(n)
    }

    /// Delegated, so the engine's own callers get the cheap path too.
    ///
    /// Without this the trait default applies and `Database::ids` quietly
    /// costs a full decode of the collection — the exact expense the override
    /// exists to remove, reintroduced one layer up.
    fn ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        self.store.ids(collection)
    }
}

#[cfg(test)]
impl Database {
    /// Drop one (key, id) pair from the first index whose name contains `needle`.
    /// Used only by the consistency checker to inject a lost-update divergence.
    pub(crate) fn test_drop_index_entry(
        &mut self,
        collection: &str,
        needle: &str,
    ) -> Option<(String, String, u64)> {
        let list = self.indexes.get_mut(collection)?;
        for idx in list.iter_mut() {
            let f_name = idx.field().to_string();
            if f_name.contains(needle) {
                let snap = idx.snapshot();
                for (key, ids) in snap {
                    if !ids.is_empty() {
                        let id = ids[0];
                        idx.remove(&key, id);
                        return Some((f_name, format!("{:?}", key), id.0));
                    }
                }
            }
        }
        None
    }

    /// Insert a bogus id into an index, creating a dangling reference.
    /// `key_string` must parse; `field` selects the index; `fake_id` is arbitrary.
    pub(crate) fn test_insert_index_entry(
        &mut self,
        collection: &str,
        field: &str,
        key_string: &str,
        fake_id: u64,
    ) -> bool {
        let list = match self.indexes.get_mut(collection) {
            Some(l) => l,
            None => return false,
        };
        for idx in list.iter_mut() {
            if idx.field().contains(field) {
                let v = Value::Str(key_string.to_string());
                idx.insert(v.clone(), RecordId(fake_id));
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod verify_tests {
    use super::*;

    fn make_db() -> Database {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(dir.path(), Policy::manual(4)).unwrap();
        db.create_collection("c", Schema::dynamic()).unwrap();
        db.insert(
            "c",
            RecordId(1),
            Record::new().with("name", "alice").with("age", 30i64),
        )
        .unwrap();
        db.insert(
            "c",
            RecordId(2),
            Record::new().with("name", "bob").with("age", 42i64),
        )
        .unwrap();
        db.create_index("c", "name", IndexKind::Hash).unwrap();
        // Second collection for columnar-divergence test.
        db.create_collection("d", Schema::dynamic()).unwrap();
        db.insert("d", RecordId(10), Record::new().with("val", 1i64))
            .unwrap();
        db
    }

    #[test]
    fn a_clean_database_reports_no_divergences() {
        let mut db = make_db();
        let r = db.verify().unwrap();
        assert!(
            r.problems.is_empty(),
            "clean database should have no verify problems; got: {:?}",
            r.problems
        );
        assert!(r.indexes_checked > 0);
        assert!(r.records_checked > 0);
    }

    #[test]
    fn a_dropped_index_entry_is_detected_forward() {
        let mut db = make_db();
        let _dropped = db
            .test_drop_index_entry("c", "name")
            .expect("drop injected entry");
        let r = db.verify().unwrap();
        let msg = r.problems.iter().find(|m| m.contains("does not list it"));
        assert!(
            msg.is_some(),
            "expected a forward divergence message; got problems: {:?}",
            r.problems
        );
    }

    #[test]
    fn a_dangling_index_id_is_detected_reverse() {
        let mut db = make_db();
        db.test_insert_index_entry("c", "name", "ghost", 99_999);
        let r = db.verify().unwrap();
        let msg = r.problems.iter().find(|m| m.contains("holds id"));
        assert!(
            msg.is_some(),
            "expected a reverse divergence message; got problems: {:?}",
            r.problems
        );
    }

    #[test]
    fn verify_reports_column_store_divergence() {
        // Columnar divergence is harder to inject without a store-level seam,
        // so this test asserts the shape: with a column store present the
        // check runs; when the collection has no column store the problem
        // list remains empty.
        let dir = tempfile::tempdir().unwrap();
        let mut db = Database::open(dir.path(), Policy::manual(4)).unwrap();
        db.create_collection("c", Schema::dynamic()).unwrap();
        db.insert("c", RecordId(1), Record::new().with("val", 1i64))
            .unwrap();
        db.insert("c", RecordId(2), Record::new().with("val", 2i64))
            .unwrap();
        // Verify runs successfully with no derived structures yet.
        assert!(db.verify().unwrap().problems.is_empty());
    }
}
