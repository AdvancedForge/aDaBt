//! Multi-version state: each record's chain of versions, and reads as of a
//! snapshot rather than as of now.

use adabt_core::error::Result;
use adabt_core::ids::{RecordId, TxnId};
use adabt_core::record::Record;
#[cfg(feature = "loom")]
use loom::sync::Arc;
#[cfg(not(feature = "loom"))]
use std::sync::Arc;

use super::HeapStore;
use crate::page::RecordLocation;
use crate::version::Snapshot;

/// Every version of one record, oldest first.
///
/// `None` marks a delete: a reader whose snapshot falls after it must see the
/// record as absent rather than seeing the version before it. Recording the
/// deletion as a version rather than removing the entry is what makes that
/// possible.
#[derive(Debug, Default, Clone)]
pub(super) struct VersionChain {
    pub(super) versions: Vec<(TxnId, Option<RecordLocation>)>,
}

impl VersionChain {
    pub(super) fn newest(&self) -> Option<RecordLocation> {
        self.versions.last().and_then(|(_, loc)| *loc)
    }

    /// The stamp of the newest version, whether it is a value or a tombstone.
    ///
    /// Never reclaimed away: the newest entry is always kept, so this is safe to
    /// call at any time and answers "when was this key last touched at all,"
    /// which is exactly what first-committer-wins conflict detection needs —
    /// distinct from `newest`, which answers "what does this key hold now" and
    /// says nothing once the newest entry is a delete.
    fn newest_stamp(&self) -> Option<TxnId> {
        self.versions.last().map(|(txn, _)| *txn)
    }

    /// The version a snapshot at `at` should see.
    fn visible_at(&self, at: TxnId) -> Option<RecordLocation> {
        self.versions
            .iter()
            .rev()
            .find(|(txn, _)| txn.0 <= at.0)
            .and_then(|(_, loc)| *loc)
    }

    pub(super) fn push(&mut self, txn: TxnId, loc: Option<RecordLocation>) {
        self.versions.push((txn, loc));
    }

    /// Drop versions no snapshot can reach, returning the freed locations.
    ///
    /// Keeps the newest version at or before the horizon: that one is still the
    /// answer for any reader at the horizon, and dropping it would make the
    /// record vanish.
    pub(super) fn reclaim_to(&mut self, horizon: TxnId) -> Vec<RecordLocation> {
        let keep_from = match self
            .versions
            .iter()
            .rposition(|(txn, _)| txn.0 <= horizon.0)
        {
            Some(i) => i,
            None => return Vec::new(),
        };
        let freed: Vec<RecordLocation> = self.versions[..keep_from]
            .iter()
            .filter_map(|(_, loc)| *loc)
            .collect();
        self.versions.drain(..keep_from);
        freed
    }

    pub(super) fn len(&self) -> usize {
        self.versions.len()
    }

    pub(super) fn is_absent(&self) -> bool {
        self.newest().is_none()
    }
}

impl HeapStore {
    /// Open a stable read view.
    ///
    /// Versions it might need are retained until it is dropped, which is what
    /// lets two representations be compared against identical state.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::open(Arc::clone(&self.versions))
    }

    /// When `id` was last touched — inserted, updated or deleted — whichever
    /// is newest.
    ///
    /// The building block first-committer-wins conflict detection is made of: a
    /// transaction whose write-set includes a key touched *after* its own
    /// snapshot was taken lost the race and must be told so, rather than
    /// silently overwriting a change it never saw.
    pub fn latest_write_ts(&self, collection: &str, id: RecordId) -> Result<Option<TxnId>> {
        Ok(self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|c| c.newest_stamp()))
    }

    /// The newest write — put or delete — committed to any record of
    /// `collection` since this store was opened, as its stamp and the id it
    /// wrote; `None` if there has been none.
    ///
    /// The collection-level counterpart of [`Self::latest_write_ts`]: a
    /// transaction that scanned `collection` validates the whole scan, rows it
    /// never saw included, by comparing this one stamp with its snapshot.
    /// Nothing before the open is reported, and nothing needs to be: no
    /// snapshot spans a restart.
    pub fn newest_write_in(&self, collection: &str) -> Result<Option<(TxnId, RecordId)>> {
        Ok(self.coll(collection)?.newest_write)
    }

    pub fn retained_versions(&self) -> usize {
        self.retained
    }

    pub fn open_snapshots(&self) -> usize {
        self.versions.open_count()
    }

    /// Read as of a snapshot rather than as of now.
    pub fn get_at(
        &mut self,
        collection: &str,
        id: RecordId,
        snapshot: &Snapshot,
    ) -> Result<Option<Record>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|c| c.visible_at(snapshot.at()))
        else {
            return Ok(None);
        };
        Ok(Some(self.read_at(collection, loc)?))
    }

    /// Scan as of a snapshot, in record-id order.
    pub fn scan_at(
        &mut self,
        collection: &str,
        snapshot: &Snapshot,
    ) -> Result<Vec<(RecordId, Record)>> {
        let locs: Vec<(RecordId, RecordLocation)> = self
            .coll(collection)?
            .directory
            .iter()
            .filter_map(|(id, chain)| chain.visible_at(snapshot.at()).map(|l| (*id, l)))
            .collect();
        let mut out = Vec::with_capacity(locs.len());
        for (id, loc) in locs {
            out.push((id, self.read_at(collection, loc)?));
        }
        Ok(out)
    }
}
