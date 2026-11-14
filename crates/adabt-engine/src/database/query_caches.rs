//! Memoised planning and results, and the per-collection epochs that keep a
//! cached result from outliving the data it was computed from.

use std::collections::HashMap;

use crate::caches::{PlanCache, ResultCache};
use crate::compiled::CompiledPaths;

/// Everything the query path memoises: planning decisions, compiled shapes and
/// whole results, with the epochs that invalidate the last.
pub(super) struct QueryCaches {
    pub(super) plans: PlanCache,
    pub(super) results: ResultCache,
    /// Bumped on every write, so a cached result can never be stale.
    epochs: HashMap<String, u64>,
    /// Shapes specialised past the general query path.
    pub(super) compiled: CompiledPaths,
}

impl QueryCaches {
    pub(super) fn new() -> Self {
        Self {
            plans: PlanCache::new(0),
            results: ResultCache::new(0),
            epochs: HashMap::new(),
            compiled: CompiledPaths::new(),
        }
    }

    pub(super) fn bump_epoch(&mut self, collection: &str) {
        *self.epochs.entry(collection.to_string()).or_default() += 1;
        self.results.invalidate_collection(collection);
    }

    pub(super) fn epoch(&self, collection: &str) -> u64 {
        self.epochs.get(collection).copied().unwrap_or(0)
    }

    /// Forget every cached planning decision and compiled path: both encode
    /// which structures existed when they were made.
    pub(super) fn invalidate_plans(&mut self) {
        self.plans.clear();
        self.compiled.clear();
    }
}
