//! Returning space: dropping versions no snapshot can reach, releasing a
//! discarded collection's slots, and vacuuming trailing pages back to the
//! operating system.
//!
//! Every slot is freed through [`HeapStore::release`], which frees a slot
//! only while it still holds the record it is being freed for. For a version
//! this process wrote that always holds. For one recovery loaded from the
//! directory snapshot it may not: after that checkpoint the slot may have been
//! freed and handed to another record, and the page written to disk like
//! that. See `HeapStore::retire_recovered` for when that happens and why
//! skipping such a slot is the right answer.

use adabt_core::error::Result;
use adabt_core::ids::{CollectionId, RecordId, TxnId};
use std::collections::{BTreeSet, HashSet};

use super::mvcc::RECOVERED;
use super::{Collection, HeapStore};
use crate::page::{Page, PageId, RecordLocation, SlotId};
use crate::wal::WalOp;

/// A slot to free, and the record it must still hold to be freed.
pub(super) struct Release {
    pub(super) loc: RecordLocation,
    pub(super) collection: CollectionId,
    pub(super) id: RecordId,
    /// The version was loaded from the directory snapshot rather than
    /// written by this process.
    pub(super) recovered: bool,
}

impl Release {
    pub(super) fn of(
        collection: CollectionId,
        id: RecordId,
        txn: TxnId,
        loc: RecordLocation,
    ) -> Self {
        Self {
            loc,
            collection,
            id,
            recovered: txn == RECOVERED,
        }
    }
}

impl HeapStore {
    /// Free `slots`, then compact every page that lost one so the space can be
    /// reused. Returns how many were freed.
    ///
    /// A slot that no longer holds the record named for it is left alone.
    /// That is expected only of a version recovery loaded (see the module
    /// documentation); for any other it means the directory and the pages
    /// disagree, which debug builds treat as the bug it is.
    pub(super) fn release(&mut self, slots: Vec<Release>) -> Result<usize> {
        let pages = self.pool.page_count();
        let mut touched = BTreeSet::new();
        let mut freed = 0;
        for r in slots {
            let holds = r.loc.page.0 < pages
                && self
                    .pool
                    .get(r.loc.page)?
                    .get(r.loc.slot)
                    .ok()
                    .and_then(Self::payload_owner)
                    == Some((r.collection, r.id));
            if !holds {
                debug_assert!(
                    r.recovered,
                    "{:?} does not hold record {} of collection {}",
                    r.loc, r.id.0, r.collection.0
                );
                continue;
            }
            self.pool.get_mut(r.loc.page)?.delete(r.loc.slot)?;
            touched.insert(r.loc.page);
            freed += 1;
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
        Ok(freed)
    }

    /// Release every page slot a collection's versions occupy.
    pub(super) fn free_pages_of(&mut self, c: &Collection) -> Result<()> {
        let slots = c
            .directory
            .iter()
            .flat_map(|(id, chain)| {
                chain
                    .versions
                    .iter()
                    .filter_map(move |(txn, loc)| loc.map(|l| Release::of(c.id, *id, *txn, l)))
            })
            .collect();
        self.release(slots).map(|_| ())
    }

    /// Free versions no open snapshot can reach.
    ///
    /// Returns how many were dropped. With a snapshot open this is bounded by
    /// its position, so a long-running reader keeps exactly what it needs alive
    /// and nothing more.
    pub fn reclaim(&mut self) -> Result<usize> {
        let horizon = self.versions.reclaim_horizon();
        let mut slots = Vec::new();
        for c in self.collections.values_mut() {
            let mut empty_ids = Vec::new();
            for (id, chain) in c.directory.iter_mut() {
                for (txn, loc) in chain.reclaim_to(horizon) {
                    slots.push(Release::of(c.id, *id, txn, loc));
                }
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
        let count = slots.len();
        self.release(slots)?;
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
    /// So, after freeing every slot nothing refers to, live records are moved
    /// out of the last pages into the holes earlier in the file, and the tail
    /// is then truncated.
    ///
    /// A move is a logged write like any other — an `Update` carrying the
    /// record's own bytes — because recovery trusts the directory snapshot's
    /// location for every record the log does not mention (see
    /// [`crate::heap`]). The log is made durable before any old copy is
    /// freed, so a crash at any point leaves each record readable where the
    /// directory snapshot says it is, or moved by a log entry that recovery
    /// replays.
    ///
    /// Returns the number of pages returned.
    pub fn vacuum(&mut self) -> Result<u32> {
        // Dead versions and unreferenced slots first: they may be the only
        // thing holding a tail page.
        self.reclaim()?;
        self.release_unreferenced_slots()?;

        let before = self.pool.page_count();
        let mut end = before;
        let mut moved = false;
        while end > 0 {
            let last = PageId(end - 1);
            // Take the page out of the free-space map first, or `place` is at
            // liberty to put the record straight back where it came from.
            self.fsm.forget(last);
            let mut stuck = false;
            for (collection, id) in self.live_records_on(last)? {
                if self.relocate(&collection, id, last)? {
                    moved = true;
                } else {
                    stuck = true;
                }
            }
            if stuck {
                // Something on this page fits nowhere earlier, so the file is
                // as short as it can be made without splitting records across
                // pages. The page stays, and stays available for placement.
                let free = self.pool.get(last)?.free_space();
                self.fsm.set(last, free);
                break;
            }
            end -= 1;
        }
        if moved {
            // Every move durable before any old copy is freed: once a freed
            // slot reaches the heap file, the log is the only record of where
            // its record went.
            self.wal.sync()?;
            self.reclaim()?;
        }

        // Truncate only what no version occupies, not just what no live
        // record does: an open snapshot may still be reading an old version
        // on a tail page.
        let keep = self.pages_in_use();
        if keep < before {
            self.pool.truncate_to(keep)?;
            for p in keep..before {
                self.fsm.forget(PageId(p));
            }
            for c in self.collections.values_mut() {
                if let Some(cluster) = c.cluster.as_mut() {
                    cluster.forget_from(PageId(keep));
                }
            }
        }
        Ok(before - keep)
    }

    /// Free every live slot that no version of any record refers to.
    ///
    /// Such slots are what a crash leaves behind: writes from after the last
    /// checkpoint that the buffer pool stole to disk, and that replay then
    /// placed afresh elsewhere, or versions an in-process snapshot was holding
    /// when a checkpoint flushed them. Nothing ever reads them, so they are
    /// harmless, but only a pass over every page can find them, and vacuum is
    /// that pass anyway.
    fn release_unreferenced_slots(&mut self) -> Result<usize> {
        let referenced: HashSet<RecordLocation> = self
            .collections
            .values()
            .flat_map(|c| c.directory.values())
            .flat_map(|chain| chain.versions.iter().filter_map(|(_, loc)| *loc))
            .collect();
        let mut freed = 0;
        for p in 0..self.pool.page_count() {
            let pid = PageId(p);
            let orphans: Vec<SlotId> = self
                .pool
                .get(pid)?
                .slots()
                .filter(|&slot| !referenced.contains(&RecordLocation { page: pid, slot }))
                .collect();
            if orphans.is_empty() {
                continue;
            }
            let page = self.pool.get_mut(pid)?;
            for slot in &orphans {
                page.delete(*slot)?;
            }
            page.compact();
            let free = page.free_space();
            self.fsm.set(pid, free);
            freed += orphans.len();
        }
        Ok(freed)
    }

    /// One past the last page any version of any record occupies.
    fn pages_in_use(&self) -> u32 {
        self.collections
            .values()
            .flat_map(|c| c.directory.values())
            .flat_map(|chain| chain.versions.iter().filter_map(|(_, loc)| *loc))
            .map(|loc| loc.page.0 + 1)
            .max()
            .unwrap_or(0)
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
    ///
    /// The move is logged and handed to the operating system before the new
    /// copy is placed, but not made durable here; `vacuum` does that once,
    /// before it frees any old copy.
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
        if self.fsm.find(Page::cost_of(payload.len())).is_none() {
            return Ok(false);
        }
        self.wal.append(
            TxnId(0),
            WalOp::Update {
                collection: collection.to_string(),
                id,
                bytes: Self::payload_record(&payload)?,
            },
        )?;
        self.wal.flush()?;
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
