//! Plan and result caches.
//!
//! Two caches with genuinely different keys, for genuinely different reasons.
//! The plan cache is keyed by `QueryShape`, so every `country = ?` shares one
//! entry — plans depend on structure, not on values. The result cache is keyed
//! by `QueryKey`, which includes the literals, because different values return
//! different rows.
//!
//! Correctness rests on **epoch invalidation**: every collection carries a
//! counter bumped on any write, and a cached result records the epoch it was
//! computed at. A stale entry is therefore impossible to serve rather than
//! merely unlikely to be. A time-based or size-based scheme would make staleness
//! a probability, and a cache that is *usually* right is a correctness bug with
//! good odds.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_exec::planner::PlanDecision;
use adabt_ir::{QueryKey, QueryShape};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub invalidations: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> Option<f64> {
        let n = self.hits + self.misses;
        if n == 0 {
            None
        } else {
            Some(self.hits as f64 / n as f64)
        }
    }
}

/// Caches *access decisions* by query shape.
///
/// Deliberately not physical plans. A plan contains literals; a shape has them
/// erased. Caching plans by shape means two queries that share a shape — which
/// is exactly the case where their literals differ — can be served each other's
/// plan, silently answering the wrong question. Caching the decision and
/// rebuilding the plan around the current literals keeps the benefit and makes
/// that failure impossible to express.
#[derive(Default)]
pub struct PlanCache {
    entries: HashMap<QueryShape, PlanDecision>,
    /// Insertion order, for eviction.
    order: Vec<QueryShape>,
    capacity: usize,
    stats: CacheStats,
}

impl PlanCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ..Default::default()
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn stats(&self) -> CacheStats {
        self.stats
    }
    pub fn enabled(&self) -> bool {
        self.capacity > 0
    }

    /// Resize, evicting down if shrinking. Zero disables the cache entirely.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.entries.len() > self.capacity {
            self.evict_one();
        }
    }

    fn evict_one(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let victim = self.order.remove(0);
        self.entries.remove(&victim);
        self.stats.evictions += 1;
    }

    pub fn get(&mut self, shape: QueryShape) -> Option<&PlanDecision> {
        if !self.enabled() {
            return None;
        }
        if self.entries.contains_key(&shape) {
            self.stats.hits += 1;
            self.entries.get(&shape)
        } else {
            self.stats.misses += 1;
            None
        }
    }

    /// Cache a decision, producing it only if it will be kept — see
    /// [`ResultCache::insert`] for why this takes a closure rather than a
    /// value. Cheaper here than there, and wrong for the same reason.
    pub fn insert(&mut self, shape: QueryShape, plan: impl FnOnce() -> PlanDecision) {
        if !self.enabled() {
            return;
        }
        if self.entries.insert(shape, plan()).is_none() {
            self.order.push(shape);
            while self.entries.len() > self.capacity {
                self.evict_one();
            }
        }
    }

    /// Drop every plan.
    ///
    /// Called whenever an index appears or disappears: a cached plan encodes an
    /// access path that may no longer exist, and serving one would either be
    /// slower than necessary or reference a structure that is gone.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.stats.invalidations += 1;
    }
}

type Rows = Vec<(RecordId, Record)>;

/// Entries a result cache holds when the optimizer turns it on, and therefore
/// the size of the ghost that predicts what such a cache would do while it is
/// off.
pub const RESULT_CACHE_ENTRIES: usize = 256;

/// The telemetry name the ghost's would-be hits and misses are recorded under.
///
/// Kept apart from `"result_cache"` so a simulated hit rate is never mistaken
/// for a measured one, and so a cache that is on is judged by what it actually
/// served.
pub const RESULT_CACHE_GHOST: &str = "result_cache_ghost";

struct ResultEntry {
    collection: String,
    epoch: u64,
    rows: Rows,
}

/// Caches query results, invalidated by collection epoch.
#[derive(Default)]
pub struct ResultCache {
    entries: HashMap<QueryKey, ResultEntry>,
    order: Vec<QueryKey>,
    capacity: usize,
    stats: CacheStats,
    /// What a cache of [`RESULT_CACHE_ENTRIES`] would hold, kept only while
    /// this one is off.
    ghost: Ghost,
}

/// One remembered key: everything the real cache keeps except the rows.
struct GhostEntry {
    collection: String,
    epoch: u64,
    bytes: usize,
}

/// A result cache that stores keys and sizes but no rows.
///
/// **Whether a result cache pays is a fact about the traffic, not about the
/// database**: it pays when the same query, literals included, comes back
/// before a write to its collection makes the answer stale. No estimate can
/// know that, and a cache that is off cannot measure it. A ghost can: it runs
/// the real cache's admission, eviction and invalidation rules on the keys
/// alone, so its hit rate is the hit rate the cache would have had, and its
/// byte count is the memory the cache would have used. Both are what the
/// optimizer otherwise had to guess.
///
/// The cost is a hash lookup per query and a few dozen bytes per remembered
/// key, bounded by [`RESULT_CACHE_ENTRIES`].
#[derive(Default)]
struct Ghost {
    entries: HashMap<QueryKey, GhostEntry>,
    order: VecDeque<QueryKey>,
    bytes: usize,
}

impl Ghost {
    /// Whether the real cache would have answered this query. A stale entry
    /// is dropped, as the real cache drops it.
    fn probe(&mut self, key: QueryKey, epoch: u64) -> bool {
        match self.entries.get(&key) {
            Some(e) if e.epoch == epoch => true,
            Some(_) => {
                self.remove(key);
                false
            }
            None => false,
        }
    }

    /// Remember a key the real cache would now hold, evicting oldest-first as
    /// it would.
    fn admit(&mut self, key: QueryKey, collection: &str, epoch: u64, bytes: usize) {
        if self.entries.contains_key(&key) {
            return;
        }
        self.entries.insert(
            key,
            GhostEntry {
                collection: collection.to_string(),
                epoch,
                bytes,
            },
        );
        self.order.push_back(key);
        self.bytes += bytes;
        while self.entries.len() > RESULT_CACHE_ENTRIES {
            match self.order.front().copied() {
                Some(oldest) => self.remove(oldest),
                None => break,
            }
        }
    }

    fn remove(&mut self, key: QueryKey) {
        if let Some(e) = self.entries.remove(&key) {
            self.bytes -= e.bytes;
            self.order.retain(|k| *k != key);
        }
    }

    fn invalidate_collection(&mut self, collection: &str) {
        self.entries.retain(|_, e| e.collection != collection);
        let entries = &self.entries;
        self.order.retain(|k| entries.contains_key(k));
        self.bytes = self.entries.values().map(|e| e.bytes).sum();
    }

    fn clear(&mut self) {
        *self = Ghost::default();
    }
}

/// Approximate bytes a cached copy of `rows` occupies, on the same scale as
/// [`ResultCache::memory_bytes`].
///
/// Measured on the first row and scaled, not summed over every row: this runs
/// on every query the ghost sees, and walking a large result set to size it
/// would cost a noticeable fraction of producing it. Rows of one result share
/// a projection, so the first is representative.
fn approx_rows_bytes(collection: &str, rows: &Rows) -> usize {
    let per_row = rows.first().map_or(0, |(_, r)| row_bytes(r));
    collection.len() + per_row * rows.len()
}

fn row_bytes(r: &Record) -> usize {
    r.iter().map(|(k, _)| k.len() + 48).sum::<usize>() + 32
}

impl ResultCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ..Default::default()
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn stats(&self) -> CacheStats {
        self.stats
    }
    pub fn enabled(&self) -> bool {
        self.capacity > 0
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.entries.len() > self.capacity {
            self.evict_one();
        }
        // A real cache now measures itself; the ghost would only shadow it.
        if self.enabled() {
            self.ghost.clear();
        }
    }

    fn evict_one(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let victim = self.order.remove(0);
        self.entries.remove(&victim);
        self.stats.evictions += 1;
    }

    /// While the cache is off: whether it would have answered this query.
    ///
    /// Always `false` while it is on — the real lookup answers that, and a
    /// second opinion would double-count the traffic.
    pub fn would_hit(&mut self, key: QueryKey, epoch: u64) -> bool {
        !self.enabled() && self.ghost.probe(key, epoch)
    }

    /// While the cache is off: note a result it would now hold.
    ///
    /// Takes the rows by reference and keeps only their size, so a disabled
    /// cache still never makes the caller build a copy it will discard.
    pub fn note_would_insert(&mut self, key: QueryKey, collection: &str, epoch: u64, rows: &Rows) {
        if self.enabled() {
            return;
        }
        let bytes = approx_rows_bytes(collection, rows);
        self.ghost.admit(key, collection, epoch, bytes);
    }

    /// Bytes this cache holds, or while it is off, would hold on the traffic
    /// seen so far. The resource half of deciding whether to turn it on.
    pub fn resident_bytes(&self) -> usize {
        if self.enabled() {
            self.memory_bytes()
        } else {
            self.ghost.bytes
        }
    }

    /// Look up, treating any epoch mismatch as a miss.
    pub fn get(&mut self, key: QueryKey, epoch: u64) -> Option<&Rows> {
        if !self.enabled() {
            return None;
        }
        match self.entries.get(&key) {
            Some(e) if e.epoch == epoch => {
                self.stats.hits += 1;
                self.entries.get(&key).map(|e| &e.rows)
            }
            Some(_) => {
                // Stale. Drop it rather than leaving it to be re-checked.
                self.entries.remove(&key);
                self.order.retain(|k| *k != key);
                self.stats.misses += 1;
                self.stats.invalidations += 1;
                None
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    /// Cache a result set, producing it only if it will actually be kept.
    ///
    /// `rows` is a closure rather than a value, and that is the whole point:
    /// the caller's rows are borrowed — they are about to be returned to the
    /// application — so caching them means cloning them. Taking `Rows` by
    /// value made every caller clone an entire result set *before* this
    /// function could decide it was disabled and drop the clone on the floor.
    ///
    /// A scan returning 50,000 records paid 50,000 record clones per query for
    /// a cache that was switched off. The answer was correct, so nothing that
    /// compares answers could see it.
    ///
    /// Taking a closure makes that unrepresentable rather than merely fixed:
    /// there is no way to write a call site that pays for a result this
    /// function is going to discard.
    pub fn insert(
        &mut self,
        key: QueryKey,
        collection: &str,
        epoch: u64,
        rows: impl FnOnce() -> Rows,
    ) {
        if !self.enabled() {
            return;
        }
        let fresh = self
            .entries
            .insert(
                key,
                ResultEntry {
                    collection: collection.to_string(),
                    epoch,
                    rows: rows(),
                },
            )
            .is_none();
        if fresh {
            self.order.push(key);
            while self.entries.len() > self.capacity {
                self.evict_one();
            }
        }
    }
    /// Drop every entry for one collection.
    pub fn invalidate_collection(&mut self, collection: &str) {
        self.ghost.invalidate_collection(collection);
        let before = self.entries.len();
        self.entries.retain(|_, e| e.collection != collection);
        self.order.retain(|k| self.entries.contains_key(k));
        if self.entries.len() != before {
            self.stats.invalidations += 1;
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.ghost.clear();
        self.stats.invalidations += 1;
    }

    /// Rough memory held, for the resource axis.
    pub fn memory_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|e| e.collection.len() + e.rows.iter().map(|(_, r)| row_bytes(r)).sum::<usize>())
            .sum()
    }
}

#[cfg(test)]
mod tests {

    /// A disabled cache must not make the caller produce what it will discard.
    ///
    /// This is the whole reason `insert` takes a closure. The rows a query
    /// returns belong to the caller, so handing them to the cache means
    /// cloning them — and when the cache is off, that clone was built, passed
    /// in, and dropped. On a 50,000-row scan it was 50,000 record clones per
    /// query, and it cost roughly half of what a scan cost.
    ///
    /// Nothing that checks answers could see it: the rows were correct either
    /// way. So the assertion is about *work*, and the closure makes the work
    /// observable — if it runs, the caller paid.
    #[test]
    fn a_disabled_cache_never_asks_for_the_rows() {
        let mut c = ResultCache::new(0);
        assert!(!c.enabled(), "capacity 0 must mean disabled");

        let asked = std::cell::Cell::new(false);
        c.insert(QueryKey(1), "users", 0, || {
            asked.set(true);
            vec![(RecordId(1), Record::new().with("a", 1i64))]
        });

        assert!(
            !asked.get(),
            "a disabled cache asked the caller to build a result set it then threw away"
        );
    }

    /// The other half of the contract: an enabled cache must still ask, and
    /// must actually keep what it is given. A closure that is never called
    /// would make the test above pass trivially.
    #[test]
    fn an_enabled_cache_asks_once_and_keeps_the_result() {
        let mut c = ResultCache::new(4);
        let asked = std::cell::Cell::new(0u32);
        let rows = vec![(RecordId(1), Record::new().with("a", 1i64))];

        c.insert(QueryKey(1), "users", 7, || {
            asked.set(asked.get() + 1);
            rows.clone()
        });

        assert_eq!(asked.get(), 1, "an enabled cache must ask exactly once");
        assert_eq!(c.get(QueryKey(1), 7), Some(&rows));
    }
    use super::*;
    use adabt_exec::planner::AccessDecision;

    fn plan(_name: &str) -> PlanDecision {
        PlanDecision {
            access: AccessDecision::FullScan,
            rationale: "test".into(),
        }
    }

    fn rows(n: u64) -> Rows {
        (0..n)
            .map(|i| (RecordId(i), Record::new().with("i", i as i64)))
            .collect()
    }

    #[test]
    fn a_plan_cache_hit_returns_the_stored_plan() {
        let mut c = PlanCache::new(4);
        assert!(c.get(QueryShape(1)).is_none());
        c.insert(QueryShape(1), || plan("users"));
        assert!(c.get(QueryShape(1)).is_some());
        assert_eq!(c.stats().hits, 1);
        assert_eq!(c.stats().misses, 1);
    }

    #[test]
    fn a_zero_capacity_cache_is_disabled_not_broken() {
        let mut c = PlanCache::new(0);
        c.insert(QueryShape(1), || plan("users"));
        assert!(c.get(QueryShape(1)).is_none());
        assert!(c.is_empty());
        assert_eq!(c.stats().hits, 0);
    }

    #[test]
    fn a_plan_cache_evicts_when_full() {
        let mut c = PlanCache::new(2);
        for i in 1..=5 {
            c.insert(QueryShape(i), || plan("users"));
        }
        assert_eq!(c.len(), 2);
        assert!(c.stats().evictions >= 3);
    }

    #[test]
    fn shrinking_a_plan_cache_evicts_down_immediately() {
        let mut c = PlanCache::new(10);
        for i in 1..=10 {
            c.insert(QueryShape(i), || plan("users"));
        }
        c.set_capacity(3);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn clearing_a_plan_cache_is_recorded() {
        let mut c = PlanCache::new(4);
        c.insert(QueryShape(1), || plan("users"));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.stats().invalidations, 1);
    }

    #[test]
    fn a_result_cache_hit_requires_a_matching_epoch() {
        let mut c = ResultCache::new(4);
        c.insert(QueryKey(1), "users", 7, || rows(3));
        assert_eq!(c.get(QueryKey(1), 7).map(|r| r.len()), Some(3));
        // A write bumped the epoch: the entry must not be served.
        assert!(c.get(QueryKey(1), 8).is_none());
        assert!(c.stats().invalidations > 0);
    }

    #[test]
    fn a_stale_entry_is_dropped_rather_than_rechecked() {
        let mut c = ResultCache::new(4);
        c.insert(QueryKey(1), "users", 1, || rows(3));
        assert!(c.get(QueryKey(1), 2).is_none());
        assert!(c.is_empty(), "a stale entry was left in place");
    }

    #[test]
    fn invalidating_one_collection_leaves_the_others_alone() {
        let mut c = ResultCache::new(8);
        c.insert(QueryKey(1), "users", 1, || rows(2));
        c.insert(QueryKey(2), "orders", 1, || rows(2));
        c.invalidate_collection("users");
        assert!(c.get(QueryKey(1), 1).is_none());
        assert!(c.get(QueryKey(2), 1).is_some());
    }

    #[test]
    fn result_cache_memory_grows_with_content() {
        let mut c = ResultCache::new(64);
        let empty = c.memory_bytes();
        c.insert(QueryKey(1), "users", 1, || rows(500));
        assert!(c.memory_bytes() > empty + 1000);
        c.clear();
        assert_eq!(c.memory_bytes(), 0);
    }

    #[test]
    fn a_disabled_cache_predicts_the_hits_an_enabled_one_would_have() {
        // The ghost's whole claim: same traffic, same verdicts as the real
        // cache, without holding a single row.
        let mut off = ResultCache::new(0);
        let mut on = ResultCache::new(RESULT_CACHE_ENTRIES);
        let traffic = [(1, 0), (2, 0), (1, 0), (1, 1), (2, 1), (2, 1), (3, 1)];
        for (key, epoch) in traffic {
            let key = QueryKey(key);
            let predicted = off.would_hit(key, epoch);
            let actual = on.get(key, epoch).is_some();
            assert_eq!(
                predicted, actual,
                "ghost disagreed on {key:?} at epoch {epoch}"
            );
            off.note_would_insert(key, "users", epoch, &rows(2));
            on.insert(key, "users", epoch, || rows(2));
        }
        assert!(off.is_empty(), "the ghost stored rows");
    }

    #[test]
    fn the_ghost_reports_the_memory_the_cache_would_use() {
        let mut off = ResultCache::new(0);
        let mut on = ResultCache::new(RESULT_CACHE_ENTRIES);
        for key in 0..10 {
            off.note_would_insert(QueryKey(key), "users", 0, &rows(20));
            on.insert(QueryKey(key), "users", 0, || rows(20));
        }
        assert_eq!(off.resident_bytes(), on.resident_bytes());
    }

    #[test]
    fn the_ghost_forgets_a_collection_that_was_written_to() {
        let mut off = ResultCache::new(0);
        off.note_would_insert(QueryKey(1), "users", 0, &rows(2));
        off.invalidate_collection("users");
        assert!(!off.would_hit(QueryKey(1), 0));
        assert_eq!(off.resident_bytes(), 0);
    }

    #[test]
    fn the_ghost_is_bounded_by_the_size_the_cache_would_have() {
        let mut off = ResultCache::new(0);
        for key in 0..(RESULT_CACHE_ENTRIES as u64 * 2) {
            off.note_would_insert(QueryKey(key), "users", 0, &rows(1));
        }
        assert!(
            !off.would_hit(QueryKey(0), 0),
            "the oldest key was not evicted"
        );
        assert!(off.would_hit(QueryKey(RESULT_CACHE_ENTRIES as u64 * 2 - 1), 0));
    }

    #[test]
    fn enabling_the_cache_discards_the_ghost() {
        let mut c = ResultCache::new(0);
        c.note_would_insert(QueryKey(1), "users", 0, &rows(2));
        c.set_capacity(RESULT_CACHE_ENTRIES);
        assert_eq!(c.resident_bytes(), 0);
        assert!(!c.would_hit(QueryKey(1), 0));
    }

    #[test]
    fn hit_rate_is_none_before_any_probe() {
        assert_eq!(CacheStats::default().hit_rate(), None);
        let mut c = PlanCache::new(2);
        c.insert(QueryShape(1), || plan("u"));
        c.get(QueryShape(1));
        assert_eq!(c.stats().hit_rate(), Some(1.0));
    }
}
