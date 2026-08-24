//! Shared-nothing execution across partitions.
//!
//! A [`ShardedDatabase`] is *N* complete databases. Each shard has its own
//! directory, its own heap file, its own write-ahead log, its own buffer pool,
//! its own indexes and caches, its own optimizer and its own lock. Nothing is
//! shared between them, which is the entire point: two requests for records in
//! different shards contend for nothing and proceed genuinely in parallel.
//!
//! Records are assigned by identity — `RecordId % shards` — so every operation
//! addressing one record touches exactly one shard, and the routing decision
//! costs a remainder.
//!
//! # What this is and is not
//!
//! It is shared-nothing partitioning, and the parallelism is real: a point
//! lookup takes one shard's lock and nothing else's, and a scan runs on every
//! shard at once.
//!
//! It is **not** thread-per-core. There is no core pinning, no run-to-completion
//! scheduler, no `io_uring`, and no attempt to keep a shard's memory on the node
//! that owns it. Those are the things that make partitioning worth the last
//! factor of two or three, and they are a different piece of work built on this
//! one. Calling this "per-core" would be claiming that work is done.
//!
//! # Why the answers cannot change
//!
//! The rule this whole project is built on applies here more sharply than
//! anywhere else, because partitioning changes the *order* work happens in and
//! order is where floating-point aggregates and sort stability go wrong.
//!
//! So the split is drawn where it is provably safe. Shards run the scan and any
//! filters — both are per-row and neither depends on what any other row did.
//! Their results are merged **by record id**, reproducing exactly the order an
//! unpartitioned scan returns. Everything after that — sorting, limiting,
//! aggregation — runs once, centrally, over rows in that order.
//!
//! In particular no aggregate is ever computed per shard and combined.
//! Combining partial sums means adding floating-point numbers in an order that
//! depends on how many shards there are, and the answer would then depend on the
//! partitioning. Aggregation over merged rows costs the same as it did before
//! and is bit-identical by construction.
//!
//! A `Limit` is likewise never pushed down: each shard would apply it to its own
//! rows and the merged result would be missing whatever the others held.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_exec::exec::{execute_with_budget, ExecBudget, ExecStats, Source};
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::Database;

pub struct ShardedDatabase {
    shards: Vec<Arc<Mutex<Database>>>,
    /// Round-robins auto-allocated inserts across shards, for load rather than
    /// correctness. Correctness comes from the `seq * shard_count + shard_index`
    /// encoding, which cannot collide however this is chosen.
    next_shard: std::sync::atomic::AtomicUsize,
}

/// Which shard owns a record.
///
/// The remainder of the identity, not a hash of it. A hash would spread better
/// for adversarial key distributions; the remainder keeps consecutive ids on
/// different shards, which is what a range scan and a bulk load both want, and
/// it is what makes the routing free.
#[inline]
fn shard_of(id: RecordId, shards: usize) -> usize {
    (id.0 % shards as u64) as usize
}

impl ShardedDatabase {
    /// Open `shards` partitions under `dir`, each in its own subdirectory.
    pub fn open(dir: &Path, shards: usize, policy: Policy) -> Result<Self> {
        if shards == 0 {
            return Err(Error::InvalidOptimization(
                "a sharded database needs at least one shard".into(),
            ));
        }
        // One clock across every shard, not one per shard. Nothing today reads
        // a timestamp across shards, so this changes nothing observable yet —
        // but it is what lets a future cross-shard reader compare "when" two
        // shards' writes happened at all, and retrofitting it once shards hold
        // real data would mean rewriting every timestamp already written.
        let versions = Arc::new(adabt_storage::version::VersionTracker::new());
        let mut out = Vec::with_capacity(shards);
        for i in 0..shards {
            let db = Database::open_shared(
                &dir.join(format!("shard-{i}")),
                policy.clone(),
                Arc::clone(&versions),
            )?;
            out.push(Arc::new(Mutex::new(db)));
        }
        Ok(Self {
            shards: out,
            next_shard: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// A shard, for inspecting what it decided on its own.
    ///
    /// Each shard optimizes independently from its own traffic, so they need not
    /// agree — and on a skewed workload they will not. That is a feature of
    /// partitioning worth being able to look at rather than an inconsistency.
    pub fn shard(&self, i: usize) -> Option<&Arc<Mutex<Database>>> {
        self.shards.get(i)
    }

    fn owner(&self, id: RecordId) -> &Mutex<Database> {
        &self.shards[shard_of(id, self.shards.len())]
    }

    /// Run `f` on every shard at once, in shard order.
    ///
    /// Scoped threads rather than a pool: the work is one burst per query, the
    /// shards outlive it, and a pool would add a queue between the caller and
    /// the only thing it is waiting for.
    fn broadcast<T, F>(&self, f: F) -> Result<Vec<T>>
    where
        T: Send,
        F: Fn(&mut Database) -> Result<T> + Sync,
    {
        // One shard is the common case in tests and in small deployments, and
        // spawning a thread to talk to yourself is pure overhead.
        if self.shards.len() == 1 {
            let mut guard = lock(&self.shards[0]);
            return Ok(vec![f(&mut guard)?]);
        }
        let f = &f;
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .shards
                .iter()
                .map(|s| {
                    scope.spawn(move || {
                        let mut guard = lock(s);
                        f(&mut guard)
                    })
                })
                .collect();
            let mut out = Vec::with_capacity(handles.len());
            for h in handles {
                // A panicking shard is a bug in the engine, not a condition to
                // recover from: it has left its own state unknown, and carrying
                // on with the other shards' answers would report a partial
                // database as a whole one.
                match h.join() {
                    Ok(r) => out.push(r?),
                    Err(_) => return Err(Error::Corruption("a shard panicked mid-query".into())),
                }
            }
            Ok(out)
        })
    }

    /// The part of a plan a shard may run: the scan and any filters above it.
    ///
    /// Stops at the first operator whose result depends on rows other than the
    /// one in hand. Projection could be pushed too, but a projected row is a
    /// worse input to whatever runs centrally afterwards, so it is not.
    fn pushdown(root: &LogicalOp) -> LogicalOp {
        match root {
            LogicalOp::Scan { .. } | LogicalOp::GetById { .. } | LogicalOp::GetByIds { .. } => {
                root.clone()
            }
            LogicalOp::Filter { input, predicate } => LogicalOp::Filter {
                input: Box::new(Self::pushdown(input)),
                predicate: predicate.clone(),
            },
            // Sort, Limit and Aggregate all read across rows. Anything above one
            // of them stays central, and so does it.
            other => Self::pushdown(other.child().expect("non-leaf has a child")),
        }
    }

    /// Whether the plan is nothing but scan-and-filter, so the merge is the
    /// whole answer and nothing needs to run centrally.
    fn is_only_pushdown(root: &LogicalOp) -> bool {
        match root {
            LogicalOp::Scan { .. } | LogicalOp::GetById { .. } | LogicalOp::GetByIds { .. } => true,
            LogicalOp::Filter { input, .. } => Self::is_only_pushdown(input),
            _ => false,
        }
    }

    pub fn query(&self, plan: &LogicalPlan) -> Result<Vec<(RecordId, Record)>> {
        self.query_with(plan, None)
    }

    /// [`Self::query`], but another thread can stop it early by setting
    /// `cancel`. The same flag reaches every shard's scan *and* the merge
    /// step that runs once the shards have answered — see
    /// `Database::query_cancellable`, which each shard's half of this call
    /// goes through.
    pub fn query_cancellable(
        &self,
        plan: &LogicalPlan,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<(RecordId, Record)>> {
        self.query_with(plan, Some(cancel))
    }

    fn query_with(
        &self,
        plan: &LogicalPlan,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Vec<(RecordId, Record)>> {
        // Checked before `pushdown` ever runs, not after: `pushdown` falls
        // back to `other.child().expect(...)` for anything it does not name
        // explicitly, and `LogicalOp::child()` panics outright on a `Join` —
        // by design, the same design that makes this an actual panic today
        // rather than a merely-unimplemented path. Two collections are each
        // partitioned independently by `RecordId % shards`, so a matching
        // join pair can land on different shards; joining across that split
        // is real cross-shard execution (broadcast or shuffle), a
        // substantially larger problem than the single-node join algorithm
        // `Database::query` now has, and not one this milestone takes on. A
        // caller wanting a join against sharded data reaches a single
        // shard's own `Database` directly via `shard()`.
        if plan.root.contains_join() {
            return Err(Error::Unsupported(
                "joins across a sharded database are not implemented yet; query a single shard's Database directly via ShardedDatabase::shard".into(),
            ));
        }
        // A query naming one record needs one shard and no merge at all.
        if let LogicalOp::GetById { id, .. } = &plan.root {
            let mut guard = lock(self.owner(*id));
            return match &cancel {
                Some(c) => guard.query_cancellable(plan, Arc::clone(c)),
                None => guard.query(plan),
            };
        }

        let pushed = LogicalPlan::new(Self::pushdown(&plan.root));
        let per_shard = self.broadcast(|db| match &cancel {
            Some(c) => db.query_cancellable(&pushed, Arc::clone(c)),
            None => db.query(&pushed),
        })?;
        let mut rows = merge_by_id(per_shard);

        if Self::is_only_pushdown(&plan.root) {
            return Ok(rows);
        }
        // The rest of the plan, once, over the merged rows in record-id order —
        // which is the order an unpartitioned scan would have produced, so the
        // arithmetic that follows is the same arithmetic.
        let mut source = MergedRows::new(std::mem::take(&mut rows));
        let physical =
            adabt_exec::planner::plan(&plan.root, &adabt_exec::planner::PlanContext::empty());
        let mut stats = ExecStats::default();
        // Every shard was opened with the same policy (`open` clones it to
        // each), so any one of them names the budget that applies here.
        let max_ram_bytes = lock(&self.shards[0])
            .policy()
            .constraints
            .max_query_ram_bytes;
        let budget = ExecBudget {
            max_ram_bytes,
            cancel,
        };
        execute_with_budget(&physical, &mut source, &mut stats, &budget)
    }

    /// Run one optimization cycle on every shard.
    pub fn optimize(&self) -> Result<()> {
        self.broadcast(|db| db.optimize().map(|_| ()))?;
        Ok(())
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.broadcast(|db| db.checkpoint())?;
        Ok(())
    }

    /// Send each shard's discarded log segments to its own subdirectory of
    /// `dir` — `dir/shard-0`, `dir/shard-1`, … — mirroring the layout
    /// `backup_to` and `open` already use, so an archive and a backup of the
    /// same database line up shard for shard.
    ///
    /// See `Database::set_log_archive` for why this is what makes
    /// point-in-time recovery reach past a backup's own checkpoint.
    pub fn set_log_archive(&self, dir: Option<&Path>) {
        for (i, shard) in self.shards.iter().enumerate() {
            let per_shard = dir.map(|d| d.join(format!("shard-{i}")));
            lock(shard).set_log_archive(per_shard);
        }
    }

    /// Make `dest` a complete, independently openable copy of every shard,
    /// laid out exactly as `open` expects — `dest/shard-0`, `dest/shard-1`,
    /// … — so `ShardedDatabase::open(dest, shard_count, policy)` opens the
    /// result directly, at whatever shard count this database currently has.
    ///
    /// One shard at a time, deliberately: each shard is already independently
    /// lockable, and interleaving their backups would not make any single
    /// shard's backup faster — `Database::backup_to` is dominated by its own
    /// checkpoint and file copy, not by contention with the others.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        for (i, shard) in self.shards.iter().enumerate() {
            lock(shard).backup_to(&dest.join(format!("shard-{i}")))?;
        }
        Ok(())
    }

    pub fn explain_optimizations(&self) -> String {
        self.shards
            .iter()
            .enumerate()
            .map(|(i, s)| format!("--- shard {i} ---\n{}", lock(s).explain_optimizations()))
            .collect::<Vec<_>>()
            .join("")
    }

    /// Prometheus exposition text, one block per shard.
    ///
    /// Not merged into one series set: shards observe independent traffic and
    /// are not expected to agree, which `explain_optimizations` already
    /// treats as a feature rather than noise this has any business hiding by
    /// summing it away.
    pub fn metrics_text(&self) -> String {
        self.shards
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    "# shard {i}\n{}",
                    adabt_telemetry::to_prometheus_text(&lock(s).telemetry())
                )
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Take a lock, ignoring poisoning.
///
/// A poisoned shard means some earlier caller panicked while holding it. The
/// data behind it is durable — every write went through the log before the lock
/// was taken — so refusing to serve anything ever again is a worse answer than
/// carrying on.
fn lock(m: &Mutex<Database>) -> std::sync::MutexGuard<'_, Database> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Merge per-shard results into one ascending run.
///
/// Each shard returns its rows in record-id order and no id appears in two
/// shards, so this is an ordinary k-way merge and the result is exactly what an
/// unpartitioned scan returns. That equality is the load-bearing property of the
/// whole module: everything downstream then behaves identically.
fn merge_by_id(mut per_shard: Vec<Vec<(RecordId, Record)>>) -> Vec<(RecordId, Record)> {
    match per_shard.len() {
        0 => return Vec::new(),
        1 => return per_shard.pop().unwrap_or_default(),
        _ => {}
    }
    let total: usize = per_shard.iter().map(|v| v.len()).sum();
    let mut cursors = vec![0usize; per_shard.len()];
    let mut out = Vec::with_capacity(total);
    for _ in 0..total {
        let mut best: Option<usize> = None;
        for (i, rows) in per_shard.iter().enumerate() {
            let Some((id, _)) = rows.get(cursors[i]) else {
                continue;
            };
            if best.is_none_or(|b| *id < per_shard[b][cursors[b]].0) {
                best = Some(i);
            }
        }
        let Some(i) = best else { break };
        let row = std::mem::take(&mut per_shard[i][cursors[i]]);
        cursors[i] += 1;
        out.push(row);
    }
    out
}

/// Rows already in memory, presented as something the executor can read.
struct MergedRows {
    rows: std::collections::BTreeMap<RecordId, Record>,
}

impl MergedRows {
    fn new(rows: Vec<(RecordId, Record)>) -> Self {
        Self {
            rows: rows.into_iter().collect(),
        }
    }
}

impl Source for MergedRows {
    fn fetch(&mut self, _collection: &str, id: RecordId) -> Result<Option<Record>> {
        Ok(self.rows.get(&id).cloned())
    }
    fn all_ids(&mut self, _collection: &str) -> Result<Vec<RecordId>> {
        Ok(self.rows.keys().copied().collect())
    }
    /// No indexes here, and declining is the correct answer rather than a
    /// limitation: these rows have already been filtered by the shards, using
    /// whatever indexes those shards had. An index at this level would be an
    /// index over a temporary.
    fn index_lookup(
        &mut self,
        _collection: &str,
        _field: &str,
        _key: &adabt_core::value::Value,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(None)
    }
    fn index_range(
        &mut self,
        _collection: &str,
        _field: &str,
        _lo: std::ops::Bound<&adabt_core::value::Value>,
        _hi: std::ops::Bound<&adabt_core::value::Value>,
    ) -> Result<Option<Vec<RecordId>>> {
        Ok(None)
    }
}

/// The natural API, which takes `&self`.
///
/// A shard locks itself, so the collection as a whole never needs to be
/// exclusively borrowed — which is what lets many threads use one
/// `ShardedDatabase` at the same time without a lock around the outside. The
/// [`LogicalStore`] impl below takes `&mut self` because the trait does, and
/// delegates here; the trait exists so a sharded database can be put through the
/// same differential runner as everything else, not because it is the better way
/// to call one.
impl ShardedDatabase {
    pub fn create_collection(&self, name: &str, schema: Schema) -> Result<()> {
        // Every shard holds every collection: partitioning is by record, not by
        // collection, so a shard that did not know the schema could not decode
        // the records it owns.
        self.broadcast(|db| db.create_collection(name, schema.clone()))?;
        Ok(())
    }

    pub fn drop_collection(&self, name: &str) -> Result<()> {
        self.broadcast(|db| db.drop_collection(name))?;
        Ok(())
    }

    /// Change a collection's schema on every shard.
    ///
    /// Every shard starts from the same schema and is given the same target,
    /// so every shard makes the same in-place-or-copy-and-swap decision; what
    /// differs per shard is only how many of *its* rows a copy-and-swap has to
    /// carry, same as any other broadcast write.
    pub fn alter_schema(&self, name: &str, schema: Schema) -> Result<()> {
        self.broadcast(|db| db.alter_schema(name, schema.clone()).map(|_| ()))?;
        Ok(())
    }

    pub fn collection_names(&self) -> Vec<String> {
        lock(&self.shards[0]).collection_names()
    }

    pub fn insert(&self, collection: &str, id: RecordId, rec: Record) -> Result<()> {
        lock(self.owner(id)).insert(collection, id, rec)
    }

    /// Insert without naming an id, and learn which one was used.
    ///
    /// **Ids are generated `seq * shard_count + shard_index`**, where `seq` is
    /// that one shard's own local counter. That guarantees two things at once:
    /// every id already satisfies `id % shard_count == shard_index`, so
    /// [`ShardedDatabase::owner`] always agrees with where the record actually
    /// lives — no cross-shard coordination is needed, or possible, to keep
    /// routing consistent — and two shards allocating concurrently can never
    /// produce the same id, because their outputs land in disjoint residue
    /// classes. A single unsharded database is `shard_count = 1`, under which
    /// this reduces to the ordinary local counter.
    ///
    /// Which shard receives a given call is round-robin, for load spreading
    /// only; nothing about correctness depends on which one is picked. The peek
    /// and the write happen under the same lock, so two calls landing on one
    /// shard cannot race each other into the same id.
    pub fn insert_auto(&self, collection: &str, rec: Record) -> Result<RecordId> {
        let shards = self.shards.len();
        let i = self
            .next_shard
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % shards;
        let mut guard = lock(&self.shards[i]);
        // The shard's own counter tracks the highest *raw, encoded* id it has
        // ever stored — that is what `apply_put` bumps it to, because a manual
        // insert has to push it past whatever id it used, encoded or not. So the
        // sequence has to be decoded back out rather than read off directly:
        // taking the raw value as `seq` would feed an already-multiplied number
        // back into the multiplication and diverge exponentially within a few
        // calls. Routing guarantees every id ever stored on shard `i` satisfies
        // `id % shards == i`, which is what makes the decode exact.
        let raw = guard.next_id(collection)?.0;
        let seq = raw.saturating_sub(i as u64).div_ceil(shards as u64);
        let id = RecordId(seq * shards as u64 + i as u64);
        guard.insert(collection, id, rec)?;
        Ok(id)
    }

    pub fn get(&self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        lock(self.owner(id)).get(collection, id)
    }

    /// Insert many records in one call, partitioned to their shards.
    ///
    /// Each shard pays for one fsync covering everything routed to it, not one
    /// per record — the same saving [`Database::insert_batch`] makes, applied
    /// per partition. Atomicity holds *within* a shard, not across all of them:
    /// a batch spanning several shards can land on some and fail on others,
    /// because there is no cross-shard transaction to make it indivisible. That
    /// is a real limitation and not a hidden one — it is the reason cross-shard
    /// transactions are their own milestone rather than a detail of this one.
    ///
    /// Returns how many were written before any error, so a partial failure is
    /// visible rather than silently swallowed.
    pub fn insert_batch(
        &self,
        collection: &str,
        records: Vec<(RecordId, Record)>,
    ) -> Result<usize> {
        let mut by_shard: Vec<Vec<(RecordId, Record)>> =
            (0..self.shards.len()).map(|_| Vec::new()).collect();
        for (id, rec) in records {
            by_shard[shard_of(id, self.shards.len())].push((id, rec));
        }
        // Every shard is attempted regardless of an earlier one's failure —
        // `?` here would have stopped at the first failing shard and left every
        // shard after it untouched, which is not "atomicity holds within a
        // shard" but "atomicity holds within a shard, unless it comes later in
        // iteration order", a much stranger and less useful promise, and one the
        // doc above does not make.
        let mut written = 0usize;
        let mut first_err = None;
        for (i, batch) in by_shard.into_iter().enumerate() {
            if batch.is_empty() {
                continue;
            }
            match lock(&self.shards[i]).insert_batch(collection, batch) {
                Ok(n) => written += n,
                Err(e) if first_err.is_none() => first_err = Some(e),
                Err(_) => {}
            }
        }
        match first_err {
            Some(e) => Err(Error::InvalidOptimization(format!(
                "batch insert failed on at least one shard after committing {written} \
                 record(s) to the others: {e}"
            ))),
            None => Ok(written),
        }
    }

    pub fn update(&self, collection: &str, id: RecordId, rec: Record) -> Result<bool> {
        lock(self.owner(id)).update(collection, id, rec)
    }

    pub fn delete(&self, collection: &str, id: RecordId) -> Result<bool> {
        lock(self.owner(id)).delete(collection, id)
    }

    pub fn scan(&self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        let per_shard = self.broadcast(|db| db.scan(collection))?;
        Ok(merge_by_id(per_shard))
    }

    pub fn count(&self, collection: &str) -> Result<usize> {
        Ok(self.broadcast(|db| db.count(collection))?.iter().sum())
    }

    /// How each shard would answer this query.
    ///
    /// Per shard rather than once, because they need not agree: a shard plans
    /// from the structures it has decided to build, and on skewed traffic those
    /// differ. One combined answer would have to pick a shard to speak for the
    /// others.
    pub fn explain(&self, plan: &LogicalPlan) -> String {
        let pushed = LogicalPlan::new(Self::pushdown(&plan.root));
        let mut s = String::new();
        for (i, shard) in self.shards.iter().enumerate() {
            s.push_str(&format!(
                "--- shard {i} ---\n{}\n",
                lock(shard).explain(&pushed)
            ));
        }
        if !Self::is_only_pushdown(&plan.root) {
            s.push_str("--- merged, then centrally ---\n");
            s.push_str(&plan.explain());
        }
        s
    }
}

impl LogicalStore for ShardedDatabase {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        ShardedDatabase::create_collection(self, name, schema)
    }
    fn drop_collection(&mut self, name: &str) -> Result<()> {
        ShardedDatabase::drop_collection(self, name)
    }
    fn collection_names(&self) -> Vec<String> {
        ShardedDatabase::collection_names(self)
    }
    fn schema_of(&self, _collection: &str) -> Result<&Schema> {
        // Returning a borrow of something behind a lock is not expressible, and
        // faking it with a leak would be worse than saying so.
        Err(Error::InvalidOptimization(
            "schema_of borrows from a shard; use shard(i) and ask it directly".into(),
        ))
    }
    fn insert(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<()> {
        ShardedDatabase::insert(self, collection, id, rec)
    }
    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        ShardedDatabase::get(self, collection, id)
    }
    fn update(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<bool> {
        ShardedDatabase::update(self, collection, id, rec)
    }
    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        ShardedDatabase::delete(self, collection, id)
    }
    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        ShardedDatabase::scan(self, collection)
    }
    fn count(&mut self, collection: &str) -> Result<usize> {
        ShardedDatabase::count(self, collection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_record_has_exactly_one_owner() {
        for shards in 1..=8usize {
            let mut seen = vec![0usize; shards];
            for i in 0..1000u64 {
                seen[shard_of(RecordId(i), shards)] += 1;
            }
            assert_eq!(seen.iter().sum::<usize>(), 1000);
            // And the split is even, which is what makes the parallelism worth
            // having: one shard holding everything is a lock with extra steps.
            let (lo, hi) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
            assert!(hi - lo <= 1, "{shards} shards split {seen:?}");
        }
    }

    fn rows(ids: &[u64]) -> Vec<(RecordId, Record)> {
        ids.iter()
            .map(|i| (RecordId(*i), Record::new().with("i", *i)))
            .collect()
    }

    #[test]
    fn merging_reproduces_an_unpartitioned_scan() {
        let merged = merge_by_id(vec![
            rows(&[0, 3, 6, 9]),
            rows(&[1, 4, 7]),
            rows(&[2, 5, 8]),
        ]);
        let ids: Vec<u64> = merged.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn merging_handles_empty_and_uneven_shards() {
        assert!(merge_by_id(vec![]).is_empty());
        assert!(merge_by_id(vec![Vec::new(), Vec::new()]).is_empty());
        let merged = merge_by_id(vec![Vec::new(), rows(&[5]), Vec::new(), rows(&[1, 2, 3])]);
        let ids: Vec<u64> = merged.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, vec![1, 2, 3, 5]);
    }

    #[test]
    fn a_limit_is_never_pushed_to_a_shard() {
        // Each shard would apply it to its own rows and the merged result would
        // be missing whatever the others held.
        let plan = LogicalOp::scan("c").limit(3);
        assert!(matches!(
            ShardedDatabase::pushdown(&plan),
            LogicalOp::Scan { .. }
        ));
        assert!(!ShardedDatabase::is_only_pushdown(&plan));
    }

    #[test]
    fn an_aggregate_is_never_computed_per_shard() {
        // Combining partial sums adds floating-point numbers in an order that
        // depends on the shard count, which would make the answer depend on the
        // partitioning.
        use adabt_ir::plan::Agg;
        let plan = LogicalOp::scan("c").aggregate(vec!["g".into()], vec![Agg::count("n")]);
        assert!(matches!(
            ShardedDatabase::pushdown(&plan),
            LogicalOp::Scan { .. }
        ));
        assert!(!ShardedDatabase::is_only_pushdown(&plan));
    }

    #[test]
    fn filters_are_pushed_and_need_no_central_pass() {
        use adabt_ir::Expr;
        let plan = LogicalOp::scan("c").filter(Expr::eq("g", "x"));
        assert!(matches!(
            ShardedDatabase::pushdown(&plan),
            LogicalOp::Filter { .. }
        ));
        assert!(ShardedDatabase::is_only_pushdown(&plan));
    }

    #[test]
    fn a_filter_under_an_aggregate_is_still_pushed() {
        use adabt_ir::plan::Agg;
        use adabt_ir::Expr;
        let plan = LogicalOp::scan("c")
            .filter(Expr::eq("g", "x"))
            .aggregate(vec![], vec![Agg::count("n")]);
        assert!(
            matches!(ShardedDatabase::pushdown(&plan), LogicalOp::Filter { .. }),
            "the filter stayed central and every shard returned every row"
        );
    }
}
