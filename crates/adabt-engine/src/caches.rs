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
use std::collections::HashMap;

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

    pub fn insert(&mut self, shape: QueryShape, plan: PlanDecision) {
        if !self.enabled() {
            return;
        }
        if self.entries.insert(shape, plan).is_none() {
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
    }

    fn evict_one(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let victim = self.order.remove(0);
        self.entries.remove(&victim);
        self.stats.evictions += 1;
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

    pub fn insert(&mut self, key: QueryKey, collection: &str, epoch: u64, rows: Rows) {
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
                    rows,
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
        self.stats.invalidations += 1;
    }

    /// Rough memory held, for the resource axis.
    pub fn memory_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|e| {
                e.collection.len()
                    + e.rows
                        .iter()
                        .map(|(_, r)| r.iter().map(|(k, _)| k.len() + 48).sum::<usize>() + 32)
                        .sum::<usize>()
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
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
        c.insert(QueryShape(1), plan("users"));
        assert!(c.get(QueryShape(1)).is_some());
        assert_eq!(c.stats().hits, 1);
        assert_eq!(c.stats().misses, 1);
    }

    #[test]
    fn a_zero_capacity_cache_is_disabled_not_broken() {
        let mut c = PlanCache::new(0);
        c.insert(QueryShape(1), plan("users"));
        assert!(c.get(QueryShape(1)).is_none());
        assert!(c.is_empty());
        assert_eq!(c.stats().hits, 0);
    }

    #[test]
    fn a_plan_cache_evicts_when_full() {
        let mut c = PlanCache::new(2);
        for i in 1..=5 {
            c.insert(QueryShape(i), plan("users"));
        }
        assert_eq!(c.len(), 2);
        assert!(c.stats().evictions >= 3);
    }

    #[test]
    fn shrinking_a_plan_cache_evicts_down_immediately() {
        let mut c = PlanCache::new(10);
        for i in 1..=10 {
            c.insert(QueryShape(i), plan("users"));
        }
        c.set_capacity(3);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn clearing_a_plan_cache_is_recorded() {
        let mut c = PlanCache::new(4);
        c.insert(QueryShape(1), plan("users"));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.stats().invalidations, 1);
    }

    #[test]
    fn a_result_cache_hit_requires_a_matching_epoch() {
        let mut c = ResultCache::new(4);
        c.insert(QueryKey(1), "users", 7, rows(3));
        assert_eq!(c.get(QueryKey(1), 7).map(|r| r.len()), Some(3));
        // A write bumped the epoch: the entry must not be served.
        assert!(c.get(QueryKey(1), 8).is_none());
        assert!(c.stats().invalidations > 0);
    }

    #[test]
    fn a_stale_entry_is_dropped_rather_than_rechecked() {
        let mut c = ResultCache::new(4);
        c.insert(QueryKey(1), "users", 1, rows(3));
        assert!(c.get(QueryKey(1), 2).is_none());
        assert!(c.is_empty(), "a stale entry was left in place");
    }

    #[test]
    fn invalidating_one_collection_leaves_the_others_alone() {
        let mut c = ResultCache::new(8);
        c.insert(QueryKey(1), "users", 1, rows(2));
        c.insert(QueryKey(2), "orders", 1, rows(2));
        c.invalidate_collection("users");
        assert!(c.get(QueryKey(1), 1).is_none());
        assert!(c.get(QueryKey(2), 1).is_some());
    }

    #[test]
    fn result_cache_memory_grows_with_content() {
        let mut c = ResultCache::new(64);
        let empty = c.memory_bytes();
        c.insert(QueryKey(1), "users", 1, rows(500));
        assert!(c.memory_bytes() > empty + 1000);
        c.clear();
        assert_eq!(c.memory_bytes(), 0);
    }

    #[test]
    fn hit_rate_is_none_before_any_probe() {
        assert_eq!(CacheStats::default().hit_rate(), None);
        let mut c = PlanCache::new(2);
        c.insert(QueryShape(1), plan("u"));
        c.get(QueryShape(1));
        assert_eq!(c.stats().hit_rate(), Some(1.0));
    }
}
