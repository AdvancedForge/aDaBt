//! Reading records back: the full decode, and the field-level peeks that
//! skip every byte the caller did not ask for.

use adabt_core::error::{Error, Result};
use adabt_core::record::Record;
use adabt_core::schema::SchemaMode;
use adabt_core::value::Value;

use super::cursor::Cursor;
use super::tlv::read_tlv_value;
use super::{bit_get, bitmap_len, flags, read_fixed_field, RecordCodec, RecordHeader, HEADER_LEN};

impl RecordCodec {
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

    /// Decode exactly one field of an encoded record, borrowing the buffer.
    ///
    /// This is the seed of the borrowed record view the zero-copy read path
    /// called for: a fetch that needs two of twenty fields should not pay for a
    /// `Record` — the map, the name refcounts, the reserve — to throw the other
    /// eighteen away. It walks only what the requested field needs: the header,
    /// its presence bit, and (for variable fields) one offset-table entry.
    /// Everything else in the buffer is untouched bytes.
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

    /// Decode only the listed fields, skipping every other field's bytes.
    ///
    /// For a wide record this is the difference between one field's cost and
    /// one record's cost: the bitmap and offset table are read once, then
    /// only the TLVs for the requested fields are decoded. Absent fields are
    /// simply not inserted.
    pub fn peek_fields(&self, buf: &[u8], fields: &[&str]) -> Result<Record> {
        if fields.is_empty() {
            return Ok(Record::new());
        }
        let mut out = Record::new();
        for f in fields {
            if let Some(v) = self.peek_field(buf, f)? {
                // Use interned handle where possible to avoid per-row allocation.
                let idx = self.schema.fields().iter().position(|ff| &ff.name == f);
                if let Some(i) = idx {
                    out.set_shared(std::sync::Arc::clone(&self.names[i]), v);
                } else {
                    // Dynamic field not in schema — intern through the map.
                    out.set_shared(self.intern(f), v);
                }
            }
        }
        Ok(out)
    }
}
