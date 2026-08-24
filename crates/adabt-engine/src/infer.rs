//! Schema inference: moving a collection up the rigidity spectrum.
//!
//! `Dynamic → Declared → Strict → Fixed` has been a declared dial since M0.
//! This is the machinery that lets the database *move along it* by observing
//! what the data actually looks like, which is the transition the whole design
//! exists to make possible: a collection that started schemaless and settled
//! into a stable shape can become directly addressable without its API
//! changing.
//!
//! # Freezing costs freedom, and says so
//!
//! Raising rigidity takes something away. A `Dynamic` collection accepts any
//! record; a `Fixed` one rejects anything that does not match. That is a real
//! loss for the user, not merely a physical change, which is why the inference
//! reports what it would forbid and why the optimization that applies it
//! carries a negative `freedom` effect.
//!
//! It is also the one specialization that cannot be silently reverted: widening
//! a schema back out is safe, but records written under the narrow one are
//! already constrained.

use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::value::Value;
use std::collections::BTreeMap;

/// What was seen in one field across a sample.
#[derive(Debug, Clone, Default)]
struct FieldEvidence {
    present: usize,
    /// Type tags seen, so a field with mixed types is not frozen to one.
    bools: usize,
    ints: usize,
    uints: usize,
    floats: usize,
    strings: usize,
    bytes: usize,
    other: usize,
    max_len: usize,
}

impl FieldEvidence {
    fn observe(&mut self, v: &Value) {
        self.present += 1;
        match v {
            Value::Null => self.present -= 1,
            Value::Bool(_) => self.bools += 1,
            Value::I64(_) => self.ints += 1,
            Value::U64(_) => self.uints += 1,
            Value::F64(_) => self.floats += 1,
            Value::Str(s) => {
                self.strings += 1;
                self.max_len = self.max_len.max(s.len());
            }
            Value::Bytes(b) => {
                self.bytes += 1;
                self.max_len = self.max_len.max(b.len());
            }
            _ => self.other += 1,
        }
    }

    /// The single type this field always held, if there is one.
    ///
    /// Returns `None` for a mixed field: freezing one of those would reject
    /// data the collection has demonstrably been storing.
    fn settled_type(&self, headroom: usize) -> Option<FieldType> {
        if self.other > 0 {
            return None;
        }
        let kinds = [
            (self.bools, 0),
            (self.ints, 1),
            (self.uints, 2),
            (self.floats, 3),
            (self.strings, 4),
            (self.bytes, 5),
        ];
        let seen: Vec<usize> = kinds
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(_, k)| *k)
            .collect();
        match seen.as_slice() {
            [0] => Some(FieldType::Bool),
            [1] => Some(FieldType::I64),
            [2] => Some(FieldType::U64),
            [3] => Some(FieldType::F64),
            // Integers and unsigned integers together widen to signed, which
            // accepts both.
            [1, 2] => Some(FieldType::I64),
            [4] => Some(FieldType::Char(width_for(self.max_len + headroom))),
            [5] => Some(FieldType::FixedBytes(width_for(self.max_len + headroom))),
            _ => None,
        }
    }
}

/// Slot width for a given content length, allowing for the inline length prefix.
fn width_for(content: usize) -> u32 {
    let content = content.max(1) as u32;
    content + FieldType::length_prefix_bytes(content + 4) + 1
}

/// What inference concluded, and what it would cost.
#[derive(Debug, Clone)]
pub struct InferredSchema {
    pub schema: Schema,
    /// Fields present in every sampled record.
    pub universal_fields: Vec<String>,
    /// Fields seen in some records but not all; nullable in the result.
    pub optional_fields: Vec<String>,
    /// Fields that could not be frozen, and why.
    pub rejected: Vec<(String, String)>,
    pub records_sampled: usize,
}

impl InferredSchema {
    /// Whether the inferred schema is strict enough to be directly addressed.
    pub fn is_fixed(&self) -> bool {
        self.schema.mode() == SchemaMode::Fixed
    }

    /// What freezing would newly forbid, in words the user can act on.
    pub fn describe_cost(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!(
            "records would be restricted to {} field(s)",
            self.schema.fields().len()
        ));
        if !self.rejected.is_empty() {
            parts.push(format!(
                "{} field(s) could not be frozen: {}",
                self.rejected.len(),
                self.rejected
                    .iter()
                    .map(|(f, why)| format!("{f} ({why})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.join("; ")
    }
}

/// Extra bytes allowed beyond the longest value seen.
///
/// Without headroom the first record longer than anything sampled is rejected,
/// which turns an optimization into an outage. With too much, the fixed layout
/// wastes the space it exists to save.
pub const DEFAULT_HEADROOM: usize = 16;

/// Infer the most rigid schema a sample supports.
///
/// Conservative by construction: a field is only frozen if every sampled record
/// agreed about it. Anything mixed, absent from the type system, or too varied
/// falls out, and the result degrades to `Declared` rather than to a `Fixed`
/// schema that would reject real data.
pub fn infer<'a>(records: impl Iterator<Item = &'a Record>, headroom: usize) -> InferredSchema {
    let mut evidence: BTreeMap<String, FieldEvidence> = BTreeMap::new();
    let mut count = 0usize;
    for rec in records {
        count += 1;
        for (name, value) in rec.iter() {
            evidence.entry(name.to_string()).or_default().observe(value);
        }
    }

    let mut fields = Vec::new();
    let mut universal = Vec::new();
    let mut optional = Vec::new();
    let mut rejected = Vec::new();

    for (name, ev) in &evidence {
        match ev.settled_type(headroom) {
            Some(ty) => {
                let always = ev.present == count && count > 0;
                let mut def = FieldDef::new(name.clone(), ty);
                if always {
                    def = def.required();
                    universal.push(name.clone());
                } else {
                    optional.push(name.clone());
                }
                fields.push(def);
            }
            None => {
                let why = if ev.other > 0 {
                    "holds nested values".to_string()
                } else {
                    "holds more than one type".to_string()
                };
                rejected.push((name.clone(), why));
            }
        }
    }

    // Fixed needs every field frozen *and* every field fixed-width. Anything
    // left out means the collection would lose data it currently accepts.
    let mode = if count > 0 && rejected.is_empty() && !fields.is_empty() {
        SchemaMode::Fixed
    } else if fields.is_empty() {
        SchemaMode::Dynamic
    } else {
        SchemaMode::Declared
    };

    let schema = if mode == SchemaMode::Dynamic {
        Schema::dynamic()
    } else {
        Schema::new(mode, fields.clone()).unwrap_or_else(|_| {
            // A width the type system rejects: fall back rather than fail.
            Schema::new(SchemaMode::Declared, fields).unwrap_or_else(|_| Schema::dynamic())
        })
    };

    InferredSchema {
        schema,
        universal_fields: universal,
        optional_fields: optional,
        rejected,
        records_sampled: count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform(n: u64) -> Vec<Record> {
        (0..n)
            .map(|i| {
                Record::new()
                    .with("id", i)
                    .with("balance", i as i64)
                    .with("name", format!("n{i}"))
            })
            .collect()
    }

    #[test]
    fn a_uniform_collection_infers_a_fixed_schema() {
        // The transition the design exists for: schemaless data that settled
        // into a shape becomes directly addressable.
        let recs = uniform(200);
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        assert!(out.is_fixed(), "{:?}", out.rejected);
        assert_eq!(out.schema.fields().len(), 3);
        assert!(out.schema.fixed_record_size().is_some());
        assert_eq!(out.universal_fields.len(), 3);
    }

    #[test]
    fn every_sampled_record_still_validates_against_the_inferred_schema() {
        // The property that makes freezing safe: nothing already stored would
        // be rejected by the schema inferred from it.
        let recs = uniform(300);
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        for r in &recs {
            out.schema
                .validate_record(r)
                .unwrap_or_else(|e| panic!("inferred schema rejects its own input: {e}"));
        }
    }

    #[test]
    fn a_mixed_type_field_is_refused_rather_than_guessed() {
        let recs = [
            Record::new().with("v", 1i64),
            Record::new().with("v", "text"),
        ];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        assert!(!out.is_fixed());
        assert_eq!(out.rejected.len(), 1);
        assert!(out.rejected[0].1.contains("more than one type"));
    }

    #[test]
    fn a_nested_field_prevents_freezing() {
        let recs = [
            Record::new().with("v", Value::List(vec![Value::I64(1)])),
            Record::new().with("v", Value::List(vec![Value::I64(2)])),
        ];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        assert!(!out.is_fixed());
        assert!(out.rejected[0].1.contains("nested"));
    }

    #[test]
    fn a_field_missing_from_some_records_becomes_optional_not_required() {
        let recs: Vec<Record> = (0..10u64)
            .map(|i| {
                let mut r = Record::new().with("id", i);
                if i % 2 == 0 {
                    r.set("extra", i as i64);
                }
                r
            })
            .collect();
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        assert_eq!(out.universal_fields, vec!["id".to_string()]);
        assert_eq!(out.optional_fields, vec!["extra".to_string()]);
        for r in &recs {
            out.schema.validate_record(r).unwrap();
        }
    }

    #[test]
    fn headroom_leaves_room_for_values_longer_than_anything_sampled() {
        // Without it the first longer record is rejected, turning an
        // optimization into an outage.
        let recs: Vec<Record> = vec![Record::new().with("s", "short")];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        let longer = Record::new().with("s", "short plus a bit more");
        assert!(
            out.schema.validate_record(&longer).is_ok(),
            "no headroom: a slightly longer value would be rejected"
        );
    }

    #[test]
    fn a_value_far_longer_than_the_headroom_is_still_rejected() {
        // Freezing does constrain; the point is that it constrains predictably.
        let recs: Vec<Record> = vec![Record::new().with("s", "short")];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        let huge = Record::new().with("s", "x".repeat(5_000));
        assert!(out.schema.validate_record(&huge).is_err());
    }

    #[test]
    fn signed_and_unsigned_integers_widen_rather_than_conflict() {
        let recs = [
            Record::new().with("n", 5u64),
            Record::new().with("n", -5i64),
        ];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        assert!(out.is_fixed(), "{:?}", out.rejected);
        for r in &recs {
            out.schema.validate_record(r).unwrap();
        }
    }

    #[test]
    fn an_empty_sample_infers_nothing() {
        let out = infer(std::iter::empty(), DEFAULT_HEADROOM);
        assert_eq!(out.schema.mode(), SchemaMode::Dynamic);
        assert_eq!(out.records_sampled, 0);
        assert!(!out.is_fixed());
    }

    #[test]
    fn nulls_do_not_count_as_a_type() {
        let recs = [
            Record::new().with("v", 1i64),
            Record::new().with("v", Value::Null),
        ];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        assert!(out.rejected.is_empty(), "{:?}", out.rejected);
        assert_eq!(out.optional_fields, vec!["v".to_string()]);
    }

    #[test]
    fn the_cost_of_freezing_is_stated() {
        let recs = [
            Record::new().with("ok", 1i64).with("mixed", 1i64),
            Record::new().with("ok", 2i64).with("mixed", "s"),
        ];
        let out = infer(recs.iter(), DEFAULT_HEADROOM);
        let cost = out.describe_cost();
        assert!(cost.contains("restricted to"), "{cost}");
        assert!(cost.contains("mixed"), "{cost}");
    }
}
