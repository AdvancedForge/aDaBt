//! Rebuilding in-memory state on open: adopt the persisted catalog, load the
//! page directory a checkpoint recorded, then replay the log over it.
//!
//! Crash recovery and a point-in-time restore run the same passes and differ
//! in one input: the heap state replay starts from. Crash recovery starts from
//! the newest directory snapshot and replays to the end of the log. A restore
//! starts from the heap's base image and replays only to its target, which is
//! sound only because a base image holds nothing after its LSN — see
//! [`crate::base_image`] for why a heap the buffer pool has written to since
//! cannot be restored from, and `restore_base` for what is checked before a
//! restore replays anything.
//!
//! # Never from the pages alone
//!
//! The directory is never rebuilt by reading the pages: a heap file holds
//! slots no reader may see again, and a page scan cannot tell them from live
//! records (see [`crate::directory`]). Recovery starts from one of exactly two
//! states it can read faithfully:
//!
//! - a directory snapshot, which names the newest version's location of every
//!   record as of its checkpoint, whatever else the heap file holds; or
//! - an empty heap, when the database has never completed a checkpoint and the
//!   log still holds every write from the first, so replay alone rebuilds it.
//!
//! Anything else — a log that no longer reaches back to the start, and no
//! directory snapshot it does reach back to — is refused rather than guessed
//! at, as a missing catalog is.
//!
//! # Why a directory snapshot is enough, whatever happened to the heap file
//!
//! Between the checkpoint at `F` that wrote the snapshot and the crash, the
//! buffer pool may have written any page to the heap file, allocated new
//! pages, and freed and reused slots. Replaying `F+1..` over the snapshot is
//! still exact, because of one invariant the write paths keep:
//!
//! > **Every change to where a record's newest version lives is logged before
//! > any page carrying it can reach the heap file.**
//!
//! Inserts, updates, deletes, transactions, schema migrations and vacuum's
//! moves all go through the log (vacuum's as an `Update` of the record's own
//! bytes). So every record falls in one of two cases:
//!
//! - **The log after `F` never mentions it.** Its slot was not freed after
//!   `F` — freeing a newest version takes a later version, which is logged —
//!   so the slot the snapshot names still holds it, on whatever page image
//!   reached the file.
//! - **The log after `F` writes it.** Replay rewrites it, and its last
//!   replayed write decides what it is: a put places the record afresh, a
//!   delete leaves it absent. The slot the snapshot names is untrustworthy —
//!   it may still hold the old version, or have been freed, or reused by
//!   another record — so before the first replayed write the loaded version
//!   is dropped and its slot freed only if it still holds this record
//!   (`retire_recovered`).
//!
//! What the pages hold besides — stolen copies of writes that replay places
//! again, versions a reader was holding at the checkpoint — is never named by
//! any version, and is never read. `vacuum` frees it. Pages allocated after
//! `F` hold nothing else, so recovery truncates the file back to the length
//! the snapshot recorded before it replays.
//!
//! Recovery writes nothing to the log, so a second crash, at any point, is
//! recovered from the same snapshot and the same log, and reaches the same
//! state.
//!
//! # A log that ends below a checkpoint
//!
//! Three durable files say how far through the log the heap already reflects:
//! the catalog, the directory snapshot and the base-image marker. Call the
//! newest LSN any of them names `C`. Recovery treats every entry at or below
//! it as folded into the pages, so **the log must never hand out an LSN at or
//! below `C`**: a write given one is skipped by the next recovery, and is lost
//! without an error. The log chooses its next LSN from its own last entry, so
//! that holds only while the log reaches `C`.
//!
//! *When the log can end below `C`.* Not after a crash. A checkpoint flushing
//! through `C` syncs the log through its own entry, at `C + 1`, before it
//! writes any of the three files, and a crash only ever takes the unsynced
//! tail of the log, which lies above that. A log ending below `C` was
//! damaged after it was written: a segment lost or cut short, a device that
//! dropped writes it had acknowledged, a backup copied without the whole of
//! its log.
//!
//! *What the damage can have taken*, if the log now ends at `E < C`:
//!
//! - **Entries in `E+1..=C`.** Replay never applies a record write at or
//!   below the snapshot's LSN, and never a definition at or below the
//!   catalog's. When the catalog reaches `C` these entries were never going
//!   to be read: losing them changes nothing. When it does not — a crash
//!   between a checkpoint's directory write and its catalog write leaves it
//!   one checkpoint behind — the definitions logged between the catalog and
//!   `C` existed nowhere else, and the snapshot files records under
//!   collection ids nothing defines any more. That is refused with
//!   `Corruption` before anything is written (`check_log_reaches`).
//! - **Entries above `C`.** Writes made after the checkpoint. Nothing records
//!   how far the log reached, so they are lost as any lost tail of the log
//!   is, and the database opens as the checkpoint left it — the outcome
//!   recovery already gives a log cut anywhere above `C`.
//!
//! So when the catalog reaches `C`, recovery proceeds to exactly the
//! checkpoint's state, and then restarts the log at `C + 1`
//! (`resume_log_after`, [`crate::wal::Wal::restart_at`]). Not later: a restore
//! from a base image at `C` demands the log after it be contiguous, and the
//! one LSN reissued, `C + 1`, belonged to the checkpoint's own entry, which
//! replay never acts on. What the log still held is set aside under
//! `superseded-log/`, all of it below `C + 1`, so the log no longer claims to
//! reach back to the first write ever made. The derived-structure cache is
//! discarded, since it is stamped with the next LSN. Nothing on disk changes
//! format: the restarted log is an ordinary segment that begins at `C + 1`,
//! exactly as one does after a checkpoint has discarded those before it.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{CollectionId, Lsn, RecordId, TxnId};
use std::collections::HashMap;

use super::definitions::store_has;
use super::migration::is_migration_name;
use super::mvcc::RECOVERED;
use super::reclaim::Release;
use super::{HeapStore, RecoverTarget, VersionChain};
use crate::catalog::decode_schema;
use crate::directory::{Snapshot, Stamp};
use crate::page::PAGE_SIZE;
use crate::wal::{WalEntry, WalOp};

/// The heap state replay starts from.
enum Base {
    /// The directory snapshot a checkpoint wrote, and the stamp it was
    /// written under: replay starts after `stamp.checkpoint_lsn`.
    Snapshot(Stamp, Snapshot),
    /// Nothing: the log holds every write ever made, and replays from the
    /// first over an empty heap.
    Empty,
}

/// A checkpoint a durable file records, and which file records it.
#[derive(Debug, Clone, Copy)]
struct Checkpoint {
    /// The LSN through which the heap reflects the log.
    lsn: u64,
    recorded_by: &'static str,
}

impl Checkpoint {
    fn or_newer(self, lsn: u64, recorded_by: &'static str) -> Self {
        if lsn > self.lsn {
            Checkpoint { lsn, recorded_by }
        } else {
            self
        }
    }
}

impl HeapStore {
    pub(super) fn recover(&mut self) -> Result<()> {
        // Pass 0: the persisted catalog, if there is one.
        let from_lsn = self.adopt_persisted_catalog()?;
        self.check_restore_target(from_lsn)?;
        let entries = self.read_log(from_lsn)?;
        let entries = &entries[..];

        // The heap state replay starts from. Crash recovery takes the newest
        // directory snapshot: replay to the end of the log carries it forward
        // whatever the heap file went through since. A restore stops short of
        // the end, so it starts from a heap holding exactly the state at a
        // known checkpoint, and nothing later.
        //
        // The snapshot, not the log's last checkpoint entry, decides where
        // replay starts. In a live directory the two agree, or the snapshot is
        // older — a crash between a checkpoint's log entry and its snapshot —
        // and replaying from it is still exact. In a restored backup handed
        // later log they do not: that log records the source's later
        // checkpoints, whose pages this heap file never received.
        let base = match self.recover_target {
            RecoverTarget::Latest => self.latest_base(from_lsn)?,
            RecoverTarget::Lsn(target) => self.restore_base(entries, from_lsn, target)?,
        };
        let checkpoint = self.newest_checkpoint(from_lsn, &base);
        self.check_log_reaches(from_lsn, checkpoint)?;
        // Every check has passed: from here on, recovery writes.
        if let RecoverTarget::Lsn(target) = self.recover_target {
            self.end_log_at(target)?;
        }

        // Pass 1: collection definitions, for whatever the catalog did not
        // already cover. The snapshot's records are filed under collection
        // ids, which only mean something once the definitions are known.
        self.replay_definitions(entries, from_lsn)?;

        // Pass 2: the page directory and free-space map, as of the base.
        let replay_after = self.load_base(base)?;

        // Pass 3: replay everything the base does not contain.
        self.replay_records(entries, replay_after)?;

        // Pass 4: schema migrations that reached their commit entry, in log
        // order. Deferred to here because an adoption hands over records, and
        // the records only exist once passes 2 and 3 have placed them.
        self.replay_adoptions(entries)?;
        self.discard_unfinished_migrations()?;
        self.check_recovered_locations()?;

        // Replay appends a version onto chains the snapshot already built. No
        // snapshot can span a restart, so collapsing them is both safe and
        // necessary: otherwise every restart doubles the retained history.
        self.reclaim()?;
        self.retained = 0;

        // Last, once the state is rebuilt: never hand out an LSN the
        // checkpoint already covers.
        self.resume_log_after(checkpoint)
    }

    /// The newest checkpoint a durable file in this directory records: the
    /// LSN through which the catalog, the directory snapshot recovery starts
    /// from, or the base-image marker says the heap already reflects, and the
    /// file that says so.
    ///
    /// The snapshot is the one `base` carries. Crash recovery takes the newest
    /// there is, and a restore takes one that must match the newest exactly,
    /// so either way no snapshot on disk is newer.
    fn newest_checkpoint(&self, catalog_lsn: u64, base: &Base) -> Checkpoint {
        let mut newest = Checkpoint {
            lsn: catalog_lsn,
            recorded_by: "catalog",
        };
        if let Base::Snapshot(stamp, _) = base {
            newest = newest.or_newer(stamp.checkpoint_lsn, "page directory");
        }
        if let Some(marker) = crate::base_image::read(&self.dir, self.identity) {
            newest = newest.or_newer(marker.0, "base-image marker");
        }
        newest
    }

    /// Refuse a log that ends below `checkpoint` when it took with it
    /// something recovery cannot do without.
    ///
    /// Recovery replays record writes only above the snapshot's LSN, and
    /// definitions only above the catalog's. What the log lost at or below
    /// `checkpoint` is therefore harmless exactly when the catalog reaches
    /// `checkpoint` too. When it does not, the definitions logged between the
    /// two are gone, and the snapshot files records under collection ids that
    /// nothing defines any more: recovery would drop them as belonging to a
    /// dropped collection, without an error.
    fn check_log_reaches(&self, catalog_lsn: u64, checkpoint: Checkpoint) -> Result<()> {
        let log_end = self.wal.next_lsn().0.saturating_sub(1);
        if log_end >= checkpoint.lsn || catalog_lsn >= checkpoint.lsn {
            return Ok(());
        }
        let catalog = if catalog_lsn == 0 {
            "there is no catalog to say what was defined before it".to_string()
        } else {
            format!("the catalog reflects only lsn {catalog_lsn}")
        };
        Err(Error::Corruption(format!(
            "the log in {} ends at lsn {log_end}, below the checkpoint at lsn {} that the {} \
             records, and {catalog}. A checkpoint syncs the log before it records anything, \
             so the log was damaged after it was written, and the collection definitions \
             logged up to lsn {} were lost with it. Recover from a backup",
            Self::wal_path(&self.dir).display(),
            checkpoint.lsn,
            checkpoint.recorded_by,
            checkpoint.lsn
        )))
    }

    /// Restart the log just after `checkpoint` if it now ends at or below it,
    /// setting what it still holds aside.
    ///
    /// The derived-structure cache goes first: it is stamped with the next
    /// LSN, and one written before the damage may carry a stamp the restarted
    /// log reaches again over a different history.
    fn resume_log_after(&mut self, checkpoint: Checkpoint) -> Result<()> {
        if self.wal.next_lsn().0 > checkpoint.lsn {
            return Ok(());
        }
        crate::derived::discard(&self.dir);
        let aside = self.superseded_log_dir(&format!("below-{:016}", checkpoint.lsn + 1));
        self.wal.restart_at(Lsn(checkpoint.lsn + 1), &aside)?;
        crate::sync_dir(&self.dir)
    }

    /// A fresh directory under `superseded-log/` for log segments recovery
    /// takes out of the log, named by `label`.
    fn superseded_log_dir(&self, label: &str) -> std::path::PathBuf {
        let parent = self.dir.join("superseded-log");
        let mut aside = parent.join(label);
        let mut n = 1;
        while aside.exists() {
            n += 1;
            aside = parent.join(format!("{label}-{n}"));
        }
        aside
    }

    /// The base crash recovery starts from.
    fn latest_base(&self, from_lsn: u64) -> Result<Base> {
        match crate::directory::read_latest(&self.dir, self.identity) {
            // A snapshot older than the catalog predates definitions pass 1
            // will not replay, so it cannot be brought forward. Every
            // checkpoint writes its snapshot before its catalog, so this
            // happens only to a damaged or foreign file.
            Some((stamp, snapshot)) if stamp.checkpoint_lsn >= from_lsn => {
                let start = self.wal.start_lsn().0;
                if start > stamp.checkpoint_lsn + 1 {
                    return Err(Error::Corruption(format!(
                        "the page directory in {} was recorded at lsn {}, but the log \
                         now begins at lsn {start}, so the writes in between cannot be \
                         replayed. Recover from a backup",
                        self.dir.display(),
                        stamp.checkpoint_lsn
                    )));
                }
                Ok(Base::Snapshot(stamp, snapshot))
            }
            // Never checkpointed: no catalog, and `adopt_persisted_catalog`
            // has established that the log is complete.
            _ if from_lsn == 0 => Ok(Base::Empty),
            _ => Err(Error::Corruption(format!(
                "the page directory in {} is missing, damaged or from another database. \
                 The database has been checkpointed, so its log cannot rebuild it alone, \
                 and the heap file cannot be read without the directory: a page cannot \
                 tell a live record from a deleted or superseded one. Restore the \
                 directory file or recover from a backup",
                self.dir.display()
            ))),
        }
    }

    /// Install `base` as the starting state: the page directory and free-space
    /// map, and a heap file holding nothing the base does not account for.
    /// Returns the LSN replay starts after.
    fn load_base(&mut self, base: Base) -> Result<Lsn> {
        let (stamp, snapshot) = match base {
            Base::Empty => {
                // Whatever the pool wrote here is replayed again from the log.
                self.pool.truncate_to(0)?;
                return Ok(Lsn(0));
            }
            Base::Snapshot(stamp, snapshot) => (stamp, snapshot),
        };
        // Pages past the length the checkpoint left were allocated after it,
        // and hold only writes that replay places again.
        self.pool
            .truncate_to((stamp.heap_bytes / PAGE_SIZE as u64) as u32)?;
        let pages = self.pool.page_count();
        for (cid, records) in snapshot.collections {
            // A collection the log no longer defines was dropped after the
            // checkpoint; replay will not resurrect it and neither should this.
            let Some(name) = self.by_id.get(&CollectionId(cid)).cloned() else {
                continue;
            };
            let Some(c) = self.collections.get_mut(&name) else {
                continue;
            };
            for (id, loc) in records {
                c.directory.insert(id, VersionChain::recovered(loc));
            }
        }
        // A page vacuum truncated after the checkpoint is gone, and must not
        // be offered for placement.
        for (page, free) in snapshot.free_space {
            if page.0 < pages {
                self.fsm.set(page, free as usize);
            }
        }
        Ok(Lsn(stamp.checkpoint_lsn))
    }

    /// Drop the version recovery loaded for `id`, if that is still all its
    /// chain holds, ahead of the first replayed write to it.
    ///
    /// The log says the record changed after the checkpoint, so the slot the
    /// snapshot names may no longer be its own. A freed slot can have been
    /// reused by another record, by a stolen write or by replay itself, and
    /// keeping the loaded version until reclamation would then free that
    /// record's slot instead. So it goes now, before replay places anything
    /// for this record, and its slot is freed only if it still holds this
    /// record — in which case it holds nothing replay will keep. Afterwards
    /// the replayed writes decide what the record is, as they would for one
    /// the snapshot had never heard of.
    fn retire_recovered(&mut self, collection: &str, id: RecordId) -> Result<()> {
        let Some(c) = self.collections.get_mut(collection) else {
            return Ok(());
        };
        let Some(loc) = c.directory.get(&id).and_then(VersionChain::only_recovered) else {
            return Ok(());
        };
        c.directory.remove(&id);
        let release = Release::of(c.id, id, RECOVERED, loc);
        self.release(vec![release]).map(|_| ())
    }

    /// Refuse a recovered directory that names a page the heap file does not
    /// have.
    ///
    /// Every loaded version still standing is one the log never touched after
    /// the checkpoint, and the invariant in the module documentation says its
    /// slot is intact. A page missing from under it means that invariant was
    /// broken somewhere, and reading on would return an error per record at
    /// best.
    fn check_recovered_locations(&self) -> Result<()> {
        let pages = self.pool.page_count();
        for (name, c) in &self.collections {
            for (id, chain) in &c.directory {
                if let Some(loc) = chain.only_recovered() {
                    if loc.page.0 >= pages {
                        return Err(Error::Corruption(format!(
                            "record {} of {name} is recorded on page {}, but the heap file \
                             has {pages} pages",
                            id.0, loc.page.0
                        )));
                    }
                }
            }
        }
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

    /// The base image a restore to `target` starts from, once it is shown to
    /// be one this log can bring to `target`.
    ///
    /// Four things must hold, and each is checked here, before recovery has
    /// written anything:
    ///
    /// 1. The heap file is a base image ([`crate::base_image`]) at least as
    ///    new as the catalog. Without that, pages written after `target` may
    ///    be in the heap file, and replay cannot take them back out.
    /// 2. The base image's directory snapshot is there, describing exactly
    ///    this heap file. Without it the heap file cannot be read faithfully
    ///    (see `crate::directory`).
    /// 3. The base image is at or before `target`. Replay only runs forward.
    /// 4. The log holds every entry from the base image to `target`. Replay
    ///    skips nothing it is not given, so a missing segment would otherwise
    ///    produce a state that never existed, under the target's name.
    fn restore_base(&self, entries: &[WalEntry], from_lsn: u64, target: Lsn) -> Result<Base> {
        let Some(base) = self.base_image(from_lsn) else {
            return Err(Error::InvalidRestore(format!(
                "cannot restore {} to lsn {}: its heap file is not a base image. Pages \
                 written since its last checkpoint may already be in it, and replaying \
                 the log cannot take later writes back out. Restore a backup taken at or \
                 before the target (`restore_from`), add the archived log segments that \
                 cover the target to its wal directory, and open that directory at the \
                 target instead",
                self.dir.display(),
                target.0
            )));
        };
        let stamp = self.directory_stamp(base);
        let Some(snapshot) = crate::directory::read(&self.dir, &stamp) else {
            return Err(self.no_directory_snapshot(target, base));
        };
        if target < base {
            return Err(Error::RestoreTargetUnreachable {
                requested: target.0,
                earliest: base.0,
            });
        }
        // `entries` is already cut at `target`, so the log reaches it exactly
        // when the entries after the base run on from it without a gap and end
        // at it.
        let mut reached = base.0;
        for e in entries.iter().filter(|e| e.lsn > base) {
            if e.lsn.0 != reached + 1 {
                return Err(Error::InvalidRestore(format!(
                    "cannot restore {} to lsn {}: the log is missing lsns {}..={} after the \
                     base image at lsn {}. Add the log segments that hold them — from the \
                     log archive, or the source database's wal directory — and try again",
                    self.dir.display(),
                    target.0,
                    reached + 1,
                    e.lsn.0 - 1,
                    base.0
                )));
            }
            reached = e.lsn.0;
        }
        if reached < target.0 {
            return Err(Error::InvalidRestore(format!(
                "cannot restore {} to lsn {}: the log ends at lsn {}. Add the log segments \
                 that reach the target — from the log archive, or the source database's \
                 wal directory — and try again",
                self.dir.display(),
                target.0,
                reached
            )));
        }
        Ok(Base::Snapshot(stamp, snapshot))
    }

    /// The LSN this directory's heap file is a base image as of, if it is one
    /// a restore can start from: at least as new as the catalog it adopted,
    /// since replay never revisits what the catalog already reflects.
    fn base_image(&self, from_lsn: u64) -> Option<Lsn> {
        crate::base_image::read(&self.dir, self.identity).filter(|base| base.0 >= from_lsn)
    }

    /// The refusal for a base image whose directory snapshot is missing.
    fn no_directory_snapshot(&self, target: Lsn, base: Lsn) -> Error {
        Error::InvalidRestore(format!(
            "cannot restore {} to lsn {}: the page directory recorded with its base image \
             at lsn {} is missing or damaged, and the heap file cannot be read faithfully \
             without it. Restore the backup again",
            self.dir.display(),
            target.0,
            base.0
        ))
    }

    /// Make `target` the end of this directory's log, setting aside what
    /// follows it.
    ///
    /// The entries after a restore target are a history the restored database
    /// did not have. Left in place, the next ordinary open would replay them,
    /// and the database's own next writes would be interleaved with them. They
    /// go to a fresh directory under `superseded-log/`, never deleted.
    ///
    /// The derived-structure cache may describe a checkpoint among those
    /// set-aside entries; it goes too, and rebuilds as after any other loss.
    /// The directory snapshot stays: `restore_base` has just established that
    /// it describes the base image, which precedes the target.
    fn end_log_at(&mut self, target: Lsn) -> Result<()> {
        let aside = self.superseded_log_dir(&format!("after-{:016}", target.0));
        if self.wal.rewind_to(target, &aside)? {
            crate::sync_dir(&self.dir)?;
            crate::derived::discard(&self.dir);
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

    /// Replay the record writes logged after `flushed_lsn`, the LSN the base
    /// reflects.
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
                self.retire_recovered(collection, *id)?;
                self.apply_put(collection, *id, bytes)
            }
            WalOp::Delete { collection, id } => {
                self.retire_recovered(collection, *id)?;
                self.apply_delete(collection, *id).map(|_| ())
            }
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
