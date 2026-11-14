//! Returning space: dropping versions no snapshot can reach, releasing a
//! discarded collection's slots, and vacuuming trailing pages back to the
//! operating system.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;

use super::{Collection, HeapStore};
use crate::page::{PageId, RecordLocation};

impl HeapStore {
    /// Release every page slot a collection's live records occupy.
    pub(super) fn free_pages_of(&mut self, c: &Collection) -> Result<()> {
        let locs: Vec<RecordLocation> = c
            .directory
            .values()
            .flat_map(|chain| chain.versions.iter().filter_map(|(_, l)| *l))
            .collect();
        let mut touched: Vec<PageId> = Vec::new();
        for loc in locs {
            let page = self.pool.get_mut(loc.page)?;
            let _ = page.delete(loc.slot);
            if !touched.contains(&loc.page) {
                touched.push(loc.page);
            }
        }
        // Deleting a slot leaves its bytes in place until the page is compacted,
        // so without this a migration's freed space is never reusable.
        for pid in touched {
            let page = self.pool.get_mut(pid)?;
            if page.fragmentation() > 0 {
                page.compact();
            }
            let free = page.free_space();
            self.fsm.set(pid, free);
        }
        Ok(())
    }

    /// Free versions no open snapshot can reach.
    ///
    /// Returns how many were dropped. With a snapshot open this is bounded by
    /// its position, so a long-running reader keeps exactly what it needs alive
    /// and nothing more.
    pub fn reclaim(&mut self) -> Result<usize> {
        let horizon = self.versions.reclaim_horizon();
        let mut freed_locations = Vec::new();
        for c in self.collections.values_mut() {
            let mut empty_ids = Vec::new();
            for (id, chain) in c.directory.iter_mut() {
                freed_locations.extend(chain.reclaim_to(horizon));
                // A chain holding only a tombstone no reader can precede is
                // just overhead.
                if chain.len() == 1 && chain.is_absent() {
                    empty_ids.push(*id);
                }
            }
            for id in empty_ids {
                c.directory.remove(&id);
            }
        }
        let count = freed_locations.len();
        let mut touched: Vec<PageId> = Vec::new();
        for loc in freed_locations {
            let page = self.pool.get_mut(loc.page)?;
            let _ = page.delete(loc.slot);
            if !touched.contains(&loc.page) {
                touched.push(loc.page);
            }
        }
        // Deleting a slot leaves its payload in place until the page is
        // compacted, so without this the freed bytes are never reusable and a
        // versioned update stream grows the file without bound.
        for pid in touched {
            let page = self.pool.get_mut(pid)?;
            if page.fragmentation() > 0 {
                page.compact();
            }
            let free = page.free_space();
            self.fsm.set(pid, free);
        }
        self.retained = self.retained.saturating_sub(count);
        Ok(count)
    }

    /// Give trailing heap pages back to the operating system.
    ///
    /// The free-space map already lets a deleted record's space be reused, so a
    /// steady workload does not grow the file without bound. What it cannot do
    /// is *return* anything: a collection dropped, or the old copy left behind by
    /// a schema migration, leaves holes that only a future insert can fill. An
    /// operator with a full disk has no answer to that except restoring from a
    /// backup, which is not an answer.
    ///
    /// So live records are moved out of the last pages into the holes earlier in
    /// the file, and the tail is then truncated. Each move goes through the
    /// ordinary write path — a new version at the new location, the old one
    /// reclaimable — so a crash part-way through leaves the record readable in
    /// one place or the other, never neither.
    ///
    /// Returns the number of pages returned.
    pub fn vacuum(&mut self) -> Result<u32> {
        // Dead versions first: they may be the only thing holding a tail page.
        self.reclaim()?;

        let mut freed = 0u32;
        loop {
            let count = self.pool.page_count();
            if count == 0 {
                break;
            }
            let last = PageId(count - 1);
            let occupants = self.live_records_on(last)?;
            if occupants.is_empty() {
                self.pool.truncate_to(count - 1)?;
                self.fsm.forget(last);
                freed += 1;
                continue;
            }
            // Take the page out of the free-space map first, or `place` is at
            // liberty to put the record straight back where it came from.
            self.fsm.forget(last);
            let mut moved_any = false;
            for (collection, id) in occupants {
                if self.relocate(&collection, id, last)? {
                    moved_any = true;
                }
            }
            if !moved_any {
                // Nothing on the last page could be moved anywhere earlier, so
                // the file is as short as it can be made without splitting
                // records across pages.
                break;
            }
            self.reclaim()?;
        }
        Ok(freed)
    }

    /// Which records currently live on a page.
    fn live_records_on(&self, page: PageId) -> Result<Vec<(String, RecordId)>> {
        let mut out = Vec::new();
        for (name, c) in &self.collections {
            for (id, chain) in &c.directory {
                if chain.newest().is_some_and(|l| l.page == page) {
                    out.push((name.clone(), *id));
                }
            }
        }
        Ok(out)
    }

    /// Move one record off `from`, if there is room for it earlier.
    fn relocate(&mut self, collection: &str, id: RecordId, from: PageId) -> Result<bool> {
        let Some(loc) = self
            .collections
            .get(collection)
            .and_then(|c| c.directory.get(&id))
            .and_then(|ch| ch.newest())
        else {
            return Ok(false);
        };
        if loc.page != from {
            return Ok(false);
        }
        let payload = self.pool.get(loc.page)?.get(loc.slot)?.to_vec();
        let need = crate::page::Page::cost_of(payload.len());
        if self.fsm.find(need).is_none() {
            return Ok(false);
        }
        let new_loc = self.place(&payload)?;
        let txn = self.versions.begin_write();
        if let Some(c) = self.collections.get_mut(collection) {
            if let Some(chain) = c.directory.get_mut(&id) {
                chain.push(txn, Some(new_loc));
                self.retained += 1;
            }
        }
        Ok(true)
    }

    /// Reclaim once retention grows past a threshold.
    ///
    /// Driven by pressure rather than by every write: reclamation walks every
    /// chain, and doing that per write would cost more than the versions do.
    pub(super) fn maybe_reclaim(&mut self) -> Result<()> {
        const RETENTION_LIMIT: usize = 4096;
        if self.retained >= RETENTION_LIMIT {
            self.reclaim()?;
        }
        Ok(())
    }
}
