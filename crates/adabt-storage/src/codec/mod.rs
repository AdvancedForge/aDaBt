//! Record encoding.
//!
//! This is where the schema-mode spectrum stops being a type and becomes bytes.
//! Each mode gets a genuinely different layout, chosen for what that mode
//! promises:
//!
//! | Mode | Layout | Field access |
//! |---|---|---|
//! | `Dynamic` | tag-length-value, names inline | scan |
//! | `Declared` | bitmap + fixed region + offset table + overflow | O(1) |
//! | `Strict` | bitmap + fixed region + offset table | O(1) |
//! | `Fixed` | bitmap + fixed region, constant size | O(1), computable |
//!
//! Two rules hold across all four:
//!
//! 1. **Decoding never panics.** A corrupt page must produce
//!    `Error::Corruption`, not a crash: the whole point of a self-modifying
//!    physical layer is that it can be wrong, and it must fail legibly when it
//!    is.
//! 2. **Every record carries a header with a transaction stamp**, reserved and
//!    always zero: snapshot isolation is built from in-memory version chains
//!    (`heap::mvcc`), and the bytes are kept so that an on-disk MVCC scheme
//!    would not be a format break.
//!
//! # Layout of this module
//!
//! The header, the presence bitmap and [`RecordCodec`] itself live here. The
//! child modules each own one encoding concern: `cursor` (bounds-checked
//! reads), `tlv` (self-describing single values), `value_ref` (the borrowed
//! decode of those values), `fixed` (fixed-width slots), `encode` and
//! `decode` (whole records, including field-level peeks), and `evolution`
//! (which schema changes old bytes survive).

use adabt_core::error::{Error, Result};
use adabt_core::ids::TxnId;
use adabt_core::record::Record;
use adabt_core::schema::{Schema, SchemaMode};

mod cursor;
mod decode;
mod encode;
mod evolution;
mod fixed;
mod tlv;
mod value_ref;

pub(crate) use evolution::schema_editable_in_place;
pub use fixed::read_fixed_field;
pub use tlv::{decode_value, encode_value};
pub use value_ref::{decode_value_ref, ValueRef};

pub const FORMAT_VERSION: u8 = 1;
/// version(1) + flags(1) + txn(8)
pub const HEADER_LEN: usize = 10;

pub mod flags {
    /// Tombstone: the slot is occupied but the record is logically deleted.
    pub const DELETED: u8 = 1 << 0;
    /// A `Declared` record carrying fields beyond its schema.
    pub const HAS_OVERFLOW: u8 = 1 << 1;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordHeader {
    pub version: u8,
    pub flags: u8,
    /// Reserved; always zero. Snapshot isolation is implemented with
    /// in-memory version chains (`heap::mvcc`), not per-record stamps. The
    /// bytes are kept so an on-disk MVCC scheme would not have to rewrite
    /// every record.
    pub txn: TxnId,
}

impl RecordHeader {
    pub fn new() -> Self {
        Self {
            version: FORMAT_VERSION,
            flags: 0,
            txn: TxnId(0),
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.flags & flags::DELETED != 0
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.version);
        out.push(self.flags);
        out.extend_from_slice(&self.txn.0.to_le_bytes());
    }

    fn read(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Corruption(format!(
                "record shorter than header: {} < {HEADER_LEN}",
                buf.len()
            )));
        }
        let version = buf[0];
        if version != FORMAT_VERSION {
            return Err(Error::Corruption(format!(
                "unsupported record format version {version}"
            )));
        }
        let mut txn = [0u8; 8];
        txn.copy_from_slice(&buf[2..10]);
        Ok(Self {
            version,
            flags: buf[1],
            txn: TxnId(u64::from_le_bytes(txn)),
        })
    }
}

// -- presence bitmap ------------------------------------------------------

#[inline]
pub(crate) fn bitmap_len(n_fields: usize) -> usize {
    n_fields.div_ceil(8)
}

#[inline]
fn bit_set(map: &mut [u8], i: usize) {
    map[i / 8] |= 1 << (i % 8);
}

#[inline]
fn bit_get(map: &[u8], i: usize) -> bool {
    map.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0)
}

// -- the codec ------------------------------------------------------------

/// Encode and decode records for one schema.
///
/// Constructed from a schema and thereafter reusable; it precomputes the layout
/// so that encoding does not re-derive offsets per record.
#[derive(Debug, Clone)]
pub struct RecordCodec {
    schema: Schema,
    /// Indices of fixed-width fields, in schema order.
    fixed: Vec<usize>,
    /// Indices of variable-width fields, in schema order.
    variable: Vec<usize>,
    /// Byte offset of each fixed field within the fixed region.
    fixed_offsets: Vec<u32>,
    fixed_region_len: u32,
    /// One shared `Arc` per schema field name, made once per collection.
    ///
    /// Decoding used to clone each name into a fresh `String` for every field
    /// of every row. The names are fixed for the life of the collection, so
    /// the clone was pure waste — three of the five heap allocations a
    /// three-field row cost on the scan path. These are cloned as refcounts
    /// instead.
    names: Vec<std::sync::Arc<str>>,
    /// Names seen in `Dynamic` records, shared across rows.
    ///
    /// A `Dynamic` schema carries field names in the record itself, so the
    /// schema cannot supply them. But the *names* still repeat: a collection
    /// of dynamic records overwhelmingly reuses the same handful of keys row
    /// after row. Interning turns a per-field allocation into a hash lookup
    /// and a refcount bump.
    ///
    /// Capped, because a `Dynamic` collection is allowed to have unbounded
    /// distinct field names and an uncapped table would be a slow leak. Past
    /// the cap this degrades to what it did before — an allocation per field —
    /// which is a performance cliff and never a wrong answer.
    interned: std::cell::RefCell<std::collections::HashMap<Box<str>, std::sync::Arc<str>>>,
}

impl RecordCodec {
    pub fn new(schema: Schema) -> Self {
        let mut fixed = Vec::new();
        let mut variable = Vec::new();
        let mut fixed_offsets = vec![0u32; schema.fields().len()];
        let mut off = 0u32;
        for (i, f) in schema.fields().iter().enumerate() {
            match f.ty.fixed_width() {
                Some(w) => {
                    fixed_offsets[i] = off;
                    off += w;
                    fixed.push(i);
                }
                None => variable.push(i),
            }
        }
        let names = schema
            .fields()
            .iter()
            .map(|f| std::sync::Arc::from(f.name.as_str()))
            .collect();
        Self {
            schema,
            fixed,
            variable,
            fixed_offsets,
            fixed_region_len: off,
            names,
            interned: Default::default(),
        }
    }

    /// Cap on distinct interned `Dynamic` field names. Well above any real
    /// record shape, low enough that a pathological collection cannot grow
    /// this without bound.
    const MAX_INTERNED: usize = 4096;

    /// A shared handle for `name`, allocating only the first time it is seen.
    fn intern(&self, name: &str) -> std::sync::Arc<str> {
        let mut map = self.interned.borrow_mut();
        if let Some(a) = map.get(name) {
            return std::sync::Arc::clone(a);
        }
        let a: std::sync::Arc<str> = std::sync::Arc::from(name);
        if map.len() < Self::MAX_INTERNED {
            map.insert(Box::from(name), std::sync::Arc::clone(&a));
        }
        a
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Physical size of a record, when the schema guarantees a constant one.
    ///
    /// This — not `Schema::fixed_record_size`, which is only the sum of field
    /// widths — is the stride `DirectLookup` multiplies by, because the header
    /// and presence bitmap are part of every stored record.
    pub fn physical_record_size(&self) -> Option<u32> {
        if self.schema.mode() != SchemaMode::Fixed {
            return None;
        }
        Some(
            HEADER_LEN as u32
                + bitmap_len(self.schema.fields().len()) as u32
                + self.fixed_region_len,
        )
    }

    pub fn encode(&self, rec: &Record) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(64);
        self.encode_into(rec, &mut out)?;
        Ok(out)
    }

    /// [`Self::encode`] into `out`, replacing whatever it held — for a caller
    /// encoding many records, which can then reuse one buffer for all of them.
    pub fn encode_into(&self, rec: &Record, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        self.encode_with_header(rec, RecordHeader::new(), out)
    }

    pub fn decode(&self, buf: &[u8]) -> Result<Record> {
        Ok(self.decode_with_header(buf)?.1)
    }
}

#[cfg(test)]
mod tests;
