//! Borrowed decoding: [`ValueRef`] reads the same tag-length-value bytes as
//! the owned path, but keeps text, bytes and map keys as slices of the source
//! buffer.

use adabt_core::error::{Error, Result};
use adabt_core::value::Value;
use std::collections::BTreeMap;

use super::cursor::Cursor;
use super::tlv::{tag, unzigzag, MAX_DEPTH};

/// A decoded value that borrows its text, bytes and map keys from the source
/// buffer instead of copying them into `String`s and `Vec`s.
///
/// This is the borrowed-batch primitive: scanning N rows to look at two text
/// fields currently allocates 2N strings that mostly get dropped again. A
/// [`ValueRef`] pays nothing until [`ValueRef::into_value`] crosses a query
/// boundary — scalars are copied either way (they are the width of a pointer),
/// while the expensive variants stay pointers into memory the caller already
/// owns. The encoding is identical to the owned path's, so the same bytes
/// decode through either and tests hold them answer-for-answer equal.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueRef<'a> {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Decimal { units: i128, scale: u8 },
    Timestamp(i64),
    Str(&'a str),
    Bytes(&'a [u8]),
    List(Vec<ValueRef<'a>>),
    Map(BTreeMap<&'a str, ValueRef<'a>>),
}

impl<'a> ValueRef<'a> {
    /// Take ownership: this is where text and bytes copy, and not before.
    pub fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(b),
            Self::I64(n) => Value::I64(n),
            Self::U64(n) => Value::U64(n),
            Self::F64(f) => Value::F64(f),
            Self::Decimal { units, scale } => Value::Decimal { units, scale },
            Self::Timestamp(t) => Value::Timestamp(t),
            Self::Str(s) => Value::Str(s.to_string()),
            Self::Bytes(b) => Value::Bytes(b.to_vec()),
            Self::List(items) => Value::List(items.into_iter().map(|v| v.into_value()).collect()),
            Self::Map(m) => Value::Map(
                m.into_iter()
                    .map(|(k, v)| (k.to_string(), v.into_value()))
                    .collect(),
            ),
        }
    }
}

/// Compare a borrowed value against an owned one without upgrading either
/// side: text and bytes are compared in place, so a probe against N stored
/// keys costs N comparisons and zero allocations. The mirror impl lets the
/// arguments come in either order.
impl PartialEq<Value> for ValueRef<'_> {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Self::Null, Value::Null) => true,
            (Self::Bool(a), Value::Bool(b)) => a == b,
            (Self::I64(a), Value::I64(b)) => a == b,
            (Self::U64(a), Value::U64(b)) => a == b,
            (Self::F64(a), Value::F64(b)) => a == b,
            (
                Self::Decimal {
                    units: ua,
                    scale: sa,
                },
                Value::Decimal {
                    units: ub,
                    scale: sb,
                },
            ) => ua == ub && sa == sb,
            (Self::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Self::Str(a), Value::Str(b)) => a == b,
            (Self::Bytes(a), Value::Bytes(b)) => a == b,
            (Self::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
            }
            (Self::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va == vb)
            }
            _ => false,
        }
    }
}

impl PartialEq<ValueRef<'_>> for Value {
    fn eq(&self, other: &ValueRef<'_>) -> bool {
        other == self
    }
}

fn read_tlv_ref<'a>(c: &mut Cursor<'a>, depth: u32) -> Result<ValueRef<'a>> {
    if depth > MAX_DEPTH {
        return Err(Error::Corruption(format!(
            "record nesting deeper than {MAX_DEPTH}"
        )));
    }
    let t = c.u8()?;
    Ok(match t {
        tag::NULL => ValueRef::Null,
        tag::FALSE => ValueRef::Bool(false),
        tag::TRUE => ValueRef::Bool(true),
        tag::I64 => ValueRef::I64(unzigzag(c.varint()?)),
        tag::U64 => ValueRef::U64(c.varint()?),
        tag::F64 => ValueRef::F64(f64::from_bits(c.u64()?)),
        tag::STR => {
            let n = c.varint()? as usize;
            let raw = c.take(n)?;
            ValueRef::Str(
                std::str::from_utf8(raw)
                    .map_err(|e| Error::Corruption(format!("record holds invalid UTF-8: {e}")))?,
            )
        }
        tag::BYTES => {
            let n = c.varint()? as usize;
            ValueRef::Bytes(c.take(n)?)
        }
        tag::LIST => {
            let n = c.varint()? as usize;
            if n > c.remaining() {
                return Err(Error::Corruption(format!(
                    "list of {n} items exceeds {} remaining bytes",
                    c.remaining()
                )));
            }
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(read_tlv_ref(c, depth + 1)?);
            }
            ValueRef::List(items)
        }
        tag::MAP => {
            let n = c.varint()? as usize;
            if n > c.remaining() {
                return Err(Error::Corruption(format!(
                    "map of {n} entries exceeds {} remaining bytes",
                    c.remaining()
                )));
            }
            let mut m = BTreeMap::new();
            for _ in 0..n {
                let klen = c.varint()? as usize;
                let kraw = c.take(klen)?;
                let k = std::str::from_utf8(kraw).map_err(|e| {
                    Error::Corruption(format!("record map key is invalid UTF-8: {e}"))
                })?;
                m.insert(k, read_tlv_ref(c, depth + 1)?);
            }
            ValueRef::Map(m)
        }
        tag::DECIMAL => {
            let scale = c.u8()?;
            let mut b = [0u8; 16];
            b.copy_from_slice(c.take(16)?);
            ValueRef::Decimal {
                units: i128::from_le_bytes(b),
                scale,
            }
        }
        tag::TIMESTAMP => ValueRef::Timestamp(c.u64()? as i64),
        other => {
            return Err(Error::Corruption(format!(
                "unknown value tag {other} in record"
            )))
        }
    })
}

/// Decode one value as a [`ValueRef`], borrowing from `buf`.
///
/// Consumes exactly the bytes [`decode_value`](super::decode_value) would,
/// so the two are freely interchangeable on the wire.
pub fn decode_value_ref(buf: &[u8]) -> Result<(ValueRef<'_>, usize)> {
    let mut c = Cursor::new(buf);
    let v = read_tlv_ref(&mut c, 0)?;
    Ok((v, c.pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_value, encode_value};

    /// The borrowed and owned decoders must agree on every variant — same
    /// bytes, same answer, byte for byte.
    #[test]
    fn the_borrowed_decoder_matches_the_owned_one_everywhere() {
        let mut samples: Vec<Value> = vec![
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::I64(-9_000_000_000_000),
            Value::U64(u64::MAX - 3),
            Value::F64(std::f64::consts::PI),
            Value::Decimal {
                units: -12_345,
                scale: 3,
            },
            Value::Timestamp(1_700_000_000_000_000_000),
            Value::Str("hello, borrowed world".into()),
            Value::Bytes(vec![0u8, 1, 250, 251]),
            Value::List(vec![Value::I64(1), Value::Str("two".into())]),
        ];
        let mut map = std::collections::BTreeMap::new();
        map.insert("k".to_string(), Value::Str("v".into()));
        map.insert("nested".to_string(), Value::List(vec![Value::U64(7)]));
        samples.push(Value::Map(map));

        for v in &samples {
            let mut enc = Vec::new();
            encode_value(v, &mut enc);
            let (owned, n_owned) = decode_value(&enc).unwrap();
            assert_eq!((owned, n_owned), {
                let (r, n_ref) = decode_value_ref(&enc).unwrap();
                (r.into_value(), n_ref)
            });
        }
    }

    /// The whole point: text stays a pointer into the caller's buffer until
    /// `into_value` says otherwise.
    #[test]
    fn borrowed_text_points_into_the_source_not_a_copy() {
        let payload = b"\x06\x05hello";
        let (r, used) = decode_value_ref(payload).unwrap();
        assert_eq!(used, payload.len());
        match r {
            ValueRef::Str(s) => {
                assert_eq!(s, "hello");
                // Same address as the source buffer: nothing was copied.
                assert_eq!(s.as_ptr(), payload[2..].as_ptr());
            }
            other => panic!("expected a borrowed str, got {other:?}"),
        }
    }

    /// Equality without upgrade: a borrowed value answers a probe against an
    /// owned one — in both argument orders — with no ownership taken.
    #[test]
    fn borrowed_values_answer_owned_probes_without_upgrading() {
        let payload = b"\x06\x05hello";
        let (r, _) = decode_value_ref(payload).unwrap();
        assert_eq!(r, Value::Str("hello".into()));
        assert_eq!(Value::Str("hello".into()), r);
        assert_ne!(r, Value::Str("goodbye".into()));
        // Cross-variant never equals, same as the owned world.
        assert_ne!(r, Value::Bytes(b"hello".to_vec()));
    }
}
