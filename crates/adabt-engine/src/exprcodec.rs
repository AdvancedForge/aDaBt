//! A binary form for `Expr`.
//!
//! # Why this exists
//!
//! A partial index is defined by a condition, and that condition has to
//! survive a restart. Nothing else in this project needed to persist an
//! expression before: predicates arrived with a query, were used, and were
//! thrown away.
//!
//! The alternatives were worse in ways worth recording, because each is a bug
//! this project has already shipped once in a different place:
//!
//! - **Store the `Debug` text.** Human-readable, and unparseable. On restart
//!   the index would come back with no condition — which means it would come
//!   back as a *full* index over the same field, holding a subset of the rows
//!   and claiming to hold all of them. That is not a slow answer, it is a
//!   wrong one, and it is the composite-index restore bug with worse
//!   consequences.
//! - **Store it as SQL and reparse.** Tempting, since `adabt_ir::sql` already
//!   parses `WHERE` clauses. But that parser deliberately refuses arithmetic,
//!   so the set of conditions that round-trip would be a subset of the ones
//!   that can be *built* — a silent cliff between what the API accepts and
//!   what survives a restart.
//! - **Drop partial indexes on restart.** Safe, and the shipped-but-
//!   unreachable pattern in yet another costume.
//!
//! So: a total encoding, with a decoder that refuses malformed input rather
//! than guessing. Values reuse the TLV encoding already used for records, so
//! there is one representation of a `Value` on disk and not two.
//!
//! # Format
//!
//! One tag byte per node, then the node's operands. Recursive, and bounded by
//! [`MAX_DEPTH`] on the way in *and* on the way out — the decoder's bound is
//! the one that matters, since the bytes may not have come from the encoder.

use adabt_core::error::{Error, Result};
use adabt_core::value::ArithOp;
use adabt_ir::{CmpOp, Expr};

/// Deepest expression that will encode or decode.
///
/// Same value and same reasoning as the record codec's limit: `Expr` is a
/// recursive `Box` type, and a decoder that followed untrusted nesting to its
/// end would overflow the stack rather than return an error.
pub const MAX_DEPTH: u32 = 32;

mod tag {
    pub const LITERAL: u8 = 1;
    pub const FIELD: u8 = 2;
    pub const COMPARE: u8 = 3;
    pub const AND: u8 = 4;
    pub const OR: u8 = 5;
    pub const NOT: u8 = 6;
    pub const IS_NULL: u8 = 7;
    pub const IS_NOT_NULL: u8 = 8;
    pub const ARITH: u8 = 9;
    pub const IN: u8 = 10;
    pub const LIKE: u8 = 11;
}

fn cmp_bits(op: CmpOp) -> u8 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
        CmpOp::Gt => 4,
        CmpOp::Ge => 5,
    }
}

fn cmp_from(b: u8) -> Result<CmpOp> {
    Ok(match b {
        0 => CmpOp::Eq,
        1 => CmpOp::Ne,
        2 => CmpOp::Lt,
        3 => CmpOp::Le,
        4 => CmpOp::Gt,
        5 => CmpOp::Ge,
        other => return Err(Error::Corruption(format!("unknown comparison op {other}"))),
    })
}

fn arith_bits(op: ArithOp) -> u8 {
    match op {
        ArithOp::Add => 0,
        ArithOp::Sub => 1,
        ArithOp::Mul => 2,
        ArithOp::Div => 3,
    }
}

fn arith_from(b: u8) -> Result<ArithOp> {
    Ok(match b {
        0 => ArithOp::Add,
        1 => ArithOp::Sub,
        2 => ArithOp::Mul,
        3 => ArithOp::Div,
        other => return Err(Error::Corruption(format!("unknown arithmetic op {other}"))),
    })
}

fn put_str(s: &str, out: &mut Vec<u8>) {
    adabt_storage::varint::write_u64(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

/// Encode `e`. Fails only on nesting deeper than [`MAX_DEPTH`].
pub fn encode_expr(e: &Expr) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    put(e, 0, &mut out)?;
    Ok(out)
}

fn put(e: &Expr, depth: u32, out: &mut Vec<u8>) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::InvalidOptimization(format!(
            "condition nested deeper than {MAX_DEPTH}"
        )));
    }
    match e {
        Expr::Literal(v) => {
            out.push(tag::LITERAL);
            adabt_storage::codec::encode_value(v, out);
        }
        Expr::Field(name) => {
            out.push(tag::FIELD);
            put_str(name, out);
        }
        Expr::Compare { op, lhs, rhs } => {
            out.push(tag::COMPARE);
            out.push(cmp_bits(*op));
            put(lhs, depth + 1, out)?;
            put(rhs, depth + 1, out)?;
        }
        Expr::And(parts) | Expr::Or(parts) => {
            out.push(if matches!(e, Expr::And(_)) {
                tag::AND
            } else {
                tag::OR
            });
            adabt_storage::varint::write_u64(parts.len() as u64, out);
            for p in parts {
                put(p, depth + 1, out)?;
            }
        }
        Expr::Not(inner) => {
            out.push(tag::NOT);
            put(inner, depth + 1, out)?;
        }
        Expr::IsNull(inner) => {
            out.push(tag::IS_NULL);
            put(inner, depth + 1, out)?;
        }
        Expr::IsNotNull(inner) => {
            out.push(tag::IS_NOT_NULL);
            put(inner, depth + 1, out)?;
        }
        Expr::Arith { op, lhs, rhs } => {
            out.push(tag::ARITH);
            out.push(arith_bits(*op));
            put(lhs, depth + 1, out)?;
            put(rhs, depth + 1, out)?;
        }
        Expr::In { needle, list } => {
            out.push(tag::IN);
            put(needle, depth + 1, out)?;
            adabt_storage::varint::write_u64(list.len() as u64, out);
            for item in list {
                put(item, depth + 1, out)?;
            }
        }
        Expr::Like { text, pattern } => {
            out.push(tag::LIKE);
            put(text, depth + 1, out)?;
            put_str(pattern, out);
        }
    }
    Ok(())
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
            .ok_or_else(|| Error::Corruption("condition truncated".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64> {
        let (v, n) = adabt_storage::varint::read_u64(&self.buf[self.pos..])
            .map_err(|_| Error::Corruption("condition truncated in a length".into()))?;
        self.pos += n;
        Ok(v)
    }

    fn str(&mut self) -> Result<String> {
        let n = self.varint()? as usize;
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| Error::Corruption("condition truncated in a name".into()))?;
        let s = std::str::from_utf8(&self.buf[self.pos..end])
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in condition: {e}")))?
            .to_string();
        self.pos = end;
        Ok(s)
    }

    /// How many more nodes could possibly remain.
    ///
    /// Used to reject a declared child count larger than the bytes left could
    /// hold, before allocating for it. Every node costs at least one byte, so
    /// the remaining length is a sound bound and a hostile length prefix
    /// cannot make this allocate gigabytes.
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
}

/// Decode an expression, refusing malformed or over-deep input.
pub fn decode_expr(buf: &[u8]) -> Result<Expr> {
    let mut r = Reader { buf, pos: 0 };
    let e = get(&mut r, 0)?;
    if r.pos != buf.len() {
        return Err(Error::Corruption(format!(
            "condition has {} trailing bytes",
            buf.len() - r.pos
        )));
    }
    Ok(e)
}

fn get(r: &mut Reader<'_>, depth: u32) -> Result<Expr> {
    if depth > MAX_DEPTH {
        return Err(Error::Corruption(format!(
            "condition nested deeper than {MAX_DEPTH}"
        )));
    }
    let t = r.u8()?;
    Ok(match t {
        tag::LITERAL => {
            let (v, n) = adabt_storage::codec::decode_value(&r.buf[r.pos..])?;
            r.pos += n;
            Expr::Literal(v)
        }
        tag::FIELD => Expr::Field(r.str()?),
        tag::COMPARE => {
            let op = cmp_from(r.u8()?)?;
            let lhs = Box::new(get(r, depth + 1)?);
            let rhs = Box::new(get(r, depth + 1)?);
            Expr::Compare { op, lhs, rhs }
        }
        tag::AND | tag::OR => {
            let n = r.varint()? as usize;
            if n > r.remaining() {
                return Err(Error::Corruption(format!(
                    "condition declares {n} operands but only {} bytes remain",
                    r.remaining()
                )));
            }
            let mut parts = Vec::with_capacity(n);
            for _ in 0..n {
                parts.push(get(r, depth + 1)?);
            }
            if t == tag::AND {
                Expr::And(parts)
            } else {
                Expr::Or(parts)
            }
        }
        tag::NOT => Expr::Not(Box::new(get(r, depth + 1)?)),
        tag::IS_NULL => Expr::IsNull(Box::new(get(r, depth + 1)?)),
        tag::IS_NOT_NULL => Expr::IsNotNull(Box::new(get(r, depth + 1)?)),
        tag::ARITH => {
            let op = arith_from(r.u8()?)?;
            let lhs = Box::new(get(r, depth + 1)?);
            let rhs = Box::new(get(r, depth + 1)?);
            Expr::Arith { op, lhs, rhs }
        }
        tag::IN => {
            let needle = Box::new(get(r, depth + 1)?);
            let n = r.varint()? as usize;
            if n > r.remaining() {
                return Err(Error::Corruption(format!(
                    "condition declares {n} list items but only {} bytes remain",
                    r.remaining()
                )));
            }
            let mut list = Vec::with_capacity(n);
            for _ in 0..n {
                list.push(get(r, depth + 1)?);
            }
            Expr::In { needle, list }
        }
        tag::LIKE => {
            let text = Box::new(get(r, depth + 1)?);
            let pattern = r.str()?;
            Expr::Like { text, pattern }
        }
        other => return Err(Error::Corruption(format!("unknown expression tag {other}"))),
    })
}

/// The encoding as lowercase hex.
///
/// Needed because the only channel a persisted index definition has is its
/// *name*, which is a `String`. Embedding bytes there is not elegant, and the
/// alternative was a catalog format change — a superblock bump and a migration
/// — for one optional feature. Hex costs two bytes per byte on a condition
/// that is a few dozen bytes long, and nothing else in the system has to
/// change.
pub fn encode_expr_hex(e: &Expr) -> Result<String> {
    let bytes = encode_expr(e)?;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    Ok(s)
}

/// Inverse of [`encode_expr_hex`].
pub fn decode_expr_hex(s: &str) -> Result<Expr> {
    if s.len() % 2 != 0 {
        return Err(Error::Corruption(
            "condition hex has an odd number of digits".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    let raw = s.as_bytes();
    for pair in raw.chunks(2) {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| Error::Corruption("condition hex has a non-hex digit".into()))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| Error::Corruption("condition hex has a non-hex digit".into()))?;
        bytes.push((hi * 16 + lo) as u8);
    }
    decode_expr(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::value::Value;

    fn round_trip(e: Expr) {
        let bytes = encode_expr(&e).expect("encode");
        assert_eq!(decode_expr(&bytes).expect("decode"), e);
        let hex = encode_expr_hex(&e).expect("hex");
        assert_eq!(decode_expr_hex(&hex).expect("unhex"), e);
    }

    #[test]
    fn every_variant_round_trips() {
        round_trip(Expr::eq("status", "pending"));
        round_trip(Expr::Field("a".into()));
        round_trip(Expr::Literal(Value::Null));
        round_trip(Expr::IsNull(Box::new(Expr::field("deleted_at"))));
        round_trip(Expr::IsNotNull(Box::new(Expr::field("deleted_at"))));
        round_trip(Expr::Not(Box::new(Expr::eq("a", 1i64))));
        round_trip(Expr::And(vec![
            Expr::eq("a", 1i64),
            Expr::eq("b", "two"),
            Expr::Or(vec![Expr::eq("c", 3i64), Expr::eq("d", 4i64)]),
        ]));
        round_trip(Expr::In {
            needle: Box::new(Expr::field("country")),
            list: vec![Expr::lit("NO"), Expr::lit("SE")],
        });
        round_trip(Expr::Like {
            text: Box::new(Expr::field("name")),
            pattern: "a%_\\%".into(),
        });
        round_trip(Expr::field("a") + Expr::lit(1i64));
        round_trip(Expr::field("a") - Expr::lit(1i64));
        round_trip(Expr::field("a") * Expr::lit(2i64));
        round_trip(Expr::field("a") / Expr::lit(2i64));
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            round_trip(Expr::cmp("x", op, 1i64));
        }
    }

    /// The decoder sees bytes that may not have come from the encoder, so
    /// every one of these must be an error rather than a panic or a plausible
    /// expression.
    #[test]
    fn malformed_input_is_refused_not_guessed() {
        assert!(decode_expr(&[]).is_err(), "empty");
        assert!(decode_expr(&[200]).is_err(), "unknown tag");
        assert!(decode_expr(&[tag::FIELD]).is_err(), "truncated in a name");
        assert!(decode_expr(&[tag::NOT]).is_err(), "missing operand");
        assert!(decode_expr_hex("abc").is_err(), "odd hex length");
        assert!(decode_expr_hex("zz").is_err(), "non-hex digit");

        // Trailing bytes: a valid expression followed by junk is not a valid
        // encoding, and accepting it would let two different byte strings mean
        // the same index.
        let mut bytes = encode_expr(&Expr::eq("a", 1i64)).unwrap();
        bytes.push(0);
        assert!(decode_expr(&bytes).is_err(), "trailing bytes");
    }

    /// A hostile length prefix must not make the decoder allocate for it.
    #[test]
    fn a_huge_declared_operand_count_is_rejected_cheaply() {
        let mut bytes = vec![tag::AND];
        adabt_storage::varint::write_u64(u32::MAX as u64, &mut bytes);
        assert!(decode_expr(&bytes).is_err());
    }

    /// Deep nesting is refused on the way in and on the way out. The decoder's
    /// bound is the one that protects the process.
    #[test]
    fn nesting_past_the_limit_is_refused_at_both_ends() {
        let mut e = Expr::eq("a", 1i64);
        for _ in 0..MAX_DEPTH + 5 {
            e = Expr::Not(Box::new(e));
        }
        assert!(encode_expr(&e).is_err(), "encoder must refuse");

        // Hand-built, so the decoder is tested on bytes the encoder would not
        // have produced.
        let mut bytes = vec![tag::NOT; (MAX_DEPTH + 5) as usize];
        bytes.extend(encode_expr(&Expr::eq("a", 1i64)).unwrap());
        assert!(decode_expr(&bytes).is_err(), "decoder must refuse");
    }
}
