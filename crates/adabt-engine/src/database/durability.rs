//! Snapshots and point-in-time reads, checkpoints, backup and restore,
//! vacuum, version reclamation, the persisted derived-structure cache, and
//! buffer-pool and cache sizing.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_storage::heap::HeapStore;
use std::path::Path;

use super::Database;

pub(super) fn index_cache_key(collection: &str, field: &str, kind: IndexKind) -> String {
    format!("idx:{collection}:{field}:{}", kind.as_str())
}

/// Index entries as bytes: repeated `key, id-count, ids`.
///
/// Grouped by key rather than written as flat pairs, which matters more than it
/// looks: a low-cardinality index — a country, a status, a boolean — has a
/// handful of keys and a great many ids, and repeating the key per id would make
/// the cache larger than the records it was built from.
fn encode_index_entries(entries: &[(Value, Vec<RecordId>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (key, ids) in entries {
        adabt_storage::codec::encode_value(key, &mut out);
        out.extend_from_slice(&(ids.len() as u64).to_le_bytes());
        for id in ids {
            out.extend_from_slice(&id.0.to_le_bytes());
        }
    }
    out
}

/// Decode what [`encode_index_entries`] wrote, or `None` if it will not decode.
///
/// Every length is checked against what is left rather than trusted, and any
/// disagreement gives up on the whole blob. The caller rebuilds from the
/// primary, which is always available and always right.
pub(super) fn decode_index_entries(blob: &[u8]) -> Option<Vec<(Value, Vec<RecordId>)>> {
    fn u64_at(blob: &[u8], pos: &mut usize) -> Option<u64> {
        let end = pos.checked_add(8)?;
        let v = u64::from_le_bytes(blob.get(*pos..end)?.try_into().ok()?);
        *pos = end;
        Some(v)
    }
    let mut pos = 0usize;
    let count = u64_at(blob, &mut pos)? as usize;
    let mut out = Vec::new();
    for _ in 0..count {
        let (key, used) = adabt_storage::codec::decode_value(blob.get(pos..)?).ok()?;
        pos += used;
        let n = u64_at(blob, &mut pos)? as usize;
        let mut ids = Vec::new();
        for _ in 0..n {
            ids.push(RecordId(u64_at(blob, &mut pos)?));
        }
        out.push((key, ids));
    }
    if pos == blob.len() {
        Some(out)
    } else {
        None
    }
}

impl Database {
    /// Open a stable read view over the primary representation.
    ///
    /// Exposed on the database rather than only on the store because shadow
    /// execution is an engine-level activity: comparing two representations is
    /// meaningless unless both read the same state, and this is what makes that
    /// available to a caller.
    pub fn snapshot(&self) -> adabt_storage::version::Snapshot {
        self.store.snapshot()
    }

    pub fn get_at(
        &mut self,
        collection: &str,
        id: RecordId,
        snapshot: &adabt_storage::version::Snapshot,
    ) -> Result<Option<Record>> {
        self.store.get_at(collection, id, snapshot)
    }

    pub fn scan_at(
        &mut self,
        collection: &str,
        snapshot: &adabt_storage::version::Snapshot,
    ) -> Result<Vec<(RecordId, Record)>> {
        self.store.scan_at(collection, snapshot)
    }

    pub fn reclaim_versions(&mut self) -> Result<usize> {
        self.store.reclaim()
    }

    /// Save the derived representations so the next open need not rebuild them.
    ///
    /// Only worth doing at a checkpoint: the cache is validated against an exact
    /// description of the primary, so one written while writes are still
    /// arriving would be stale before it hit the disk.
    fn save_derived(&mut self) -> Result<()> {
        let mut blobs = Vec::new();
        for (collection, list) in &self.indexes {
            for idx in list {
                blobs.push((
                    index_cache_key(collection, idx.field(), idx.kind()),
                    encode_index_entries(&idx.snapshot()),
                ));
            }
        }
        let stamp = self.store.state_stamp()?;
        adabt_storage::derived::write(self.store.dir(), &stamp, &blobs)
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        self.store.checkpoint()?;
        // Ordering matters: the stamp is taken after the checkpoint entry is in
        // the log, so it describes the state the cache was actually built from.
        // A failure here is not a failure of the checkpoint — the database is
        // durable either way, and all a missing cache costs is a rebuild — so it
        // is discarded rather than propagated.
        if self.save_derived().is_err() {
            adabt_storage::derived::discard(self.store.dir());
        }
        Ok(())
    }

    /// Make `dest` a complete, independently openable copy of this database.
    ///
    /// `HeapStore::backup_to` already copies everything a restart depends on;
    /// this adds the one file it does not know exists — the unique-constraint
    /// sidecar, which lives at this layer (see `crate::unique`) because
    /// whether a field is constrained is a logical decision, not a physical
    /// one. Everything else this engine caches — indexes, materialized views,
    /// the derived-representation stamp — is exactly as reconstructible from
    /// `dest` as it is from any other reopened directory, so none of it needs
    /// copying either.
    pub fn backup_to(&mut self, dest: &Path) -> Result<()> {
        self.store.backup_to(dest)?;
        if !self.unique_constraints.is_empty() {
            crate::unique::write(dest, &self.unique_constraints)?;
        }
        Ok(())
    }

    /// Reconstruct a database directory from a backup made by `backup_to`.
    ///
    /// The counterpart `backup_to` never had: an application could back up
    /// through `Database` but had to drop to `HeapStore` to restore, which
    /// is an asymmetry nobody would guess at. `HeapStore::restore_from`
    /// copies the whole directory, so the engine-level sidecar
    /// (`unique.adabt`) that `backup_to` writes comes back with it.
    pub fn restore_from(src: &Path, dest: &Path) -> Result<()> {
        HeapStore::restore_from(src, dest)
    }

    /// The lsn to restore to for a given wall-clock moment, as nanoseconds
    /// since the Unix epoch.
    ///
    /// This is what turns "restore to 14:32" into something `open_at` can
    /// act on. Without it that translation lived only in `adabt-storage`,
    /// so PITR's headline use case was unreachable from the engine API even
    /// after `open_at` and `set_log_archive` were exposed — two thirds of a
    /// feature.
    ///
    /// `None` means every entry in the log is already after `nanos`: there
    /// is no prefix of this log ending at or before that moment.
    pub fn lsn_at_or_before(dir: &Path, nanos: u64) -> Result<Option<adabt_core::ids::Lsn>> {
        adabt_storage::wal::Wal::lsn_at_or_before(&HeapStore::wal_path(dir), nanos)
    }

    /// Relocate live records off trailing pages and shrink the heap file.
    ///
    /// Returns pages reclaimed. Deleting records returns their slots to the
    /// free-space map but never shrinks the file; this is the operation that
    /// actually gives disk back, and it was reachable only from the storage
    /// crate.
    pub fn vacuum(&mut self) -> Result<u32> {
        let freed = self.store.vacuum()?;
        // Vacuum moves records between pages, so anything addressing them by
        // physical position is no longer valid. Same invalidation a schema
        // change needs, for the same reason.
        self.direct.clear();
        self.plan_cache.clear();
        self.compiled.clear();
        Ok(freed)
    }

    /// Resize the result cache, in entries. Zero disables it.
    ///
    /// Operator-facing for the same reason `set_pool_capacity` is: memoizing
    /// whole result sets is a large win on repeated identical queries and a
    /// pure loss on a workload that never repeats one, and only the operator
    /// knows which they have. It was reachable only through an `Action`, which
    /// means only through the optimizer.
    pub fn set_result_cache_entries(&mut self, n: usize) {
        self.result_cache.set_capacity(n);
    }

    /// Resize the plan cache, in entries. Zero disables it.
    pub fn set_plan_cache_entries(&mut self, n: usize) {
        self.plan_cache.set_capacity(n);
    }

    /// Resize the buffer pool, in 4 KiB pages.
    ///
    /// `Database::open` fixes this at 1024 pages — 4 MiB — regardless of how
    /// much data the database holds, and `HeapStore::set_pool_capacity` was
    /// reachable from nowhere above the storage crate. The scale benchmark
    /// showed why that matters: at 1.6M rows the heap is far larger
    /// than 4 MiB, so nearly every fetch is a cold page read and both scans
    /// and indexed point lookups degrade with collection size.
    pub fn set_pool_capacity(&mut self, pages: usize) -> Result<()> {
        self.store.set_pool_capacity(pages)
    }

    /// Send discarded log segments to `dir` instead of deleting them.
    ///
    /// **Without this, point-in-time recovery cannot reach anything but a
    /// backup's own checkpoint.** `backup_to` checkpoints before copying, and
    /// a checkpoint discards every segment it has folded into pages — so a
    /// backup taken on its own carries no history before its own checkpoint,
    /// and `open_at` correctly refuses (`RestoreTargetUnreachable`) any
    /// earlier target rather than answering wrongly. Archiving is what keeps
    /// those segments, and it is therefore the difference between "restore to
    /// exactly when I backed up" and real PITR.
    ///
    /// The mechanism lived in `adabt-storage` long before anything above it
    /// could reach it, which made "point-in-time recovery" true of the
    /// storage layer and not of anything an application could call. This
    /// method is the fix.
    pub fn set_log_archive(&mut self, dir: Option<std::path::PathBuf>) {
        self.store.set_log_archive(dir);
    }
}
