//! The [`LogicalStore`] contract over the heap, and the slot read every
//! record fetch goes through.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::{normalize_for_storage, LogicalStore};
use adabt_core::value::Value;

use super::migration::is_migration_name;
use super::{HeapStore, SLOT_PREFIX};
use crate::catalog::encode_schema;
use crate::page::RecordLocation;
use crate::wal::{RecordFrame, WalOp};

impl HeapStore {
    /// Read one record out of the page it lives on.
    ///
    /// The codec is resolved *before* the page, and by touching
    /// `self.collections` directly rather than through `coll`. Both matter:
    /// `decompress` now borrows the page rather than copying it, so the
    /// decoded bytes hold a borrow of `self.pool` for the rest of the
    /// function. A `&self` method like `coll` borrows the whole struct and
    /// would conflict with that; naming the two fields separately lets the
    /// borrow checker see they are disjoint.
    pub(super) fn read_at(&mut self, collection: &str, loc: RecordLocation) -> Result<Record> {
        let codec = &self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?
            .codec;
        let page = self.pool.get(loc.page)?;
        let payload = page.get(loc.slot)?;
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        let bytes = crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?;
        codec.decode(&bytes)
    }
}

impl LogicalStore for HeapStore {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        if self.collections.contains_key(name) {
            return Err(Error::CollectionExists(name.to_string()));
        }
        self.log(WalOp::CreateCollection {
            name: name.to_string(),
            schema: encode_schema(&schema),
        })?;
        self.register_collection(name, schema);
        Ok(())
    }

    fn drop_collection(&mut self, name: &str) -> Result<()> {
        if !self.collections.contains_key(name) {
            return Err(Error::NoSuchCollection(name.to_string()));
        }
        self.log(WalOp::DropCollection {
            name: name.to_string(),
        })?;
        if let Some(c) = self.collections.remove(name) {
            self.by_id.remove(&c.id);
            self.free_pages_of(&c)?;
        }
        Ok(())
    }

    fn collection_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .collections
            .keys()
            .filter(|n| !is_migration_name(n))
            .cloned()
            .collect();
        v.sort();
        v
    }

    fn schema_of(&self, collection: &str) -> Result<&Schema> {
        Ok(self.coll(collection)?.codec.schema())
    }

    fn insert(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<()> {
        normalize_for_storage(&mut rec);
        let c = self.coll(collection)?;
        c.codec.schema().validate_record(&rec)?;
        if c.directory.get(&id).is_some_and(|v| !v.is_absent()) {
            return Err(Error::RecordExists(id));
        }
        let bytes = c.codec.encode(&rec)?;
        self.check_fits(&bytes)?;
        self.log_record(RecordFrame::Insert {
            collection,
            id,
            bytes: &bytes,
        })?;
        self.apply_put(collection, id, &bytes)
    }

    fn insert_keyed(
        &mut self,
        collection: &str,
        id: RecordId,
        rec: Record,
        key: i64,
    ) -> Result<()> {
        let mut rec = rec;
        normalize_for_storage(&mut rec);
        let c = self.coll(collection)?;
        c.codec.schema().validate_record(&rec)?;
        if c.directory.get(&id).is_some_and(|v| !v.is_absent()) {
            return Err(Error::RecordExists(id));
        }
        let bytes = c.codec.encode(&rec)?;
        self.check_fits(&bytes)?;
        // The log carries the same op either way: clustering is placement, not
        // content, so replay reproduces the bytes exactly and re-derives (or
        // forgets, which is equally correct) the locality.
        self.log_record(RecordFrame::Insert {
            collection,
            id,
            bytes: &bytes,
        })?;
        self.apply_put_keyed(collection, id, &bytes, key)
    }

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|v| v.newest())
        else {
            // Still validates the collection exists, above.
            return Ok(None);
        };
        self.touched.insert(loc.page.0);
        Ok(Some(self.read_at(collection, loc)?))
    }

    fn peek_field(
        &mut self,
        collection: &str,
        id: RecordId,
        field: &str,
    ) -> Result<Option<Option<Value>>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|v| v.newest())
        else {
            return Ok(None);
        };
        let codec = &self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?
            .codec;
        let page = self.pool.get(loc.page)?;
        let payload = page.get(loc.slot)?;
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        let bytes = crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?;
        self.touched.insert(loc.page.0);
        Ok(Some(codec.peek_field(&bytes, field)?))
    }

    fn get_projected(
        &mut self,
        collection: &str,
        id: RecordId,
        fields: &[&str],
    ) -> Result<Option<Record>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|v| v.newest())
        else {
            return Ok(None);
        };
        let codec = &self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?
            .codec;
        let page = self.pool.get(loc.page)?;
        let payload = page.get(loc.slot)?;
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        let bytes = crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?;
        self.touched.insert(loc.page.0);
        if fields.is_empty() {
            return Ok(Some(Record::new()));
        }
        Ok(Some(codec.peek_fields(&bytes, fields)?))
    }

    fn update(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<bool> {
        normalize_for_storage(&mut rec);
        let c = self.coll(collection)?;
        c.codec.schema().validate_record(&rec)?;
        let existed = c.directory.get(&id).is_some_and(|v| !v.is_absent());
        let bytes = c.codec.encode(&rec)?;
        self.check_fits(&bytes)?;
        self.log_record(RecordFrame::Update {
            collection,
            id,
            bytes: &bytes,
        })?;
        self.apply_put(collection, id, &bytes)?;
        Ok(existed)
    }

    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let existed = self
            .coll(collection)?
            .directory
            .get(&id)
            .is_some_and(|v| !v.is_absent());
        if existed {
            self.log_record(RecordFrame::Delete { collection, id })?;
        }
        self.apply_delete(collection, id)
    }

    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        let locs: Vec<(RecordId, RecordLocation)> = self
            .coll(collection)?
            .directory
            .iter()
            .filter_map(|(k, v)| v.newest().map(|l| (*k, l)))
            .collect();
        let mut out = Vec::with_capacity(locs.len());
        for (id, loc) in locs {
            out.push((id, self.read_at(collection, loc)?));
        }
        Ok(out)
    }

    /// O(1): the directory keeps its live count as it changes.
    fn count(&mut self, collection: &str) -> Result<usize> {
        Ok(self.coll(collection)?.directory.live())
    }

    /// Straight off the in-memory page directory: no pages, no decodes.
    ///
    /// The filter matches `scan`'s exactly — `newest()` is `None` for a record
    /// whose newest version is a tombstone — because the two must agree. They
    /// are the same question asked at different costs, and a scan driven by
    /// ids that disagreed with `scan` would drop or invent rows.
    fn ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        Ok(self
            .coll(collection)?
            .directory
            .iter()
            .filter_map(|(k, v)| v.newest().map(|_| *k))
            .collect())
    }
}
