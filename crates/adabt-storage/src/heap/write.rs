//! The physical write path: building slot payloads, recording versions in
//! the directory, and logging each operation before it is applied.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{CollectionId, RecordId, TxnId};
use adabt_core::record::Record;
use adabt_core::store::normalize_for_storage;

use super::{HeapStore, MAX_RECORD_BYTES, SLOT_PREFIX};
use crate::page::MAX_PAYLOAD;
use crate::wal::{RecordFrame, WalOp};

impl HeapStore {
    /// Write the slot payload for `bytes` into `payload`, replacing whatever
    /// it held: the owner prefix, the encoding byte, then the record —
    /// compressed when that is enabled and pays.
    fn build_payload(
        cid: CollectionId,
        id: RecordId,
        bytes: &[u8],
        compress: bool,
        payload: &mut Vec<u8>,
    ) {
        payload.clear();
        payload.extend_from_slice(&cid.0.to_le_bytes());
        payload.extend_from_slice(&id.0.to_le_bytes());
        if compress {
            let (encoding, body) = crate::compress::maybe_compress(bytes);
            payload.push(encoding.bit());
            payload.extend_from_slice(&body);
        } else {
            payload.push(crate::compress::Encoding::Raw.bit());
            payload.extend_from_slice(bytes);
        }
    }

    /// The collection and record a stored payload's prefix names.
    pub(super) fn payload_owner(payload: &[u8]) -> Option<(CollectionId, RecordId)> {
        let cid = payload.get(0..4)?.try_into().ok()?;
        let id = payload.get(4..12)?.try_into().ok()?;
        Some((
            CollectionId(u32::from_le_bytes(cid)),
            RecordId(u64::from_le_bytes(id)),
        ))
    }

    /// The record bytes a stored payload holds, decompressed: what
    /// [`WalOp::Update`] logs for it.
    pub(super) fn payload_record(payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        Ok(crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?.into_owned())
    }

    /// Insert or overwrite. The single physical write path, so insert, update
    /// and replay cannot drift apart.
    /// Write a new version. Never updates in place: a superseded version may
    /// still be the answer for an open snapshot, and overwriting it would move
    /// the ground under a reader mid-scan.
    pub(super) fn apply_put(&mut self, collection: &str, id: RecordId, bytes: &[u8]) -> Result<()> {
        self.apply_put_placed(collection, id, bytes, None)
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
        self.apply_put_placed(collection, id, bytes, Some(key))
    }

    /// The body of [`Self::apply_put`] and [`Self::apply_put_keyed`]: place
    /// the payload — near `key` when there is one — and record the new
    /// version.
    fn apply_put_placed(
        &mut self,
        collection: &str,
        id: RecordId,
        bytes: &[u8],
        key: Option<i64>,
    ) -> Result<()> {
        let Some(c) = self.collections.get(collection) else {
            // Replay of an operation against a collection later dropped.
            return Ok(());
        };
        let cid = c.id;
        // The payload is only needed until it is copied into a page, so one
        // buffer serves every write; it is taken out of `self` for the
        // duration because placing borrows the store mutably.
        let mut payload = std::mem::take(&mut self.payload);
        Self::build_payload(cid, id, bytes, self.compress, &mut payload);
        if payload.len() > MAX_PAYLOAD {
            return Err(too_large(bytes.len()));
        }

        let txn = self.versions.begin_write();
        let placed = match key {
            Some(key) => self.place_keyed(collection, &payload, key),
            None => self.place(&payload),
        };
        self.payload = payload;
        let loc = placed?;
        let c = self.collections.get_mut(collection).expect("checked above");
        let superseded = c.directory.push(id, txn, Some(loc));
        c.note_write(txn, id);
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

    pub(super) fn apply_delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let txn = self.versions.begin_write();
        let Some(c) = self.collections.get_mut(collection) else {
            return Ok(false);
        };
        if c.directory.get(&id).is_none_or(|chain| chain.is_absent()) {
            return Ok(false);
        }
        // A tombstone rather than a removal: a reader whose snapshot predates
        // the delete must still find the record.
        c.directory.push(id, txn, None);
        c.note_write(txn, id);
        self.retained += 1;
        self.maybe_reclaim()?;
        Ok(true)
    }

    /// Log one autocommit operation and make it durable per policy.
    ///
    /// Whatever the policy, the entry is also handed to the operating system
    /// before this returns, ahead of the page change it describes. The buffer
    /// pool may write that page to the heap file at any moment, and recovery
    /// trusts the directory snapshot's location for every record the log does
    /// not mention; a page that reached the file while its log entry was still
    /// in this process's buffer would break that the moment the process died.
    pub(super) fn log(&mut self, op: WalOp) -> Result<()> {
        if self.replaying {
            return Ok(());
        }
        self.wal.append(TxnId(0), op)?;
        self.wal.commit()?;
        self.wal.flush()
    }

    /// [`Self::log`] for a record-level write, encoded straight from the
    /// caller's buffers rather than from a copy of them.
    pub(super) fn log_record(&mut self, frame: RecordFrame<'_>) -> Result<()> {
        if self.replaying {
            return Ok(());
        }
        self.wal.append_record(TxnId(0), frame)?;
        self.wal.commit()?;
        self.wal.flush()
    }

    /// Refuse an encoded record that could never be placed in a page.
    ///
    /// Checked before the record is logged, never only when it is placed: a
    /// logged write is one recovery will replay, and one that cannot be placed
    /// would then fail every open of the database, not just the call that
    /// wrote it.
    pub(super) fn check_fits(&self, bytes: &[u8]) -> Result<()> {
        if SLOT_PREFIX + bytes.len() <= MAX_PAYLOAD {
            return Ok(());
        }
        // Too big raw, but compression decides what is actually stored —
        // exactly as `build_payload` will.
        if self.compress {
            let (_, body) = crate::compress::maybe_compress(bytes);
            if SLOT_PREFIX + body.len() <= MAX_PAYLOAD {
                return Ok(());
            }
        }
        Err(too_large(bytes.len()))
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
    /// All-or-nothing holds against a crash too. The batch is logged as one
    /// transaction — tagged frames and a single commit record, see
    /// `heap::commit` — before any row reaches a page, so recovery replays
    /// every row of it or none.
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
        self.insert_batch_from(collection, &records)
    }

    /// [`Self::insert_batch`] from records the caller keeps.
    ///
    /// For a caller that still needs the records once they are stored — the
    /// engine indexes them — and would otherwise have to copy every one of
    /// them to hand the store a batch it can consume. A record holding a null
    /// field is normalised on a private copy; one already in stored shape,
    /// which is what such a caller passes, is encoded where it lies.
    pub fn insert_batch_from(
        &mut self,
        collection: &str,
        records: &[(RecordId, Record)],
    ) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let c = self.coll(collection)?;
        // Ids in strictly ascending order — what a load almost always is —
        // cannot repeat within the batch, and when the first of them is past
        // the collection's highest id, none can already exist. Either way the
        // check per row is skipped only where it could not fail.
        let ascending = records.windows(2).all(|w| w[0].0 < w[1].0);
        let beyond_existing = ascending
            && match (records.first(), c.directory.last_key_value()) {
                (Some((first, _)), Some((last, _))) => first > last,
                _ => true,
            };
        let mut seen = std::collections::HashSet::new();
        // Every encoded record goes into one buffer, addressed by range, so a
        // batch costs a handful of allocations rather than one per row.
        let mut encoded = Vec::new();
        let mut scratch = Vec::new();
        let mut prepared = Vec::with_capacity(records.len());
        for (id, rec) in records {
            let normalized;
            let rec = if rec.iter().any(|(_, v)| v.is_null()) {
                let mut copy = rec.clone();
                normalize_for_storage(&mut copy);
                normalized = copy;
                &normalized
            } else {
                rec
            };
            c.codec.schema().validate_record(rec)?;
            if !beyond_existing && c.directory.get(id).is_some_and(|v| !v.is_absent()) {
                return Err(Error::RecordExists(*id));
            }
            if !ascending && !seen.insert(*id) {
                return Err(Error::RecordExists(*id));
            }
            c.codec.encode_into(rec, &mut scratch)?;
            self.check_fits(&scratch)?;
            let start = encoded.len();
            encoded.extend_from_slice(&scratch);
            prepared.push((*id, start..encoded.len()));
        }

        self.log_atomically(prepared.iter().map(|(id, at)| RecordFrame::Insert {
            collection,
            id: *id,
            bytes: &encoded[at.clone()],
        }))?;
        for (id, at) in &prepared {
            self.apply_put(collection, *id, &encoded[at.clone()])?;
        }
        Ok(prepared.len())
    }
}

/// The refusal for a record that cannot fit in a page.
fn too_large(len: usize) -> Error {
    Error::Corruption(format!(
        "record of {len} bytes exceeds the {MAX_RECORD_BYTES}-byte page limit"
    ))
}
