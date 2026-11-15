//! Read-only views of the store's internals, for the planner, the engine's
//! metrics, and tests that assert on physical behaviour.

use adabt_core::ids::RecordId;
use adabt_core::policy::Durability;
use std::path::Path;

use super::migration::is_migration_name;
use super::HeapStore;
use crate::pager::BufferStats;

impl HeapStore {
    /// Approximate live row count without taking `&mut`.
    ///
    /// Used by the planner's calibrated cost model: an index that matches a
    /// large fraction of the collection can be more expensive than a scan, but
    /// that comparison needs both `rows` and `distinct`. This is the `rows`
    /// half, and keeping it `&self` lets `plan_context` be `&self`.
    pub fn live_count(&self, collection: &str) -> Option<u64> {
        let c = self.collections.get(collection)?;
        Some(c.directory.values().filter(|v| !v.is_absent()).count() as u64)
    }

    /// The highest id in `collection` whose newest version is a live record,
    /// read off the in-memory directory: no pages, no decodes.
    ///
    /// The same answer as the last row of `scan`, which filters on the same
    /// `newest()` and walks the same ordered directory, at the cost of a walk
    /// back past any trailing tombstones rather than a read of every record.
    /// The optimizer asks every cycle, inside the windows it measures.
    pub fn max_live_id(&self, collection: &str) -> adabt_core::error::Result<Option<RecordId>> {
        Ok(self
            .coll(collection)?
            .directory
            .iter()
            .rev()
            .find_map(|(id, v)| v.newest().map(|_| *id)))
    }

    /// Every collection and the id its pages are stamped with, sorted by name.
    ///
    /// Exposed for tests that need to prove the binding did not move. It is the
    /// one piece of state where "the same, exactly" is the whole requirement.
    pub fn collection_ids(&self) -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> = self
            .collections
            .iter()
            .filter(|(n, _)| !is_migration_name(n))
            .map(|(n, c)| (n.clone(), c.id.0))
            .collect();
        v.sort();
        v
    }

    /// How many distinct pages `get` has touched since the last clear.
    ///
    /// This is the number a locality claim is measured against: a clustered
    /// range scan should touch pages in proportion to the *range's* size,
    /// an unclustered one in proportion to the collection's. A diagnostic,
    /// not a statistic — it accumulates under the caller's control rather
    /// than pretending to be a rate.
    pub fn touched_pages(&self) -> usize {
        self.touched.len()
    }

    /// Reset the touched-page set.
    pub fn clear_page_touches(&mut self) {
        self.touched.clear();
    }

    pub fn pool_stats(&self) -> crate::pager::BufferStats {
        self.pool.stats()
    }

    /// Log fsyncs so far. Exposed so a batch API's whole reason for existing —
    /// fewer of these — is something a test can assert on rather than time.
    pub fn sync_count(&self) -> u64 {
        self.wal.sync_count()
    }

    pub fn buffer_stats(&self) -> BufferStats {
        self.pool.stats()
    }
    pub fn page_count(&self) -> u32 {
        self.pool.page_count()
    }
    pub fn wal_appended(&self) -> u64 {
        self.wal.appended()
    }
    pub fn wal_syncs(&self) -> u64 {
        self.wal.sync_count()
    }
    pub fn durability(&self) -> Durability {
        self.wal.durability()
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
