//! Making state durable and portable: checkpoints, the catalog and
//! directory snapshots they persist, backup and restore, and the state stamp
//! derived caches are validated against.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{Lsn, TxnId};
use adabt_core::store::LogicalStore;
use std::path::Path;

use super::HeapStore;
use crate::catalog::encode_schema;
use crate::wal::WalOp;

/// Recursively copy every file under `src` into `dest`, creating `dest` if it
/// does not exist. Used for backup, where the log is a directory of segments
/// rather than a single file.
fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

impl HeapStore {
    /// The catalog as it stands, for persisting at a checkpoint.
    fn catalog_snapshot(&self, through_lsn: u64) -> crate::metadata::Catalog {
        crate::metadata::Catalog {
            collections: self
                .collections
                .iter()
                .map(|(name, c)| crate::metadata::CollectionMeta {
                    name: name.clone(),
                    id: c.id.0,
                    next_record_id: c.next_record_id,
                    schema: encode_schema(c.codec.schema()),
                    cluster_field: c.cluster_name.clone(),
                })
                .collect(),
            indexes: self
                .index_defs
                .iter()
                .map(|(c, f, k)| crate::metadata::IndexMeta {
                    collection: c.clone(),
                    field: f.clone(),
                    kind: k.clone(),
                })
                .collect(),
            next_collection_id: self.next_collection_id,
            through_lsn,
            log_start_lsn: self.wal.start_lsn().0,
            delta_encoding: self.delta_encoding,
            thread_per_core: self.thread_per_core,
        }
    }

    /// The stamp a cached directory must match to be usable.
    ///
    /// Everything in it is available before a single page has been read, which
    /// is the constraint: this cache replaces a step of recovery, so it cannot
    /// be validated against anything recovery produces.
    pub(super) fn directory_stamp(&self, flushed_lsn: Lsn) -> crate::directory::Stamp {
        crate::directory::Stamp {
            identity: self.identity,
            checkpoint_lsn: flushed_lsn.0,
            heap_bytes: std::fs::metadata(Self::heap_path(&self.dir))
                .map(|m| m.len())
                .unwrap_or(0),
        }
    }

    /// The directory as it stands, in the form the cache stores.
    fn directory_snapshot(&self) -> crate::directory::Snapshot {
        crate::directory::Snapshot {
            collections: self
                .collections
                .values()
                .map(|c| {
                    let records = c
                        .directory
                        .iter()
                        .filter_map(|(id, chain)| chain.newest().map(|loc| (*id, loc)))
                        .collect();
                    (c.id.0, records)
                })
                .collect(),
            free_space: self
                .fsm
                .free
                .iter()
                .map(|(p, free)| (*p, *free as u32))
                .collect(),
        }
    }

    /// Flush every dirty page, fsync, and record the point replay may start from.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pool.checkpoint()?;
        // Ordering matters: the pages must be on disk before the log claims they
        // are, or a crash between the two would skip replaying live changes.
        let flushed = Lsn(self.wal.next_lsn().0.saturating_sub(1));
        self.wal.append(
            TxnId(0),
            WalOp::Checkpoint {
                flushed_lsn: flushed,
            },
        )?;
        self.wal.sync()?;
        // The directory describing the heap this checkpoint just left behind.
        // Written after the log entry, so its stamp names the checkpoint it
        // belongs to. A failure here is not a failure of the checkpoint — the
        // database is durable either way and a missing cache costs a scan — so
        // the stale file is removed rather than the error propagated.
        let flushed_lsn = flushed;
        let stamp = self.directory_stamp(flushed);
        if crate::directory::write(&self.dir, &stamp, &self.directory_snapshot()).is_err() {
            crate::directory::discard(&self.dir);
        }
        // The catalog, unlike the directory, is authoritative: a failure to
        // write it is propagated rather than swallowed. Discarding it and
        // carrying on would leave a database whose name-to-id binding exists
        // only in a log that is about to become discardable.
        let catalog = self.catalog_snapshot(flushed.0);
        crate::metadata::write(&self.dir, self.identity, &catalog)?;

        // Only now. Everything below this point is recorded in pages that have
        // been flushed and in a catalog that has been fsynced, so the segments
        // holding it are redundant. Discarding any earlier would remove the only
        // remaining record of which collection each page belongs to.
        self.wal.discard_below(flushed_lsn)?;
        // The catalog names the log's new lower bound, so it is rewritten once
        // the bound has actually moved. A catalog claiming a complete log over a
        // truncated one would let a later open silently rebuild from a log that
        // has lost its beginning.
        let catalog = self.catalog_snapshot(flushed.0);
        crate::metadata::write(&self.dir, self.identity, &catalog)?;
        Ok(())
    }

    /// Make `dest` a complete, independently openable copy of this database as
    /// of a fresh checkpoint.
    ///
    /// Only the files a restart itself depends on are copied — the heap, the
    /// log, the superblock, the catalog. `directory.adabt` and
    /// `derived.adabt` are not: they are caches (see their own modules), a
    /// restored database rebuilds them exactly as any other reopen would if
    /// they turned out to be missing or stale, and starting a restore from
    /// cache entries stamped against a checkpoint that may not even be
    /// `dest`'s most recent has no advantage over just not copying them.
    ///
    /// Checkpointing first is what makes the copy meaningful: without it,
    /// `dest` would need every log segment back to the last one, rather than
    /// only the ones this checkpoint could not yet fold into pages — and,
    /// since `checkpoint` discards (or archives) everything older, taking the
    /// checkpoint here rather than trusting the caller to have already is
    /// what keeps the two facts — "the log copied" and "the state the log
    /// starts from" — from being able to drift apart.
    ///
    /// Refuses if `dest` already exists and is non-empty, exactly as
    /// `restore_from` does — a backup target is not a thing this library
    /// unilaterally clears out from under a caller who typo'd a path;
    /// deleting whatever was already there is the caller's decision to make
    /// explicitly, not this function's to make for them.
    pub fn backup_to(&mut self, dest: &Path) -> Result<()> {
        self.checkpoint()?;
        std::fs::create_dir_all(dest)?;
        if std::fs::read_dir(dest)?.next().is_some() {
            return Err(Error::InvalidRestore(format!(
                "refusing to back up into non-empty directory {}",
                dest.display()
            )));
        }
        std::fs::copy(Self::heap_path(&self.dir), Self::heap_path(dest))?;
        copy_dir_all(&Self::wal_path(&self.dir), &Self::wal_path(dest))?;
        std::fs::copy(
            crate::superblock::path(&self.dir),
            crate::superblock::path(dest),
        )?;
        std::fs::copy(
            crate::metadata::path(&self.dir),
            crate::metadata::path(dest),
        )?;
        Ok(())
    }

    /// Reconstruct a database directory from a backup made by `backup_to`.
    ///
    /// `dest` must not already exist (or must be empty) — the same reasoning
    /// as `backup_to`'s refusal, in the direction where getting it wrong is
    /// worse: overwriting a live database's files with a backup's is exactly
    /// the kind of destructive action this library never takes without the
    /// caller asking for it by name. Once this returns, `dest` opens exactly
    /// as `src` would, including through `open_at` with a `RecoverTarget::Lsn`
    /// short of `src`'s own history — restoring copies bytes; it does not
    /// decide how much of them a later open replays.
    pub fn restore_from(src: &Path, dest: &Path) -> Result<()> {
        if !crate::superblock::path(src).exists() {
            return Err(Error::InvalidRestore(format!(
                "{} is not an aDaBt backup: no superblock present",
                src.display()
            )));
        }
        std::fs::create_dir_all(dest)?;
        if std::fs::read_dir(dest)?.next().is_some() {
            return Err(Error::InvalidRestore(format!(
                "refusing to restore into non-empty directory {}",
                dest.display()
            )));
        }
        copy_dir_all(src, dest)
    }

    /// Describe the current state of the primary, for the derived-representation
    /// cache to be validated against.
    ///
    /// Everything here is already in memory after recovery — the log position,
    /// the heap file's length, and the directory sizes — so taking a stamp is
    /// cheap enough to do on every checkpoint and every open.
    pub fn state_stamp(&mut self) -> Result<crate::derived::Stamp> {
        let mut counts: Vec<(String, u64)> = Vec::new();
        for name in self.store_collection_names() {
            counts.push((name.clone(), self.count(&name)? as u64));
        }
        counts.sort();
        Ok(crate::derived::Stamp {
            identity: self.identity,
            lsn: self.wal.next_lsn().0,
            heap_bytes: std::fs::metadata(Self::heap_path(&self.dir))
                .map(|m| m.len())
                .unwrap_or(0),
            counts,
        })
    }
}
