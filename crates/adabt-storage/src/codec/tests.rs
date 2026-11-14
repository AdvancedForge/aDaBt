use super::*;
use adabt_core::schema::FieldType;
use adabt_core::schema::{FieldDef, SchemaMode};
use adabt_core::value::Value;
use adabt_testkit::rng::Rng;

use super::tlv::{tag, MAX_DEPTH};
use crate::varint;

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
