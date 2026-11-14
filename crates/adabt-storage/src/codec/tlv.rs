//! Tag-length-value encoding of single values: self-describing, so it reads
//! back with no schema. Variable-width fields, `Dynamic` records and index
//! keys all use it.

use adabt_core::error::{Error, Result};
use adabt_core::value::Value;

use super::cursor::Cursor;
use crate::varint;

// -- zigzag ---------------------------------------------------------------

#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub(super) fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

// -- tag-length-value -----------------------------------------------------

pub(super) mod tag {
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

/// Maximum nesting depth accepted when decoding.
///
/// Without it, a corrupt buffer describing deeply nested lists would recurse
/// until the stack overflows, which is a crash rather than an error.
pub(super) const MAX_DEPTH: u32 = 64;

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

pub(super) fn write_tlv_value(v: &Value, out: &mut Vec<u8>) {
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

pub(super) fn read_tlv_value(c: &mut Cursor<'_>, depth: u32) -> Result<Value> {
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
