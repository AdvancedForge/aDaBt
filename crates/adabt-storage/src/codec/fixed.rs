//! Fixed-width slots: the constant-size field encoding that gives
//! `Strict` and `Fixed` records O(1) field offsets.

use adabt_core::error::{Error, Result};
use adabt_core::schema::FieldType;
use adabt_core::value::{rescale, Value};

use super::cursor::Cursor;

// -- fixed-width slots ----------------------------------------------------

/// Write `v` into exactly `width` bytes at the end of `out`.
///
/// Text and byte fields store their length inline, then their content, then
/// zero padding. That is what makes the slot lossless: padding schemes lose
/// trailing NULs or trailing spaces, and a fixed layout that silently corrupts
/// its own data is worse than no fixed layout at all.
pub(super) fn write_fixed_slot(
    ty: &FieldType,
    v: &Value,
    width: u32,
    out: &mut Vec<u8>,
) -> Result<()> {
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
