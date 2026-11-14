//! Rebuilding in-memory state on open: adopt the persisted catalog, load or
//! rebuild the page directory, then replay the log over it.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{CollectionId, Lsn, RecordId, TxnId};
use std::collections::HashMap;

use super::definitions::store_has;
use super::migration::is_migration_name;
use super::{HeapStore, RecoverTarget, VersionChain, SLOT_PREFIX};
use crate::catalog::decode_schema;
use crate::page::{PageId, RecordLocation, SlotId};
use crate::wal::{WalEntry, WalOp};

impl HeapStore {
    pub(super) fn recover(&mut self) -> Result<()> {
        // Pass 0: the persisted catalog, if there is one.
        let from_lsn = self.adopt_persisted_catalog()?;
        self.check_restore_target(from_lsn)?;
        let entries = self.read_log(from_lsn)?;
        let entries = &entries[..];

        // Pass 1: collection definitions, for whatever the catalog did not
        // already cover. The page scan below cannot interpret a stored record
        // without knowing its schema.
        self.replay_definitions(entries, from_lsn)?;

        // Pass 2: the page directory and free-space map, as of the last
        // checkpoint. Loaded from the cache when one describes that exact
        // checkpoint, and rebuilt by reading every page when it does not.
        //
        // Both produce the same directory. The scan is the definition and the
        // cache is an optimization of it, which is why a cache that cannot be
        // trusted for any reason costs a scan rather than an error.
        let flushed_lsn = last_checkpoint(entries);
        if !self.load_directory(flushed_lsn)? {
            self.scan_pages()?;
        }

        // Pass 3: replay everything the heap file might not yet contain.
        self.replay_records(entries, flushed_lsn)?;

        // Pass 4: schema migrations that reached their commit entry, in log
        // order. Deferred to here because an adoption hands over records, and
        // the records only exist once passes 2 and 3 have placed them.
        self.replay_adoptions(entries)?;
        self.discard_unfinished_migrations()?;

        // Replay appends a version onto chains the page scan already built. No
        // snapshot can span a restart, so collapsing them is both safe and
        // necessary: otherwise every restart doubles the retained history.
        self.reclaim()?;
        self.retained = 0;
        Ok(())
    }

    /// Adopt the persisted catalog, returning the LSN it already reflects.
    fn adopt_persisted_catalog(&mut self) -> Result<u64> {
        // The catalog supplies the name-to-id binding directly rather than
        // letting it fall out of replay order. That matters because a
        // collection's id is embedded in every one of its heap slots:
        // re-deriving the binding from a log that has lost its beginning
        // silently reattributes every page.
        let catalog = crate::metadata::read(&self.dir, self.identity);
        match &catalog {
            Some(cat) => {
                self.adopt_catalog(cat)?;
                Ok(cat.through_lsn)
            }
            None => {
                // No catalog. Sound only while the log still holds everything.
                let start = self.wal.start_lsn();
                if start.0 > 0 {
                    return Err(crate::metadata::missing(start));
                }
                Ok(0)
            }
        }
    }

    /// Reject a restore target that the adopted catalog has already passed.
    fn check_restore_target(&self, from_lsn: u64) -> Result<()> {
        // A restore target below what the catalog already reflects is not an
        // earlier state — the catalog Pass 0 just adopted already contains
        // writes past it, unconditionally. There is no way to "replay less"
        // out of that; the only fix is a backup taken before this one's own
        // checkpoint.
        if let RecoverTarget::Lsn(target) = self.recover_target {
            if target.0 < from_lsn {
                return Err(Error::RestoreTargetUnreachable {
                    requested: target.0,
                    earliest: from_lsn,
                });
            }
        }
        Ok(())
    }

    /// Read the log entries recovery needs, cut off at any restore target.
    fn read_log(&self, from_lsn: u64) -> Result<Vec<WalEntry>> {
        // Read once, from the earliest point any pass still needs.
        //
        // That point is the catalog's `through_lsn`, because the catalog and the
        // checkpoint entry are written by the same call: everything at or below
        // it is already in the pages and in the catalog. Below it the segments
        // are never opened, which is what makes opening a database cost what has
        // happened lately rather than everything that has ever happened.
        //
        // If the catalog is older than the last checkpoint — a crash between the
        // two — this reads slightly more than it needs to, which is the correct
        // direction to be wrong in.
        let mut entries = crate::wal::Wal::entries_from(&Self::wal_path(&self.dir), Lsn(from_lsn))?;
        // A point-in-time restore is recovery fed a deliberately short log
        // instead of an accidentally short one — everything below already
        // knows how to stop wherever the log stops.
        if let RecoverTarget::Lsn(target) = self.recover_target {
            entries.retain(|e| e.lsn.0 <= target.0);
        }
        Ok(entries)
    }

    /// Replay the definition entries logged after `from_lsn`.
    fn replay_definitions(&mut self, entries: &[WalEntry], from_lsn: u64) -> Result<()> {
        for e in entries {
            if e.lsn.0 <= from_lsn {
                continue;
            }
            match &e.op {
                WalOp::CreateCollection { name, schema } => {
                    let schema = decode_schema(schema)?;
                    self.register_collection(name, schema);
                }
                // Deliberately *not* applied here. An adoption moves records
                // that only exist once the log has been replayed, so it is
                // collected in order and executed in pass 4.
                WalOp::AdoptMigration { .. } => {}
                // Unlike an adoption this touches no record, so — unlike an
                // adoption — there is no reason to defer it: replaying it here,
                // in log order alongside the `CreateCollection` it follows, is
                // exactly what the original call did.
                WalOp::AlterSchemaInPlace { collection, schema } => {
                    let schema = decode_schema(schema)?;
                    self.apply_alter_schema_in_place(collection, schema)?;
                }
                WalOp::CreateIndex {
                    collection,
                    field,
                    kind,
                } => {
                    let def = (collection.clone(), field.clone(), kind.clone());
                    if !store_has(&self.index_defs, &def) {
                        self.index_defs.push(def);
                    }
                }
                WalOp::SetClusterField { collection, field } => {
                    if let Some(c) = self.collections.get_mut(collection.as_str()) {
                        c.cluster_name = field.clone();
                    }
                    // A declaration for a collection later dropped replays as
                    // nothing — same rule as every other per-collection op.
                }
                WalOp::DropIndex {
                    collection,
                    field,
                    kind,
                } => {
                    self.index_defs
                        .retain(|(c, f, k)| !(c == collection && f == field && k == kind));
                }
                WalOp::DropCollection { name } => {
                    if let Some(c) = self.collections.remove(name) {
                        self.by_id.remove(&c.id);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Replay the record writes logged after `flushed_lsn`.
    ///
    /// An autocommit frame (`TxnId(0)`) is applied as it is read. A frame
    /// tagged with a transaction is held back until that transaction's
    /// `Commit`, and dropped if the log ends first — which is the whole of
    /// what makes a transaction all-or-nothing under a crash, and equally
    /// what keeps a point-in-time target between a transaction's first frame
    /// and its commit from exposing part of it.
    ///
    /// No checkpoint can fall inside a transaction (both need exclusive access
    /// to the store, and a transaction's frames are logged in one call), so a
    /// committed transaction's frames are either all at or below
    /// `flushed_lsn`, and already in the pages, or all above it.
    fn replay_records(&mut self, entries: &[WalEntry], flushed_lsn: Lsn) -> Result<()> {
        let mut open: HashMap<TxnId, Vec<&WalEntry>> = HashMap::new();
        for e in entries {
            if e.lsn.0 <= flushed_lsn.0 {
                continue;
            }
            match (&e.op, e.txn) {
                (WalOp::Commit { txn }, _) => {
                    let frames = open.remove(txn).unwrap_or_default();
                    check_complete(*txn, &frames, e.lsn)?;
                    for frame in frames {
                        self.replay_record(&frame.op)?;
                    }
                }
                (_, TxnId(0)) => self.replay_record(&e.op)?,
                (_, txn) => open.entry(txn).or_default().push(e),
            }
        }
        // Whatever is still in `open` never reached its commit record: it was
        // cut off by a crash or by a restore target, and is discarded unseen.
        Ok(())
    }

    /// Apply one logged record write. Anything else is not a record write and
    /// was handled, or deliberately deferred, by another pass.
    fn replay_record(&mut self, op: &WalOp) -> Result<()> {
        match op {
            WalOp::Insert {
                collection,
                id,
                bytes,
            }
            | WalOp::Update {
                collection,
                id,
                bytes,
            } => {
                // Replay is idempotent by construction: an insert of an id
                // already present is applied as an overwrite, so it does not
                // matter how far the heap had got before the crash.
                self.apply_put(collection, *id, bytes)
            }
            WalOp::Delete { collection, id } => self.apply_delete(collection, *id).map(|_| ()),
            _ => Ok(()),
        }
    }

    /// Execute every committed migration adoption, in log order.
    fn replay_adoptions(&mut self, entries: &[WalEntry]) -> Result<()> {
        for e in entries {
            if let WalOp::AdoptMigration { target, source } = &e.op {
                self.adopt(target, source)?;
            }
        }
        Ok(())
    }

    /// Drop what is left of migrations that never reached their commit entry.
    fn discard_unfinished_migrations(&mut self) -> Result<()> {
        // Anything still wearing a migration name belongs to a change that was
        // cut short. Its target is untouched and correct, so this is waste, not
        // state — and leaving it would let the next attempt inherit a partial
        // copy of the collection it is trying to replace.
        for name in self
            .collections
            .keys()
            .filter(|n| is_migration_name(n))
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Some(c) = self.collections.remove(&name) {
                self.by_id.remove(&c.id);
                self.free_pages_of(&c)?;
            }
        }
        Ok(())
    }

    /// Seed the collection tables from the persisted catalog.
    ///
    /// Ids come from the file rather than from a counter, so a page written
    /// under `CollectionId(7)` is still read as belonging to that collection
    /// however much of the log has since been discarded.
    fn adopt_catalog(&mut self, cat: &crate::metadata::Catalog) -> Result<()> {
        for c in &cat.collections {
            let schema = decode_schema(&c.schema)?;
            self.register_collection_with_id(&c.name, schema, CollectionId(c.id));
            // The counter comes from the catalog rather than being left to
            // rebuild from whatever pass 3 replays: a record inserted, then
            // deleted, before the checkpoint must still push the sequence past
            // its id, and no surviving page says that it ever existed.
            if let Some(coll) = self.collections.get_mut(&c.name) {
                coll.next_record_id = coll.next_record_id.max(c.next_record_id);
            }
        }
        self.next_collection_id = cat.next_collection_id;
        self.delta_encoding = cat.delta_encoding;
        self.thread_per_core = cat.thread_per_core;
        self.index_defs = cat
            .indexes
            .iter()
            .map(|i| (i.collection.clone(), i.field.clone(), i.kind.clone()))
            .collect();
        Ok(())
    }

    /// Load the directory as of the last checkpoint. Returns whether it worked.
    fn load_directory(&mut self, flushed_lsn: Lsn) -> Result<bool> {
        // A database that has never checkpointed has nothing to load: the heap
        // may hold anything and only the log knows what.
        if flushed_lsn.0 == 0 {
            return Ok(false);
        }
        let stamp = self.directory_stamp(flushed_lsn);
        let Some(snapshot) = crate::directory::read(&self.dir, &stamp) else {
            return Ok(false);
        };
        for (cid, records) in snapshot.collections {
            // Records go back under the collection *id* their slots carry, which
            // is what a page scan would have done. A collection the log no
            // longer defines was dropped after the checkpoint; replay will not
            // resurrect it and neither should this.
            let Some(name) = self.by_id.get(&CollectionId(cid)).cloned() else {
                continue;
            };
            let Some(c) = self.collections.get_mut(&name) else {
                continue;
            };
            for (id, loc) in records {
                let mut chain = VersionChain::default();
                chain.push(TxnId(0), Some(loc));
                c.directory.insert(id, chain);
            }
        }
        for (page, free) in snapshot.free_space {
            self.fsm.set(page, free as usize);
        }
        Ok(true)
    }

    fn scan_pages(&mut self) -> Result<()> {
        let count = self.pool.page_count();
        for i in 0..count {
            let pid = PageId(i);
            let page = self.pool.get(pid)?;
            let free = page.free_space();
            let mut found: Vec<(CollectionId, RecordId, SlotId)> = Vec::new();
            for slot in page.slots() {
                let payload = page.get(slot)?;
                if payload.len() < SLOT_PREFIX {
                    return Err(Error::Corruption(format!(
                        "slot {} on page {i} is shorter than its prefix",
                        slot.0
                    )));
                }
                let cid = CollectionId(u32::from_le_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
                let mut rid = [0u8; 8];
                rid.copy_from_slice(&payload[4..12]);
                found.push((cid, RecordId(u64::from_le_bytes(rid)), slot));
            }
            for (cid, rid, slot) in found {
                // A record whose collection was dropped is an orphan; leave it
                // in place rather than pretending it belongs somewhere.
                if let Some(name) = self.by_id.get(&cid).cloned() {
                    if let Some(c) = self.collections.get_mut(&name) {
                        let mut chain = VersionChain::default();
                        chain.push(TxnId(0), Some(RecordLocation { page: pid, slot }));
                        c.directory.insert(rid, chain);
                    }
                }
            }
            self.fsm.set(pid, free);
        }
        Ok(())
    }
}

/// The flushed LSN named by the last checkpoint in `entries`, or zero.
fn last_checkpoint(entries: &[WalEntry]) -> Lsn {
    entries
        .iter()
        .rev()
        .find_map(|e| match e.op {
            WalOp::Checkpoint { flushed_lsn } => Some(flushed_lsn),
            _ => None,
        })
        .unwrap_or(Lsn(0))
}

/// Refuse to apply a transaction unless the log holds every one of its frames.
///
/// The writer uses the first frame's LSN as the transaction id and appends the
/// frames back to back, so a complete transaction is exactly the frames at
/// LSNs `txn..commit`. Anything else means frames vanished from the middle of
/// a log whose commit record survived — damage, not a crash, since a crash
/// only ever removes a suffix — and applying what is left would be precisely
/// the partial transaction this mechanism exists to rule out.
fn check_complete(txn: TxnId, frames: &[&WalEntry], commit: Lsn) -> Result<()> {
    let contiguous = frames
        .iter()
        .enumerate()
        .all(|(i, f)| f.lsn.0 == txn.0 + i as u64);
    if contiguous && txn.0 + frames.len() as u64 == commit.0 {
        return Ok(());
    }
    Err(Error::Corruption(format!(
        "transaction {} commits at lsn {} but the log does not hold the {} \
         contiguous frames before it (found {})",
        txn.0,
        commit.0,
        commit.0.saturating_sub(txn.0),
        frames.len()
    )))
}
