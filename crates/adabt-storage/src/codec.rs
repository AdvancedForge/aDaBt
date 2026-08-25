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
//! 2. **Every record carries an MVCC header**, reserved now even though nothing
//!    reads it yet, so snapshot isolation is not a format break later.

use adabt_core::error::{Error, Result};
use adabt_core::ids::TxnId;
use adabt_core::record::Record;
use adabt_core::schema::{FieldType, Schema, SchemaMode};
use adabt_core::value::Value;

use crate::varint;

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
    /// Reserved for MVCC. Always zero until snapshot isolation lands; the bytes
    /// exist now so adding it does not rewrite every record on disk.
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

/// Bounds-checked reader. Every decode path goes through this so that a
/// truncated or corrupt buffer yields an error rather than a panic.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            Error::Corruption("length overflow while decoding record".to_string())
        })?;
        if end > self.buf.len() {
            return Err(Error::Corruption(format!(
                "record truncated: wanted {n} bytes at {}, {} available",
                self.pos,
                self.buf.len().saturating_sub(self.pos)
            )));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn varint(&mut self) -> Result<u64> {
        let (v, used) = varint::read_u64(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(v)
    }

    /// The same bytes as `string`, without owning them.
    ///
    /// A `Dynamic` record carries its own field names, so decoding one used
    /// to allocate a `String` per field per row before anything looked at it.
    /// Borrowing lets the caller decide whether a fresh allocation is needed
    /// at all — usually it is not, because the same names repeat on every row.
    fn str_ref(&mut self, n: usize) -> Result<&'a str> {
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in record: {e}")))
    }

    fn string(&mut self, n: usize) -> Result<String> {
        let b = self.take(n)?;
        String::from_utf8(b.to_vec())
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in record: {e}")))
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
}

// -- zigzag ---------------------------------------------------------------

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

// -- tag-length-value -----------------------------------------------------

mod tag {
    pub const NULL: u8 = 0;
    pub const FALSE: u8 = 1;
    pub const TRUE: u8 = 2;
    pub const I64: u8 = 3;
    pub const U64: u8 = 4;
    pub const F64: u8 = 5;
    pub const STR: u8 = 6;
    pub const BYTES: u8 = 7;
    pub const LIST: u8 = 8;
    pub const MAP: u8 = 9;
    pub const DECIMAL: u8 = 10;
    pub const TIMESTAMP: u8 = 11;
}

use adabt_core::value::rescale;

/// Maximum nesting depth accepted when decoding.
///
/// Without it, a corrupt buffer describing deeply nested lists would recurse
/// until the stack overflows, which is a crash rather than an error.
const MAX_DEPTH: u32 = 64;

/// Encode one value, self-describing, with no schema needed to read it back.
///
/// Exposed because the derived-representation cache stores index keys, which are
/// bare values rather than records. Sharing this encoding rather than inventing
/// a second one means an index key round-trips through exactly the code the
/// differential and property tests already cover.
pub fn encode_value(v: &Value, out: &mut Vec<u8>) {
    write_tlv_value(v, out)
}

/// Decode one value written by [`encode_value`], returning it and the number of
/// bytes consumed.
pub fn decode_value(buf: &[u8]) -> Result<(Value, usize)> {
    let mut c = Cursor::new(buf);
    let v = read_tlv_value(&mut c, 0)?;
    Ok((v, c.pos))
}

fn write_tlv_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(tag::NULL),
        Value::Decimal { units, scale } => {
            out.push(tag::DECIMAL);
            out.push(*scale);
            out.extend_from_slice(&units.to_le_bytes());
        }
        Value::Timestamp(t) => {
            out.push(tag::TIMESTAMP);
            out.extend_from_slice(&t.to_le_bytes());
        }
        // Booleans get a tag each, so they cost one byte rather than two.
        Value::Bool(false) => out.push(tag::FALSE),
        Value::Bool(true) => out.push(tag::TRUE),
        Value::I64(n) => {
            out.push(tag::I64);
            varint::write_u64(zigzag(*n), out);
        }
        Value::U64(n) => {
            out.push(tag::U64);
            varint::write_u64(*n, out);
        }
        Value::F64(f) => {
            out.push(tag::F64);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Str(s) => {
            out.push(tag::STR);
            varint::write_u64(s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(tag::BYTES);
            varint::write_u64(b.len() as u64, out);
            out.extend_from_slice(b);
        }
        Value::List(items) => {
            out.push(tag::LIST);
            varint::write_u64(items.len() as u64, out);
            for i in items {
                write_tlv_value(i, out);
            }
        }
        Value::Map(m) => {
            out.push(tag::MAP);
            varint::write_u64(m.len() as u64, out);
            for (k, val) in m {
                varint::write_u64(k.len() as u64, out);
                out.extend_from_slice(k.as_bytes());
                write_tlv_value(val, out);
            }
        }
    }
}

fn read_tlv_value(c: &mut Cursor<'_>, depth: u32) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(Error::Corruption(format!(
            "record nesting deeper than {MAX_DEPTH}"
        )));
    }
    let t = c.u8()?;
    Ok(match t {
        tag::NULL => Value::Null,
        tag::DECIMAL => {
            let scale = c.u8()?;
            let mut b = [0u8; 16];
            b.copy_from_slice(c.take(16)?);
            Value::Decimal {
                units: i128::from_le_bytes(b),
                scale,
            }
        }
        tag::TIMESTAMP => Value::Timestamp(c.u64()? as i64),
        tag::FALSE => Value::Bool(false),
        tag::TRUE => Value::Bool(true),
        tag::I64 => Value::I64(unzigzag(c.varint()?)),
        tag::U64 => Value::U64(c.varint()?),
        tag::F64 => Value::F64(f64::from_bits(c.u64()?)),
        tag::STR => {
            let n = c.varint()? as usize;
            Value::Str(c.string(n)?)
        }
        tag::BYTES => {
            let n = c.varint()? as usize;
            Value::Bytes(c.take(n)?.to_vec())
        }
        tag::LIST => {
            let n = c.varint()? as usize;
            // A corrupt count must not pre-allocate gigabytes: one element is
            // at least one byte, so the buffer bounds the plausible count.
            if n > c.remaining() {
                return Err(Error::Corruption(format!(
                    "list of {n} items exceeds {} remaining bytes",
                    c.remaining()
                )));
            }
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(read_tlv_value(c, depth + 1)?);
            }
            Value::List(items)
        }
        tag::MAP => {
            let n = c.varint()? as usize;
            if n > c.remaining() {
                return Err(Error::Corruption(format!(
                    "map of {n} entries exceeds {} remaining bytes",
                    c.remaining()
                )));
            }
            let mut m = std::collections::BTreeMap::new();
            for _ in 0..n {
                let klen = c.varint()? as usize;
                let k = c.string(klen)?;
                m.insert(k, read_tlv_value(c, depth + 1)?);
            }
            Value::Map(m)
        }
        other => {
            return Err(Error::Corruption(format!(
                "unknown value tag {other} in record"
            )))
        }
    })
}

// -- fixed-width slots ----------------------------------------------------

/// Write `v` into exactly `width` bytes at the end of `out`.
///
/// Text and byte fields store their length inline, then their content, then
/// zero padding. That is what makes the slot lossless: padding schemes lose
/// trailing NULs or trailing spaces, and a fixed layout that silently corrupts
/// its own data is worse than no fixed layout at all.
fn write_fixed_slot(ty: &FieldType, v: &Value, width: u32, out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    match ty {
        FieldType::Bool => out.push(matches!(v, Value::Bool(true)) as u8),
        FieldType::I64 => {
            let n = match v {
                Value::I64(n) => *n,
                Value::U64(n) => *n as i64,
                _ => 0,
            };
            out.extend_from_slice(&n.to_le_bytes());
        }
        FieldType::U64 => {
            let n = match v {
                Value::U64(n) => *n,
                Value::I64(n) => *n as u64,
                _ => 0,
            };
            out.extend_from_slice(&n.to_le_bytes());
        }
        FieldType::Timestamp => {
            let t = match v {
                Value::Timestamp(t) => *t,
                _ => 0,
            };
            out.extend_from_slice(&t.to_le_bytes());
        }
        FieldType::Decimal { scale } => {
            // Stored at the *field's* scale, so every row occupies the same
            // sixteen bytes and the column can be addressed arithmetically.
            // Rescaling an integer or a coarser decimal is exact; a finer one
            // cannot be represented and is rejected before it reaches here.
            let units = match v {
                Value::Decimal { units, scale: s } => rescale(*units, *s, *scale).unwrap_or(0),
                Value::I64(n) => rescale(*n as i128, 0, *scale).unwrap_or(0),
                Value::U64(n) => rescale(*n as i128, 0, *scale).unwrap_or(0),
                _ => 0,
            };
            out.extend_from_slice(&units.to_le_bytes());
        }
        FieldType::F64 => {
            let f = match v {
                Value::F64(f) => *f,
                Value::I64(n) => *n as f64,
                Value::U64(n) => *n as f64,
                _ => 0.0,
            };
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        FieldType::Char(_) | FieldType::FixedBytes(_) => {
            let content: &[u8] = match v {
                Value::Str(s) => s.as_bytes(),
                Value::Bytes(b) => b,
                _ => &[],
            };
            let prefix = FieldType::length_prefix_bytes(width) as usize;
            let capacity = width as usize - prefix;
            if content.len() > capacity {
                return Err(Error::Corruption(format!(
                    "value of {} bytes does not fit fixed slot capacity {capacity}",
                    content.len()
                )));
            }
            let len = content.len() as u32;
            out.extend_from_slice(&len.to_le_bytes()[..prefix]);
            out.extend_from_slice(content);
            out.resize(start + width as usize, 0);
        }
        other => {
            return Err(Error::InvalidOptimization(format!(
                "{} is not a fixed-width type",
                other.name()
            )))
        }
    }
    debug_assert_eq!(
        out.len() - start,
        width as usize,
        "fixed slot width mismatch"
    );
    Ok(())
}

/// Decode one fixed-width field from its exact bytes.
///
/// Public so a directly-addressed array can read a single field from a computed
/// address without decoding the record around it.
pub fn read_fixed_field(ty: &FieldType, buf: &[u8]) -> Result<Value> {
    let mut c = Cursor::new(buf);
    Ok(match ty {
        FieldType::Bool => Value::Bool(c.u8()? != 0),
        FieldType::I64 => Value::I64(c.u64()? as i64),
        FieldType::U64 => Value::U64(c.u64()?),
        FieldType::F64 => Value::F64(f64::from_bits(c.u64()?)),
        FieldType::Timestamp => Value::Timestamp(c.u64()? as i64),
        FieldType::Decimal { scale } => {
            let mut b = [0u8; 16];
            b.copy_from_slice(c.take(16)?);
            Value::Decimal {
                units: i128::from_le_bytes(b),
                scale: *scale,
            }
        }
        FieldType::Char(w) | FieldType::FixedBytes(w) => {
            let prefix = FieldType::length_prefix_bytes(*w) as usize;
            let raw = c.take(prefix)?;
            let mut le = [0u8; 4];
            le[..prefix].copy_from_slice(raw);
            let len = u32::from_le_bytes(le) as usize;
            let capacity = *w as usize - prefix;
            if len > capacity {
                return Err(Error::Corruption(format!(
                    "fixed slot declares length {len} beyond capacity {capacity}"
                )));
            }
            if matches!(ty, FieldType::Char(_)) {
                Value::Str(c.string(len)?)
            } else {
                Value::Bytes(c.take(len)?.to_vec())
            }
        }
        other => {
            return Err(Error::InvalidOptimization(format!(
                "{} is not a fixed-width type",
                other.name()
            )))
        }
    })
}

// -- presence bitmap ------------------------------------------------------

#[inline]
pub(crate) fn bitmap_len(n_fields: usize) -> usize {
    n_fields.div_ceil(8)
}

/// Whether every record already on disk under `old` still decodes correctly
/// under `new`, with no byte touched — the condition that lets a schema change
/// become a catalog edit instead of the copy-and-swap in
/// `HeapStore::alter_schema`.
///
/// This is not one rule; it is a different rule per mode, because the layout
/// table at the top of this file is a different rule per mode. What follows
/// is the layout argument for each; `mod in_place_eligibility` below holds it
/// to the same standard every other bit-level claim in this file is held to —
/// a written-out record, decoded and checked, not just reasoned about. An
/// earlier version of this function allowed fixed-width appends for `Strict`
/// and `Declared` on exactly this reasoning before that suite caught it:
/// wrong, in a way pure reasoning about "the field is presence-gated" made
/// look safe.
///
/// - **`Fixed`.** The layout is bitmap, then fixed region, then nothing —
///   `RecordCodec::decode_with_header` returns as soon as the fixed fields are
///   read, so there is no offset table whose position depends on
///   `fixed_region_len`. That is what makes *both* directions safe here and
///   nowhere else: appending one nullable field at the tail only adds a
///   presence-gated read past the old data (old records have that bit unset,
///   so it is skipped, unless the new field count crosses into an additional
///   bitmap byte — `bitmap_len` growing means the bitmap itself is now longer
///   than an old record's, which is checked and turned into a loud
///   `Error::Corruption` rather than read past); dropping the last field
///   shrinks the region old records simply have unread trailing bytes beyond.
///   `nullable` on the added field is required for a different reason than
///   safety: decode does not consult `Schema::validate_record` at all, so
///   without it an old record would silently violate a `required` constraint
///   the schema is supposed to guarantee on every row.
/// - **`Strict`.** Fixed fields sit before the offset table, so *any* change
///   to the fixed field set — adding one or dropping one — moves
///   `fixed_region_len` and therefore `table_at`, the position decode looks
///   for that table at. An old record's table is not there any more; this is
///   exactly the bug the `Fixed`-mode reasoning above does not have, and it
///   is why `Strict` cannot take the append path at all, in either field
///   width. Dropping the last field is safe only when that field was
///   *variable*-width: the fixed region — and so `table_at` — is untouched,
///   and the new schema simply reads one fewer table entry than the old
///   record's table has, which is a safe prefix of real bytes. (Appending a
///   variable-width field is the mirror case — `table_at` likewise untouched
///   — but reading one *extra* entry means reading past the old table into
///   whatever data follows, which is not reliably a valid offset. It happens
///   to look safe by the same presence-gating argument that was wrong above,
///   so it is excluded too, not allowed on the strength of that argument.)
/// - **`Declared`.** Never takes either path. Appending fails for the same
///   reason as `Strict`. Dropping the last field would otherwise work the
///   same way `Strict`'s does, except a `Declared` record may carry a real
///   overflow bag (`flags::HAS_OVERFLOW`) immediately after its last declared
///   field; the new schema's table is then one entry short of the old one,
///   and it reads the dropped field's own bytes as the overflow section's
///   leading length-prefixed count — silently wrong, not a decode error.
///   Telling the two cases apart means reading every row, which is the cost
///   this function exists to avoid.
/// - **`Dynamic`.** Has no declared fields to add or drop in the first place.
pub(crate) fn schema_editable_in_place(old: &Schema, new: &Schema) -> bool {
    if old.mode() != new.mode() {
        return false;
    }
    let (of, nf) = (old.fields(), new.fields());
    match old.mode() {
        SchemaMode::Dynamic | SchemaMode::Declared => false,
        SchemaMode::Fixed => {
            if nf.len() == of.len() + 1 && nf[..of.len()] == *of {
                let added = &nf[of.len()];
                added.nullable && bitmap_len(nf.len()) == bitmap_len(of.len())
            } else {
                of.len() == nf.len() + 1 && of[..nf.len()] == *nf
            }
        }
        SchemaMode::Strict => {
            of.len() == nf.len() + 1
                && of[..nf.len()] == *nf
                && of[nf.len()].ty.fixed_width().is_none()
        }
    }
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
        self.encode_with_header(rec, RecordHeader::new(), &mut out)?;
        Ok(out)
    }

    pub fn encode_with_header(
        &self,
        rec: &Record,
        mut header: RecordHeader,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if self.schema.mode() == SchemaMode::Dynamic {
            header.write(out);
            varint::write_u64(rec.len() as u64, out);
            for (name, v) in rec.iter() {
                varint::write_u64(name.len() as u64, out);
                out.extend_from_slice(name.as_bytes());
                write_tlv_value(v, out);
            }
            return Ok(());
        }

        let fields = self.schema.fields();
        let n = fields.len();

        // Overflow is only legal where the mode permits undeclared fields.
        let overflow: Vec<(&str, &Value)> = if self.schema.mode().allows_extra_fields() {
            rec.iter()
                .filter(|(name, _)| self.schema.field(name).is_none())
                .collect()
        } else {
            Vec::new()
        };
        if !overflow.is_empty() {
            header.flags |= flags::HAS_OVERFLOW;
        }

        header.write(out);
        let bitmap_at = out.len();
        out.resize(bitmap_at + bitmap_len(n), 0);

        // Fixed region. Absent fields still occupy their slot so that every
        // field offset is a constant, which is what makes O(1) access and later
        // direct addressing possible.
        for &i in &self.fixed {
            let f = &fields[i];
            let present = matches!(rec.get(&f.name), Some(v) if !v.is_null());
            if present {
                let v = rec.get(&f.name).unwrap();
                let w = f.ty.fixed_width().expect("classified as fixed");
                write_fixed_slot(&f.ty, v, w, out)?;
            } else {
                let w = f.ty.fixed_width().expect("classified as fixed") as usize;
                out.resize(out.len() + w, 0);
            }
        }

        if self.schema.mode() == SchemaMode::Fixed {
            debug_assert!(self.variable.is_empty());
            for (i, f) in fields.iter().enumerate() {
                if matches!(rec.get(&f.name), Some(v) if !v.is_null()) {
                    bit_set(&mut out[bitmap_at..bitmap_at + bitmap_len(n)], i);
                }
            }
            return Ok(());
        }

        // Offset table: one entry per variable field plus a sentinel marking the
        // end of the variable region, so each length is the gap to the next.
        let table_at = out.len();
        out.resize(table_at + 4 * (self.variable.len() + 1), 0);

        let mut var_offsets: Vec<u32> = Vec::with_capacity(self.variable.len() + 1);
        for &i in &self.variable {
            var_offsets.push(out.len() as u32);
            let f = &fields[i];
            if let Some(v) = rec.get(&f.name) {
                if !v.is_null() {
                    write_tlv_value(v, out);
                }
            }
        }
        var_offsets.push(out.len() as u32);

        for (k, off) in var_offsets.iter().enumerate() {
            let at = table_at + 4 * k;
            out[at..at + 4].copy_from_slice(&off.to_le_bytes());
        }

        if !overflow.is_empty() {
            varint::write_u64(overflow.len() as u64, out);
            for (name, v) in overflow {
                varint::write_u64(name.len() as u64, out);
                out.extend_from_slice(name.as_bytes());
                write_tlv_value(v, out);
            }
        }

        for (i, f) in fields.iter().enumerate() {
            if matches!(rec.get(&f.name), Some(v) if !v.is_null()) {
                bit_set(&mut out[bitmap_at..bitmap_at + bitmap_len(n)], i);
            }
        }
        Ok(())
    }

    pub fn decode(&self, buf: &[u8]) -> Result<Record> {
        Ok(self.decode_with_header(buf)?.1)
    }

    /// Decode exactly one field of an encoded record, borrowing the buffer.
    ///
    /// This is the seed of the borrowed view M27 named: a fetch that needs
    /// two of twenty fields should not pay for a `Record` — the map, the
    /// name refcounts, the reserve — to throw the other eighteen away. It
    /// walks only what the requested field needs: the header, its presence
    /// bit, and (for variable fields) one offset-table entry. Everything else
    /// in the buffer is untouched bytes.
    ///
    /// The returned `Value` is still owned — strings copy out of the buffer,
    /// because lending slices tied to an encoded page's lifetime is the
    /// executor's next step, not this one. What lands here is the *skip*
    /// cost: decoding one field of a wide record costs one field, not one
    /// record.
    pub fn peek_field(&self, buf: &[u8], field: &str) -> Result<Option<Value>> {
        if self.schema.mode() == SchemaMode::Dynamic {
            // Names live in the record; walk TLVs and match raw bytes so a
            // miss costs a scan of name prefixes, not a decode of values.
            let mut c = Cursor::new(buf);
            c.take(HEADER_LEN)?;
            let n = c.varint()? as usize;
            for _ in 0..n {
                let klen = c.varint()? as usize;
                let matches = {
                    let raw = c.take(klen)?;
                    raw == field.as_bytes()
                };
                let v = read_tlv_value(&mut c, 0)?;
                if matches {
                    return Ok(Some(v));
                }
            }
            return Ok(None);
        }

        let fields = self.schema.fields();
        let Some(i) = fields.iter().position(|f| f.name == field) else {
            return Ok(None);
        };
        let n = fields.len();
        let bm_len = bitmap_len(n);
        if buf.len() < HEADER_LEN + bm_len {
            return Err(Error::Corruption(
                "record too short for its presence bitmap".to_string(),
            ));
        }
        let bitmap = &buf[HEADER_LEN..HEADER_LEN + bm_len];
        if !bit_get(bitmap, i) {
            return Ok(None);
        }
        let fixed_at = HEADER_LEN + bm_len;

        if let Some(w) = fields[i].ty.fixed_width() {
            let at = fixed_at + self.fixed_offsets[i] as usize;
            let end = at + w as usize;
            if end > buf.len() {
                return Err(Error::Corruption(format!(
                    "fixed field {i} extends past the record"
                )));
            }
            return read_fixed_field(&fields[i].ty, &buf[at..end]).map(Some);
        }

        let k = self
            .variable
            .iter()
            .position(|&v| v == i)
            .expect("variable field indexed");
        let table_at = fixed_at + self.fixed_region_len as usize;
        let entries = self.variable.len() + 1;
        if table_at + 4 * entries > buf.len() {
            return Err(Error::Corruption(
                "record too short for its offset table".to_string(),
            ));
        }
        let offset_at = |kk: usize| -> usize {
            let at = table_at + 4 * kk;
            u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) as usize
        };
        let (lo, hi) = (offset_at(k), offset_at(k + 1));
        if lo > hi || hi > buf.len() {
            return Err(Error::Corruption(format!(
                "offset table entry {k} is out of range"
            )));
        }
        let mut c = Cursor::new(&buf[lo..hi]);
        read_tlv_value(&mut c, 0).map(Some)
    }

    pub fn decode_with_header(&self, buf: &[u8]) -> Result<(RecordHeader, Record)> {
        let header = RecordHeader::read(buf)?;
        let mut rec = Record::new();
        // One allocation for the whole record rather than a regrow per field.
        rec.reserve(self.schema.fields().len().max(1));

        if self.schema.mode() == SchemaMode::Dynamic {
            let mut c = Cursor::new(buf);
            c.take(HEADER_LEN)?;
            let n = c.varint()? as usize;
            if n > c.remaining() {
                return Err(Error::Corruption(format!(
                    "record declares {n} fields but only {} bytes remain",
                    c.remaining()
                )));
            }
            for _ in 0..n {
                let klen = c.varint()? as usize;
                let name = self.intern(c.str_ref(klen)?);
                let v = read_tlv_value(&mut c, 0)?;
                rec.set_shared(name, v);
            }
            return Ok((header, rec));
        }

        let fields = self.schema.fields();
        let n = fields.len();
        let bm_len = bitmap_len(n);
        if buf.len() < HEADER_LEN + bm_len {
            return Err(Error::Corruption(
                "record too short for its presence bitmap".to_string(),
            ));
        }
        let bitmap = &buf[HEADER_LEN..HEADER_LEN + bm_len];
        let fixed_at = HEADER_LEN + bm_len;

        for (k, &i) in self.fixed.iter().enumerate() {
            if !bit_get(bitmap, i) {
                continue;
            }
            let f = &fields[i];
            let w = f.ty.fixed_width().expect("classified as fixed") as usize;
            let at = fixed_at + self.fixed_offsets[i] as usize;
            let end = at + w;
            if end > buf.len() {
                return Err(Error::Corruption(format!(
                    "fixed field {k} extends past the record"
                )));
            }
            rec.set_shared(
                std::sync::Arc::clone(&self.names[i]),
                read_fixed_field(&f.ty, &buf[at..end])?,
            );
        }

        if self.schema.mode() == SchemaMode::Fixed {
            return Ok((header, rec));
        }

        let table_at = fixed_at + self.fixed_region_len as usize;
        let entries = self.variable.len() + 1;
        if table_at + 4 * entries > buf.len() {
            return Err(Error::Corruption(
                "record too short for its offset table".to_string(),
            ));
        }
        // Read straight out of the buffer rather than materialising the table.
        // It was one `Vec` per record decoded — the last allocation on this
        // path that the record itself does not need.
        let offset_at = |k: usize| -> usize {
            let at = table_at + 4 * k;
            u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) as usize
        };
        // Offsets come from the buffer, so they are untrusted: verify they are
        // ordered and in range before slicing with them.
        let mut last_offset = offset_at(0);
        for k in 1..entries {
            let next = offset_at(k);
            if last_offset > next {
                return Err(Error::Corruption(
                    "offset table is not monotonically increasing".to_string(),
                ));
            }
            last_offset = next;
        }
        if last_offset > buf.len() {
            return Err(Error::Corruption(
                "offset table points past the end of the record".to_string(),
            ));
        }

        for (k, &i) in self.variable.iter().enumerate() {
            if !bit_get(bitmap, i) {
                continue;
            }
            let (lo, hi) = (offset_at(k), offset_at(k + 1));
            let mut c = Cursor::new(&buf[lo..hi]);
            rec.set_shared(
                std::sync::Arc::clone(&self.names[i]),
                read_tlv_value(&mut c, 0)?,
            );
        }

        if header.flags & flags::HAS_OVERFLOW != 0 {
            let start = last_offset;
            let mut c = Cursor::new(&buf[start..]);
            let count = c.varint()? as usize;
            if count > c.remaining() {
                return Err(Error::Corruption(format!(
                    "overflow declares {count} fields but only {} bytes remain",
                    c.remaining()
                )));
            }
            for _ in 0..count {
                let klen = c.varint()? as usize;
                let name = self.intern(c.str_ref(klen)?);
                let v = read_tlv_value(&mut c, 0)?;
                rec.set_shared(name, v);
            }
        }

        Ok((header, rec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::schema::{FieldDef, SchemaMode};
    use adabt_testkit::rng::Rng;

    #[test]
    fn peek_field_matches_decode_for_every_field() {
        for schema in [fixed_schema(), strict_schema()] {
            let codec = RecordCodec::new(schema.clone());
            let rec = Record::new()
                .with("id", 42_u64)
                .with("balance", -7_i64)
                .with("active", true)
                .with("name", "ada")
                .with("bio", "analytical engine")
                .with("score", 1.5_f64)
                .with("tags", Value::List(vec![Value::Str("math".to_string())]));
            let buf = codec.encode(&rec).unwrap();
            for f in codec.schema.fields() {
                assert_eq!(
                    codec.peek_field(&buf, &f.name).unwrap(),
                    rec.get(&f.name).cloned(),
                    "{}: peek disagrees with decode",
                    f.name
                );
            }
            assert_eq!(codec.peek_field(&buf, "absent").unwrap(), None);
        }
    }

    #[test]
    fn peek_of_a_missing_optional_field_is_none_not_an_error() {
        let codec = RecordCodec::new(fixed_schema());
        let rec = Record::new().with("id", 1_u64).with("balance", 2_i64);
        let buf = codec.encode(&rec).unwrap();
        // `active` and `name` are declared but not present in this record.
        assert_eq!(codec.peek_field(&buf, "active").unwrap(), None);
        assert_eq!(codec.peek_field(&buf, "name").unwrap(), None);
        assert_eq!(codec.peek_field(&buf, "id").unwrap(), Some(Value::U64(1)));
    }

    #[test]
    fn dynamic_records_peek_by_in_record_name() {
        let codec = RecordCodec::new(Schema::dynamic());
        let rec = Record::new().with("kind", "sensor").with("reading", 9_i64);
        let buf = codec.encode(&rec).unwrap();
        assert_eq!(
            codec.peek_field(&buf, "reading").unwrap(),
            Some(Value::I64(9))
        );
        assert_eq!(
            codec.peek_field(&buf, "kind").unwrap(),
            Some(Value::Str("sensor".to_string()))
        );
        assert_eq!(codec.peek_field(&buf, "nope").unwrap(), None);
    }

    fn fixed_schema() -> Schema {
        Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("balance", FieldType::I64).required(),
                FieldDef::new("active", FieldType::Bool),
                FieldDef::new("name", FieldType::Char(32)),
            ],
        )
        .unwrap()
    }

    fn strict_schema() -> Schema {
        Schema::new(
            SchemaMode::Strict,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("bio", FieldType::Str { max_len: None }),
                FieldDef::new("score", FieldType::F64),
                FieldDef::new(
                    "tags",
                    FieldType::List(Box::new(FieldType::Str { max_len: None })),
                ),
            ],
        )
        .unwrap()
    }

    fn declared_schema() -> Schema {
        Schema::new(
            SchemaMode::Declared,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("note", FieldType::Str { max_len: None }),
            ],
        )
        .unwrap()
    }

    fn fixed_record() -> Record {
        Record::new()
            .with("id", 42u64)
            .with("balance", -1234i64)
            .with("active", true)
            .with("name", "Ada Lovelace")
    }

    fn strict_record() -> Record {
        Record::new()
            .with("id", 7u64)
            .with("bio", "a".repeat(300))
            .with("score", 1.5f64)
            .with(
                "tags",
                Value::List(vec![Value::Str("x".into()), Value::Str("y".into())]),
            )
    }

    fn roundtrip(schema: Schema, rec: &Record) -> Record {
        let c = RecordCodec::new(schema);
        let bytes = c.encode(rec).expect("encode");
        c.decode(&bytes).expect("decode")
    }

    #[test]
    fn fixed_mode_round_trips() {
        assert_eq!(roundtrip(fixed_schema(), &fixed_record()), fixed_record());
    }

    #[test]
    fn strict_mode_round_trips() {
        assert_eq!(
            roundtrip(strict_schema(), &strict_record()),
            strict_record()
        );
    }

    #[test]
    fn dynamic_mode_round_trips_nested_values() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "inner".to_string(),
            Value::List(vec![Value::I64(-5), Value::Null]),
        );
        let rec = Record::new()
            .with("a", 1i64)
            .with("b", Value::Map(m))
            .with("c", Value::Bytes(vec![0, 1, 2, 255]))
            .with("d", f64::NAN);
        assert_eq!(roundtrip(Schema::dynamic(), &rec), rec);
    }

    #[test]
    fn declared_mode_preserves_undeclared_fields() {
        let rec = Record::new()
            .with("id", 1u64)
            .with("note", "hi")
            .with("surprise", 99i64)
            .with("another", "extra");
        let got = roundtrip(declared_schema(), &rec);
        assert_eq!(got, rec);
        assert_eq!(got.get("surprise"), Some(&Value::I64(99)));
    }

    #[test]
    fn declared_mode_sets_the_overflow_flag_only_when_needed() {
        let c = RecordCodec::new(declared_schema());
        let plain = Record::new().with("id", 1u64);
        let extra = Record::new().with("id", 1u64).with("x", 1i64);
        let h = |r: &Record| RecordHeader::read(&c.encode(r).unwrap()).unwrap().flags;
        assert_eq!(h(&plain) & flags::HAS_OVERFLOW, 0);
        assert_ne!(h(&extra) & flags::HAS_OVERFLOW, 0);
    }

    #[test]
    fn strict_mode_drops_undeclared_fields_rather_than_storing_them() {
        // Schema validation rejects such a record before it reaches the codec;
        // if one arrives anyway the codec must not invent an overflow section.
        let rec = Record::new().with("id", 1u64).with("stowaway", 5i64);
        let got = roundtrip(strict_schema(), &rec);
        assert_eq!(got.get("stowaway"), None);
        assert_eq!(got.get("id"), Some(&Value::U64(1)));
    }

    #[test]
    fn fixed_records_all_have_the_same_physical_size() {
        let c = RecordCodec::new(fixed_schema());
        let stride = c.physical_record_size().expect("fixed schema has a stride");
        // header + bitmap(4 fields -> 1 byte) + 8 + 8 + 1 + 32
        assert_eq!(stride, HEADER_LEN as u32 + 1 + 49);
        for rec in [
            fixed_record(),
            Record::new().with("id", 0u64),
            Record::new()
                .with("id", u64::MAX)
                .with("balance", i64::MIN)
                .with("active", false)
                .with("name", "x".repeat(31)),
        ] {
            assert_eq!(
                c.encode(&rec).unwrap().len() as u32,
                stride,
                "record {rec:?} did not encode to the stride"
            );
        }
    }

    #[test]
    fn only_fixed_schemas_report_a_stride() {
        for s in [strict_schema(), declared_schema(), Schema::dynamic()] {
            assert_eq!(RecordCodec::new(s).physical_record_size(), None);
        }
    }

    #[test]
    fn a_trailing_nul_survives_a_fixed_char_slot() {
        // The whole reason fixed slots carry an inline length: zero-padding
        // would silently eat this, and space-padding would eat a trailing space.
        for s in ["ends with nul\0", "trailing space ", "\0\0\0", ""] {
            let rec = Record::new()
                .with("id", 1u64)
                .with("balance", 0i64)
                .with("name", s);
            let got = roundtrip(fixed_schema(), &rec);
            assert_eq!(
                got.get("name"),
                Some(&Value::Str(s.to_string())),
                "lost data for {s:?}"
            );
        }
    }

    #[test]
    fn an_absent_field_is_distinguishable_from_an_empty_one() {
        let c = RecordCodec::new(fixed_schema());
        let empty = Record::new()
            .with("id", 1u64)
            .with("balance", 0i64)
            .with("name", "");
        let absent = Record::new().with("id", 1u64).with("balance", 0i64);
        assert_eq!(
            c.decode(&c.encode(&empty).unwrap()).unwrap().get("name"),
            Some(&Value::Str(String::new()))
        );
        assert_eq!(
            c.decode(&c.encode(&absent).unwrap()).unwrap().get("name"),
            None
        );
    }

    #[test]
    fn a_null_value_decodes_as_absent_not_as_null() {
        // Presence is one bit, so an explicit null and a missing field are the
        // same on disk. The reference model must agree, hence the assertion.
        let c = RecordCodec::new(fixed_schema());
        let rec = Record::new()
            .with("id", 1u64)
            .with("balance", 0i64)
            .with("name", Value::Null);
        assert_eq!(
            c.decode(&c.encode(&rec).unwrap()).unwrap().get("name"),
            None
        );
    }

    #[test]
    fn the_mvcc_header_survives_a_round_trip() {
        let c = RecordCodec::new(fixed_schema());
        let header = RecordHeader {
            version: FORMAT_VERSION,
            flags: flags::DELETED,
            txn: TxnId(0xDEAD_BEEF_CAFE),
        };
        let mut buf = Vec::new();
        c.encode_with_header(&fixed_record(), header, &mut buf)
            .unwrap();
        let (got, rec) = c.decode_with_header(&buf).unwrap();
        assert_eq!(got.txn, TxnId(0xDEAD_BEEF_CAFE));
        assert!(got.is_deleted());
        assert_eq!(rec, fixed_record());
    }

    #[test]
    fn a_foreign_format_version_is_rejected() {
        let c = RecordCodec::new(fixed_schema());
        let mut buf = c.encode(&fixed_record()).unwrap();
        buf[0] = 99;
        assert!(matches!(c.decode(&buf), Err(Error::Corruption(_))));
    }

    #[test]
    fn extreme_numeric_values_survive() {
        let c = RecordCodec::new(fixed_schema());
        for (b, i) in [(i64::MIN, u64::MAX), (i64::MAX, 0), (0, 1)] {
            let rec = Record::new().with("id", i).with("balance", b);
            let got = c.decode(&c.encode(&rec).unwrap()).unwrap();
            assert_eq!(got.get("balance"), Some(&Value::I64(b)));
            assert_eq!(got.get("id"), Some(&Value::U64(i)));
        }
    }

    // -- corruption resistance --------------------------------------------

    fn corpus() -> Vec<(RecordCodec, Vec<u8>)> {
        vec![
            (RecordCodec::new(fixed_schema()), fixed_record()),
            (RecordCodec::new(strict_schema()), strict_record()),
            (
                RecordCodec::new(declared_schema()),
                Record::new()
                    .with("id", 1u64)
                    .with("note", "n")
                    .with("x", 2i64),
            ),
            (
                RecordCodec::new(Schema::dynamic()),
                Record::new()
                    .with("k", Value::List(vec![Value::Str("v".into())]))
                    .with("j", 1i64),
            ),
        ]
        .into_iter()
        .map(|(c, r)| {
            let b = c.encode(&r).unwrap();
            (c, b)
        })
        .collect()
    }

    #[test]
    fn truncation_at_any_length_is_an_error_never_a_panic() {
        for (c, bytes) in corpus() {
            for n in 0..bytes.len() {
                // Must not panic. A short read is corruption; anything else is
                // a bug, but a *crash* is unacceptable either way.
                let _ = c.decode(&bytes[..n]);
            }
            assert!(
                c.decode(&bytes).is_ok(),
                "the untruncated record must decode"
            );
        }
    }

    #[test]
    fn arbitrary_byte_corruption_never_panics() {
        let mut rng = Rng::new(0xC0FFEE);
        for (c, bytes) in corpus() {
            for _ in 0..4_000 {
                let mut b = bytes.clone();
                let flips = 1 + rng.below_usize(3);
                for _ in 0..flips {
                    let i = rng.below_usize(b.len());
                    b[i] ^= 1 << rng.below_usize(8);
                }
                // Either a clean error or some record; never a crash, never a
                // hang, never an out-of-memory allocation from a bogus count.
                let _ = c.decode(&b);
            }
        }
    }

    #[test]
    fn appended_garbage_does_not_corrupt_the_decoded_record() {
        for (c, bytes) in corpus() {
            let want = c.decode(&bytes).unwrap();
            let mut b = bytes.clone();
            b.extend_from_slice(&[0xff; 64]);
            // Trailing bytes belong to the page, not the record; a record must
            // decode from a buffer that is longer than itself.
            if let Ok(got) = c.decode(&b) {
                assert_eq!(got, want, "trailing bytes changed the decoded record");
            }
        }
    }

    #[test]
    fn deeply_nested_input_is_rejected_rather_than_overflowing_the_stack() {
        let mut buf = vec![FORMAT_VERSION, 0];
        buf.extend_from_slice(&0u64.to_le_bytes());
        varint::write_u64(1, &mut buf); // one field
        varint::write_u64(1, &mut buf); // name length
        buf.push(b'x');
        // A list containing a list containing a list, far past MAX_DEPTH.
        for _ in 0..MAX_DEPTH * 4 {
            buf.push(tag::LIST);
            varint::write_u64(1, &mut buf);
        }
        buf.push(tag::NULL);
        let c = RecordCodec::new(Schema::dynamic());
        assert!(matches!(c.decode(&buf), Err(Error::Corruption(_))));
    }

    #[test]
    fn a_huge_declared_count_does_not_allocate() {
        let mut buf = vec![FORMAT_VERSION, 0];
        buf.extend_from_slice(&0u64.to_le_bytes());
        varint::write_u64(u64::MAX / 2, &mut buf);
        let c = RecordCodec::new(Schema::dynamic());
        assert!(matches!(c.decode(&buf), Err(Error::Corruption(_))));
    }

    #[test]
    fn a_non_monotonic_offset_table_is_rejected() {
        let c = RecordCodec::new(strict_schema());
        let mut b = c.encode(&strict_record()).unwrap();
        let n = c.schema().fields().len();
        let table_at = HEADER_LEN + bitmap_len(n) + c.fixed_region_len as usize;
        // Point the first variable field past the second.
        b[table_at..table_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(c.decode(&b), Err(Error::Corruption(_))));
    }
}

/// `schema_editable_in_place` makes a claim about bytes, not about types, so
/// this checks it the same way: encode under one schema, decode under
/// another, on every path the function allows and several it must not.
///
/// This suite exists because the function's first version was wrong — it
/// allowed a fixed-width append for `Strict`/`Declared` on an argument that
/// sounded right (the new field is presence-gated) and missed that the
/// *offset table's own position* moves with `fixed_region_len` regardless.
/// Every branch below is the direct antidote: write real old-format bytes,
/// decode them with a real new-format codec, and check the record, not the
/// reasoning.
#[cfg(test)]
mod in_place_eligibility {
    use super::*;
    use adabt_core::schema::FieldDef;

    fn f(name: &str, ty: FieldType) -> FieldDef {
        FieldDef::new(name, ty)
    }
    fn req(name: &str, ty: FieldType) -> FieldDef {
        FieldDef::new(name, ty).required()
    }

    fn schema(mode: SchemaMode, fields: Vec<FieldDef>) -> Schema {
        Schema::new(mode, fields).unwrap()
    }

    /// Encode `rec` under `old`, decode it under `new`, and assert the two
    /// codecs agree on every field `new` still declares — the empirical
    /// half of what `schema_editable_in_place` promises.
    fn assert_reads_back(old: &Schema, new: &Schema, rec: &Record) {
        assert!(
            schema_editable_in_place(old, new),
            "test setup expected this pair to be eligible"
        );
        let old_codec = RecordCodec::new(old.clone());
        let new_codec = RecordCodec::new(new.clone());
        let bytes = old_codec.encode(rec).unwrap();
        let got = new_codec
            .decode(&bytes)
            .unwrap_or_else(|e| panic!("old bytes did not decode under the new schema: {e}"));
        for field in new.fields() {
            assert_eq!(
                got.get(&field.name),
                rec.get(&field.name),
                "field {} mismatched after decoding old bytes under the new schema",
                field.name
            );
        }
    }

    #[test]
    fn fixed_mode_append_reads_old_rows_with_the_new_field_absent() {
        let old = schema(
            SchemaMode::Fixed,
            vec![req("a", FieldType::I64), req("b", FieldType::I64)],
        );
        let new = schema(
            SchemaMode::Fixed,
            vec![
                req("a", FieldType::I64),
                req("b", FieldType::I64),
                f("c", FieldType::I64),
            ],
        );
        let rec = Record::new().with("a", 1i64).with("b", 2i64);
        assert_reads_back(&old, &new, &rec);
        assert_eq!(
            RecordCodec::new(new.clone())
                .decode(&RecordCodec::new(old).encode(&rec).unwrap())
                .unwrap()
                .get("c"),
            None
        );
    }

    #[test]
    fn fixed_mode_drop_last_reads_old_rows_with_the_field_ignored() {
        let old = schema(
            SchemaMode::Fixed,
            vec![
                req("a", FieldType::I64),
                req("b", FieldType::I64),
                req("c", FieldType::I64),
            ],
        );
        let new = schema(
            SchemaMode::Fixed,
            vec![req("a", FieldType::I64), req("b", FieldType::I64)],
        );
        let rec = Record::new()
            .with("a", 1i64)
            .with("b", 2i64)
            .with("c", 3i64);
        assert_reads_back(&old, &new, &rec);
    }

    #[test]
    fn strict_mode_drop_last_variable_field_reads_old_rows() {
        let old = schema(
            SchemaMode::Strict,
            vec![
                req("a", FieldType::I64),
                f("note", FieldType::Str { max_len: None }),
            ],
        );
        let new = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        let rec = Record::new().with("a", 1i64).with("note", "hello");
        assert_reads_back(&old, &new, &rec);
    }

    #[test]
    fn strict_mode_never_allows_a_fixed_width_append() {
        let old = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        let new = schema(
            SchemaMode::Strict,
            vec![req("a", FieldType::I64), f("b", FieldType::I64)],
        );
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn strict_mode_never_allows_a_variable_width_append() {
        let old = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        let new = schema(
            SchemaMode::Strict,
            vec![
                req("a", FieldType::I64),
                f("b", FieldType::Str { max_len: None }),
            ],
        );
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn strict_mode_never_allows_dropping_a_trailing_fixed_field() {
        // The mirror bug of the append case: dropping the last field also
        // moves `table_at` when that field is fixed-width, because
        // `fixed_region_len` shrinks by exactly its width.
        let old = schema(
            SchemaMode::Strict,
            vec![req("a", FieldType::I64), f("b", FieldType::I64)],
        );
        let new = schema(SchemaMode::Strict, vec![req("a", FieldType::I64)]);
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn declared_mode_never_takes_the_in_place_path() {
        let old = schema(
            SchemaMode::Declared,
            vec![
                req("a", FieldType::I64),
                f("note", FieldType::Str { max_len: None }),
            ],
        );
        let append = schema(
            SchemaMode::Declared,
            vec![
                req("a", FieldType::I64),
                f("note", FieldType::Str { max_len: None }),
                f("b", FieldType::I64),
            ],
        );
        let drop = schema(SchemaMode::Declared, vec![req("a", FieldType::I64)]);
        assert!(!schema_editable_in_place(&old, &append));
        assert!(!schema_editable_in_place(&old, &drop));
    }

    #[test]
    fn dynamic_mode_never_takes_the_in_place_path() {
        assert!(!schema_editable_in_place(
            &Schema::dynamic(),
            &Schema::dynamic()
        ));
    }

    #[test]
    fn crossing_a_bitmap_byte_boundary_is_rejected_even_though_it_is_an_append() {
        let fields: Vec<FieldDef> = (0..8)
            .map(|i| req(&format!("f{i}"), FieldType::I64))
            .collect();
        let old = schema(SchemaMode::Fixed, fields.clone());
        let mut nine = fields;
        nine.push(f("f8", FieldType::I64));
        let new = schema(SchemaMode::Fixed, nine);
        assert!(!schema_editable_in_place(&old, &new));
    }

    #[test]
    fn appending_a_required_field_is_rejected_regardless_of_layout_safety() {
        let old = schema(SchemaMode::Fixed, vec![req("a", FieldType::I64)]);
        let new = schema(
            SchemaMode::Fixed,
            vec![req("a", FieldType::I64), req("b", FieldType::I64)],
        );
        assert!(!schema_editable_in_place(&old, &new));
    }
}
