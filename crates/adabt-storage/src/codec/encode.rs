//! Writing a record in its schema mode's layout: tag-length-value for
//! `Dynamic`, and bitmap, fixed region, offset table and overflow bag for the
//! declared modes.

use adabt_core::error::Result;
use adabt_core::record::Record;
use adabt_core::schema::SchemaMode;
use adabt_core::value::Value;

use super::fixed::write_fixed_slot;
use super::tlv::write_tlv_value;
use super::{bit_set, bitmap_len, flags, RecordCodec, RecordHeader};
use crate::varint;

impl RecordCodec {
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
}
