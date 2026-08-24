//! Request and response bodies.
//!
//! A frame's header says how long its body is and what kind of message it is;
//! this is what the body means. Everything is little-endian and length-prefixed,
//! with no self-describing structure beyond what each message needs — the frame
//! header has already established the kind, so re-encoding it in the body would
//! be a second source of truth about the same fact.
//!
//! # Records go over the wire in the engine's own encoding
//!
//! A record body is exactly what a `Dynamic` collection stores: tag-length-value
//! bytes produced by [`adabt_storage::codec`]. Inventing a second record
//! encoding for the network would double the surface where a value can be
//! misinterpreted, and this one is already covered by the codec property tests
//! and the differential runner.
//!
//! # The query body is a deliberate subset
//!
//! [`Query`](crate::RequestKind::Query) carries a collection, an optional
//! single-field comparison and a limit — not the logical IR. That is not an
//! oversight. A wire format is a compatibility promise, the IR is still
//! changing, and freezing the second inside the first would mean every future
//! change to the query language became a protocol break. A stable serialized IR
//! is worth having and is not this.
//!
//! What the subset does cover is the shape that matters most for exercising the
//! engine remotely: a filtered scan, which is what routes through an index, a
//! column store or a full scan depending on what the optimizer has decided.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::value::Value;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::{CmpOp, Expr};
use adabt_storage::codec::{decode_value, encode_value, RecordCodec};

fn record_codec() -> RecordCodec {
    RecordCodec::new(Schema::dynamic())
}

pub fn encode_record(rec: &Record) -> Result<Vec<u8>> {
    record_codec().encode(rec)
}

pub fn decode_record(bytes: &[u8]) -> Result<Record> {
    record_codec().decode(bytes)
}

// -- writing ---------------------------------------------------------------

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    pub fn value(&mut self, v: &Value) -> &mut Self {
        encode_value(v, &mut self.0);
        self
    }
    pub fn record(&mut self, rec: &Record) -> Result<&mut Self> {
        let b = encode_record(rec)?;
        Ok(self.bytes(&b))
    }
    /// Take the bytes written so far.
    ///
    /// `&mut self` rather than `self` so the builder methods can return `&mut
    /// Self` and still be chained into a `finish()` — the alternative is a
    /// by-value builder whose every call moves, which reads worse at every call
    /// site than it saves here.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

// -- reading ---------------------------------------------------------------

/// Every read is bounds-checked against what is left.
///
/// The bytes come from the network, so a length field is a claim rather than a
/// fact: nothing is reserved or sliced on the strength of one without checking
/// it first.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn short(what: &str) -> Error {
        Error::Corruption(format!("request body ended while reading {what}"))
    }

    pub fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| Self::short(what))?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| Self::short(what))?;
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }
    pub fn u32(&mut self, what: &str) -> Result<u32> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self, what: &str) -> Result<u64> {
        let b = self.take(8, what)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    pub fn str(&mut self, what: &str) -> Result<String> {
        let n = self.u32(what)? as usize;
        let b = self.take(n, what)?;
        String::from_utf8(b.to_vec())
            .map_err(|_| Error::Corruption(format!("{what} is not valid UTF-8")))
    }
    pub fn bytes(&mut self, what: &str) -> Result<&'a [u8]> {
        let n = self.u32(what)? as usize;
        self.take(n, what)
    }
    pub fn value(&mut self, what: &str) -> Result<Value> {
        let rest = self.buf.get(self.pos..).ok_or_else(|| Self::short(what))?;
        let (v, used) = decode_value(rest)?;
        self.pos += used;
        Ok(v)
    }
    pub fn record(&mut self, what: &str) -> Result<Record> {
        let b = self.bytes(what)?;
        decode_record(b)
    }
    /// Fail if anything is left over.
    ///
    /// A body longer than the message needs means the two sides disagree about
    /// the format, and continuing on the assumption that the extra bytes are
    /// harmless is how a version skew turns into a silent misread.
    pub fn end(&self) -> Result<()> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(Error::Corruption(format!(
                "{} unexpected trailing byte(s) in request body",
                self.buf.len() - self.pos
            )))
        }
    }
}

// -- the query subset ------------------------------------------------------

/// A filtered scan: the one query shape the wire format commits to.
#[derive(Debug, Clone, PartialEq)]
pub struct QuerySpec {
    pub collection: String,
    pub filter: Option<(String, CmpOp, Value)>,
    /// Zero means no limit.
    pub limit: u32,
}

pub fn cmp_code(op: CmpOp) -> u8 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
        CmpOp::Gt => 4,
        CmpOp::Ge => 5,
    }
}

pub fn cmp_from_code(c: u8) -> Result<CmpOp> {
    Ok(match c {
        0 => CmpOp::Eq,
        1 => CmpOp::Ne,
        2 => CmpOp::Lt,
        3 => CmpOp::Le,
        4 => CmpOp::Gt,
        5 => CmpOp::Ge,
        other => return Err(Error::Corruption(format!("unknown comparison {other}"))),
    })
}

impl QuerySpec {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(&self.collection);
        match &self.filter {
            Some((field, op, value)) => {
                w.u8(1).str(field).u8(cmp_code(*op)).value(value);
            }
            None => {
                w.u8(0);
            }
        }
        w.u32(self.limit);
        w.finish()
    }

    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let collection = r.str("collection")?;
        let filter = match r.u8("filter flag")? {
            0 => None,
            1 => Some((
                r.str("filter field")?,
                cmp_from_code(r.u8("comparison")?)?,
                r.value("filter value")?,
            )),
            other => {
                return Err(Error::Corruption(format!(
                    "filter flag must be 0 or 1, not {other}"
                )))
            }
        };
        let limit = r.u32("limit")?;
        r.end()?;
        Ok(QuerySpec {
            collection,
            filter,
            limit,
        })
    }

    pub fn to_plan(&self) -> LogicalPlan {
        let mut op = LogicalOp::scan(&self.collection);
        if let Some((field, cmp, value)) = &self.filter {
            op = op.filter(Expr::cmp(field, *cmp, value.clone()));
        }
        if self.limit > 0 {
            op = op.limit(self.limit as usize);
        }
        LogicalPlan::new(op)
    }
}

/// Rows, as returned by a query or a scan.
pub fn encode_rows(rows: &[(RecordId, Record)]) -> Result<Vec<u8>> {
    let mut w = Writer::new();
    w.u32(rows.len() as u32);
    for (id, rec) in rows {
        w.u64(id.0);
        w.record(rec)?;
    }
    Ok(w.finish())
}

pub fn decode_rows(body: &[u8]) -> Result<Vec<(RecordId, Record)>> {
    let mut r = Reader::new(body);
    let n = r.u32("row count")? as usize;
    let mut out = Vec::new();
    for _ in 0..n {
        let id = RecordId(r.u64("row id")?);
        out.push((id, r.record("row")?));
    }
    r.end()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_through_the_wire_encoding() {
        let rec = Record::new()
            .with("id", 7u64)
            .with("name", "Ada")
            .with("score", -3i64)
            .with("ratio", 0.5f64);
        assert_eq!(decode_record(&encode_record(&rec).unwrap()).unwrap(), rec);
    }

    #[test]
    fn every_query_spec_round_trips() {
        for filter in [
            None,
            Some(("country".to_string(), CmpOp::Eq, Value::from("NO"))),
            Some(("age".to_string(), CmpOp::Ge, Value::I64(40))),
        ] {
            for limit in [0u32, 1, 1000] {
                let q = QuerySpec {
                    collection: "users".into(),
                    filter: filter.clone(),
                    limit,
                };
                assert_eq!(QuerySpec::decode(&q.encode()).unwrap(), q);
            }
        }
    }

    #[test]
    fn every_comparison_survives_the_wire() {
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            assert_eq!(cmp_from_code(cmp_code(op)).unwrap(), op);
        }
        assert!(cmp_from_code(200).is_err());
    }

    #[test]
    fn a_truncated_body_is_refused_at_every_length() {
        // The bytes come off a socket. Every length in them is a claim.
        let q = QuerySpec {
            collection: "users".into(),
            filter: Some(("country".into(), CmpOp::Eq, Value::from("NO"))),
            limit: 10,
        };
        let full = q.encode();
        for cut in 0..full.len() {
            assert!(
                QuerySpec::decode(&full[..cut]).is_err(),
                "a body cut to {cut} bytes was accepted"
            );
        }
        assert!(QuerySpec::decode(&full).is_ok());
    }

    #[test]
    fn trailing_bytes_are_refused_rather_than_ignored() {
        let q = QuerySpec {
            collection: "users".into(),
            filter: None,
            limit: 0,
        };
        let mut extra = q.encode();
        extra.push(0);
        let e = QuerySpec::decode(&extra).unwrap_err();
        assert!(e.to_string().contains("trailing"), "{e}");
    }

    #[test]
    fn an_enormous_length_field_does_not_allocate() {
        // `u32::MAX` claimed for a string in a ten-byte body. Reading it must
        // fail on the bounds check rather than on the allocator.
        let mut w = Writer::new();
        w.u32(u32::MAX);
        w.u8(1);
        let body = w.finish();
        let mut r = Reader::new(&body);
        assert!(r.str("collection").is_err());
    }

    #[test]
    fn rows_round_trip() {
        let rows = vec![
            (RecordId(1), Record::new().with("a", 1i64)),
            (RecordId(9), Record::new().with("a", 9i64).with("b", "x")),
        ];
        assert_eq!(decode_rows(&encode_rows(&rows).unwrap()).unwrap(), rows);
        assert_eq!(decode_rows(&encode_rows(&[]).unwrap()).unwrap(), vec![]);
    }

    #[test]
    fn a_query_spec_becomes_the_plan_it_describes() {
        let q = QuerySpec {
            collection: "users".into(),
            filter: Some(("country".into(), CmpOp::Eq, Value::from("NO"))),
            limit: 5,
        };
        let text = q.to_plan().explain();
        assert!(text.contains("users"), "{text}");
        assert!(text.contains("country"), "{text}");
        assert!(text.contains('5'), "{text}");
    }
}
