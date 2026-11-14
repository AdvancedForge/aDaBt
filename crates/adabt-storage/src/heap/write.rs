//! The physical write path: building slot payloads, recording versions in
//! the directory, and logging each operation before it is applied.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{CollectionId, RecordId, TxnId};
use adabt_core::record::Record;
use adabt_core::store::normalize_for_storage;

use super::{HeapStore, MAX_RECORD_BYTES, SLOT_PREFIX};
use crate::page::MAX_PAYLOAD;
use crate::wal::WalOp;

impl HeapStore {
    fn build_payload(cid: CollectionId, id: RecordId, bytes: &[u8], compress: bool) -> Vec<u8> {
        let (encoding, body) = if compress {
            crate::compress::maybe_compress(bytes)
        } else {
            (crate::compress::Encoding::Raw, bytes.to_vec())
        };
        let mut payload = Vec::with_capacity(SLOT_PREFIX + body.len());
        payload.extend_from_slice(&cid.0.to_le_bytes());
        payload.extend_from_slice(&id.0.to_le_bytes());
        payload.push(encoding.bit());
        payload.extend_from_slice(&body);
        payload
    }

    /// Insert or overwrite. The single physical write path, so insert, update
    /// and replay cannot drift apart.
    /// Write a new version. Never updates in place: a superseded version may
    /// still be the answer for an open snapshot, and overwriting it would move
    /// the ground under a reader mid-scan.
    pub(super) fn apply_put(&mut self, collection: &str, id: RecordId, bytes: &[u8]) -> Result<()> {
        let Some(c) = self.collections.get(collection) else {
            // Replay of an operation against a collection later dropped.
            return Ok(());
        };
        let cid = c.id;
        let payload = Self::build_payload(cid, id, bytes, self.compress);
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Corruption(format!(
                "record of {} bytes exceeds the {MAX_RECORD_BYTES}-byte page limit",
                bytes.len()
            )));
        }

        let txn = self.versions.begin_write();
        let loc = self.place(&payload)?;
        let c = self.collections.get_mut(collection).expect("checked above");
        let chain = c.directory.entry(id).or_default();
        let superseded = !chain.versions.is_empty();
        chain.push(txn, Some(loc));
        // Every write through here — manual or auto-allocated, live or
        // replayed — pushes the sequence past whatever id it just used. This is
        // the one physical write path, so there is nowhere else an id could
        // enter the collection without this running.
        c.next_record_id = c.next_record_id.max(id.0 + 1);
        if superseded {
            self.retained += 1;
        }
        self.maybe_reclaim()?;
        Ok(())
    }

    /// Insert or overwrite with a clustering key: same durability and
    /// directory work as [`Self::apply_put`], placement chosen for locality.
    pub(super) fn apply_put_keyed(
        &mut self,
        collection: &str,
        id: RecordId,
        bytes: &[u8],
        key: i64,
    ) -> Result<()> {
        let Some(c) = self.collections.get(collection) else {
            return Ok(());
        };
        let cid = c.id;
        let payload = Self::build_payload(cid, id, bytes, self.compress);
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Corruption(format!(
                "record of {} bytes exceeds the {MAX_RECORD_BYTES}-byte page limit",
                bytes.len()
            )));
        }

        let txn = self.versions.begin_write();
        let loc = self.place_keyed(collection, &payload, key)?;
        let c = self.collections.get_mut(collection).expect("checked above");
        let chain = c.directory.entry(id).or_default();
        let superseded = !chain.versions.is_empty();
        chain.push(txn, Some(loc));
        c.next_record_id = c.next_record_id.max(id.0 + 1);
        if superseded {
            self.retained += 1;
        }
        self.maybe_reclaim()?;
        Ok(())
    }

    pub(super) fn apply_delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let txn = self.versions.begin_write();
        let Some(c) = self.collections.get_mut(collection) else {
            return Ok(false);
        };
        let Some(chain) = c.directory.get_mut(&id) else {
            return Ok(false);
        };
        if chain.is_absent() {
            return Ok(false);
        }
        // A tombstone rather than a removal: a reader whose snapshot predates
        // the delete must still find the record.
        chain.push(txn, None);
        self.retained += 1;
        self.maybe_reclaim()?;
        Ok(true)
    }

    pub(super) fn log(&mut self, op: WalOp) -> Result<()> {
        if self.replaying {
            return Ok(());
        }
        self.wal.append(TxnId(0), op)?;
        self.wal.commit()
    }

    /// Append without making it durable yet. Paired with a single
    /// [`Wal::commit`](crate::wal::Wal::commit) after every entry in a batch,
    /// so `Strict` pays for one fsync instead of one per row.
    fn log_uncommitted(&mut self, op: WalOp) -> Result<()> {
        if self.replaying {
            return Ok(());
        }
        self.wal.append(TxnId(0), op)?;
        Ok(())
    }

    /// Insert many records into `collection` in one call.
    ///
    /// **All-or-nothing, and one fsync rather than one per row.** Every record
    /// is normalised and validated, and every id checked against both the
    /// existing directory and the rest of the batch, before anything is
    /// written — a batch containing one bad record inserts nothing, exactly as
    /// calling `insert` in a loop and stopping at the first error would leave
    /// the earlier calls in place, except here there are no earlier calls to
    /// leave in place.
    ///
    /// The durability saving is the point: under `Strict`, inserting a hundred
    /// thousand rows one at a time is a hundred thousand fsyncs. Loading a
    /// dataset is a first-hour activity, and paying that cost per row rather
    /// than per batch is not a trade anyone would choose on purpose.
    pub fn insert_batch(
        &mut self,
        collection: &str,
        records: Vec<(RecordId, Record)>,
    ) -> Result<usize> {
        let mut seen = std::collections::HashSet::with_capacity(records.len());
        let mut prepared = Vec::with_capacity(records.len());
        for (id, mut rec) in records {
            normalize_for_storage(&mut rec);
            let c = self.coll(collection)?;
            c.codec.schema().validate_record(&rec)?;
            if c.directory.get(&id).is_some_and(|v| !v.is_absent()) {
                return Err(Error::RecordExists(id));
            }
            if !seen.insert(id) {
                return Err(Error::RecordExists(id));
            }
            let bytes = c.codec.encode(&rec)?;
            prepared.push((id, bytes));
        }

        for (id, bytes) in &prepared {
            self.log_uncommitted(WalOp::Insert {
                collection: collection.to_string(),
                id: *id,
                bytes: bytes.clone(),
            })?;
        }
        if !self.replaying {
            self.wal.commit()?;
        }
        for (id, bytes) in &prepared {
            self.apply_put(collection, *id, bytes)?;
        }
        Ok(prepared.len())
    }
}
