//! Schema serialisation.
//!
//! A schema has to survive a restart, and it has to survive it *exactly*: the
//! physical layout of every record in a collection is derived from it, so a
//! schema that reloads even slightly differently silently reinterprets data
//! already on disk.

use adabt_core::error::{Error, Result};
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};

use crate::varint;

const V_BOOL: u8 = 0;
const V_I64: u8 = 1;
const V_U64: u8 = 2;
const V_F64: u8 = 3;
const V_CHAR: u8 = 4;
const V_FIXED_BYTES: u8 = 5;
const V_STR: u8 = 6;
const V_BYTES: u8 = 7;
const V_LIST: u8 = 8;
const V_MAP: u8 = 9;
const V_ANY: u8 = 10;
const V_DECIMAL: u8 = 11;
const V_TIMESTAMP: u8 = 12;

/// Guards against a corrupt `List` chain recursing without bound.
const MAX_TYPE_DEPTH: u32 = 32;

fn write_type(ty: &FieldType, out: &mut Vec<u8>) {
    match ty {
        FieldType::Bool => out.push(V_BOOL),
        FieldType::I64 => out.push(V_I64),
        FieldType::U64 => out.push(V_U64),
        FieldType::F64 => out.push(V_F64),
        FieldType::Timestamp => out.push(V_TIMESTAMP),
        FieldType::Decimal { scale } => {
            out.push(V_DECIMAL);
            out.push(*scale);
        }
        FieldType::Char(n) => {
            out.push(V_CHAR);
            varint::write_u64(*n as u64, out);
        }
        FieldType::FixedBytes(n) => {
            out.push(V_FIXED_BYTES);
            varint::write_u64(*n as u64, out);
        }
        FieldType::Str { max_len } => {
            out.push(V_STR);
            write_opt_u32(*max_len, out);
        }
        FieldType::Bytes { max_len } => {
            out.push(V_BYTES);
            write_opt_u32(*max_len, out);
        }
        FieldType::List(inner) => {
            out.push(V_LIST);
            write_type(inner, out);
        }
        FieldType::Map => out.push(V_MAP),
        FieldType::Any => out.push(V_ANY),
    }
}

fn write_opt_u32(v: Option<u32>, out: &mut Vec<u8>) {
    match v {
        None => out.push(0),
        Some(n) => {
            out.push(1);
            varint::write_u64(n as u64, out);
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8> {
        let b = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| Error::Corruption("truncated schema".into()))?;
        self.pos += 1;
        Ok(b)
    }
    fn varint(&mut self) -> Result<u64> {
        let (v, used) = varint::read_u64(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(v)
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| Error::Corruption("truncated schema".into()))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn opt_u32(&mut self) -> Result<Option<u32>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.varint()? as u32)),
            other => Err(Error::Corruption(format!(
                "invalid optional marker {other} in schema"
            ))),
        }
    }
    fn ty(&mut self, depth: u32) -> Result<FieldType> {
        if depth > MAX_TYPE_DEPTH {
            return Err(Error::Corruption("schema type nested too deeply".into()));
        }
        Ok(match self.u8()? {
            V_BOOL => FieldType::Bool,
            V_I64 => FieldType::I64,
            V_U64 => FieldType::U64,
            V_F64 => FieldType::F64,
            V_TIMESTAMP => FieldType::Timestamp,
            V_DECIMAL => FieldType::Decimal { scale: self.u8()? },
            V_CHAR => FieldType::Char(self.varint()? as u32),
            V_FIXED_BYTES => FieldType::FixedBytes(self.varint()? as u32),
            V_STR => FieldType::Str {
                max_len: self.opt_u32()?,
            },
            V_BYTES => FieldType::Bytes {
                max_len: self.opt_u32()?,
            },
            V_LIST => FieldType::List(Box::new(self.ty(depth + 1)?)),
            V_MAP => FieldType::Map,
            V_ANY => FieldType::Any,
            other => {
                return Err(Error::Corruption(format!(
                    "unknown field type tag {other} in schema"
                )))
            }
        })
    }
}

pub fn encode_schema(schema: &Schema) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(match schema.mode() {
        SchemaMode::Dynamic => 0,
        SchemaMode::Declared => 1,
        SchemaMode::Strict => 2,
        SchemaMode::Fixed => 3,
    });
    varint::write_u64(schema.fields().len() as u64, &mut out);
    for f in schema.fields() {
        varint::write_u64(f.name.len() as u64, &mut out);
        out.extend_from_slice(f.name.as_bytes());
        out.push(f.nullable as u8);
        write_type(&f.ty, &mut out);
    }
    out
}

pub fn decode_schema(buf: &[u8]) -> Result<Schema> {
    let mut r = Reader { buf, pos: 0 };
    let mode = match r.u8()? {
        0 => SchemaMode::Dynamic,
        1 => SchemaMode::Declared,
        2 => SchemaMode::Strict,
        3 => SchemaMode::Fixed,
        other => return Err(Error::Corruption(format!("unknown schema mode {other}"))),
    };
    let n = r.varint()? as usize;
    if n > buf.len() {
        return Err(Error::Corruption(format!(
            "schema declares {n} fields but is only {} bytes",
            buf.len()
        )));
    }
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        let name_len = r.varint()? as usize;
        let name = std::str::from_utf8(r.bytes(name_len)?)
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in field name: {e}")))?
            .to_string();
        let nullable = r.u8()? != 0;
        let ty = r.ty(0)?;
        let mut f = FieldDef::new(name, ty);
        f.nullable = nullable;
        fields.push(f);
    }
    if mode == SchemaMode::Dynamic && fields.is_empty() {
        return Ok(Schema::dynamic());
    }
    Schema::new(mode, fields).map_err(Error::Schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cases() -> Vec<Schema> {
        vec![
            Schema::dynamic(),
            Schema::new(
                SchemaMode::Fixed,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    FieldDef::new("balance", FieldType::I64),
                    FieldDef::new("active", FieldType::Bool),
                    FieldDef::new("name", FieldType::Char(32)),
                    FieldDef::new("blob", FieldType::FixedBytes(300)),
                ],
            )
            .unwrap(),
            Schema::new(
                SchemaMode::Strict,
                vec![
                    FieldDef::new("s", FieldType::Str { max_len: Some(64) }),
                    FieldDef::new("b", FieldType::Bytes { max_len: None }),
                    FieldDef::new(
                        "nested",
                        FieldType::List(Box::new(FieldType::List(Box::new(FieldType::F64)))),
                    ),
                    FieldDef::new("m", FieldType::Map),
                ],
            )
            .unwrap(),
            Schema::new(
                SchemaMode::Declared,
                vec![FieldDef::new("anything", FieldType::Any)],
            )
            .unwrap(),
        ]
    }

    #[test]
    fn schemas_round_trip_exactly() {
        for s in cases() {
            let got = decode_schema(&encode_schema(&s)).unwrap();
            assert_eq!(got, s);
            // Layout-critical derived values must match too, not just the
            // struct: these are what the codec computes offsets from.
            assert_eq!(got.fixed_record_size(), s.fixed_record_size());
            assert_eq!(got.mode(), s.mode());
        }
    }

    #[test]
    fn nullability_survives() {
        let s = Schema::new(
            SchemaMode::Strict,
            vec![
                FieldDef::new("req", FieldType::I64).required(),
                FieldDef::new("opt", FieldType::I64),
            ],
        )
        .unwrap();
        let got = decode_schema(&encode_schema(&s)).unwrap();
        assert!(!got.field("req").unwrap().nullable);
        assert!(got.field("opt").unwrap().nullable);
    }

    #[test]
    fn truncation_is_an_error_never_a_panic() {
        for s in cases() {
            let b = encode_schema(&s);
            for n in 0..b.len() {
                let _ = decode_schema(&b[..n]);
            }
        }
    }

    #[test]
    fn corrupt_input_never_panics() {
        use adabt_testkit::rng::Rng;
        let mut rng = Rng::new(0x05C4_E11A);
        for s in cases() {
            let b = encode_schema(&s);
            for _ in 0..3_000 {
                let mut c = b.clone();
                let i = rng.below_usize(c.len());
                c[i] ^= 1 << rng.below_usize(8);
                let _ = decode_schema(&c);
            }
        }
    }

    #[test]
    fn an_unknown_type_tag_is_rejected() {
        let s = Schema::new(SchemaMode::Strict, vec![FieldDef::new("x", FieldType::I64)]).unwrap();
        let mut b = encode_schema(&s);
        *b.last_mut().unwrap() = 200;
        assert!(decode_schema(&b).is_err());
    }

    #[test]
    fn a_deeply_nested_list_chain_is_rejected() {
        let mut b = vec![2u8];
        varint::write_u64(1, &mut b);
        varint::write_u64(1, &mut b);
        b.push(b'x');
        b.push(1);
        b.extend(std::iter::repeat_n(V_LIST, (MAX_TYPE_DEPTH * 3) as usize));
        b.push(V_I64);
        assert!(decode_schema(&b).is_err());
    }
}
