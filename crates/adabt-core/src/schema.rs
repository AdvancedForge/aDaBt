//! Schema definitions and the **schema-mode spectrum**.
//!
//! The rigidity of a collection's shape is a declared, per-collection dial. It
//! is what lets one logical API span the whole optimization space: `Dynamic`
//! is the freedom endpoint, `Fixed` is the precondition that makes
//! `address = BASE + id * RECORD_SIZE` legal at Level 10+.
//!
//! The logical call is `db.collection("users").get(id)` in all four modes. Only
//! the physical path differs.

use crate::error::SchemaError;
use crate::record::Record;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SchemaMode {
    /// Arbitrary nested fields, nothing declared. Tag-length-value encoding.
    Dynamic,
    /// Declared fields plus an overflow bag for extras. Offset-table encoding.
    Declared,
    /// Exactly the declared fields, no extras. Offset-table, no overflow.
    Strict,
    /// `Strict` and every field fixed-width, so records have a constant size.
    Fixed,
}

impl SchemaMode {
    /// Whether records may carry fields the schema does not declare.
    pub fn allows_extra_fields(self) -> bool {
        matches!(self, SchemaMode::Dynamic | SchemaMode::Declared)
    }

    /// Rigidity rank, ascending. Freezing a schema may only move upward.
    pub fn rigidity(self) -> u8 {
        match self {
            SchemaMode::Dynamic => 0,
            SchemaMode::Declared => 1,
            SchemaMode::Strict => 2,
            SchemaMode::Fixed => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FieldType {
    Bool,
    I64,
    U64,
    F64,
    /// Fixed-width, space-padded text of exactly `width` bytes.
    Char(u32),
    /// Fixed-width byte array of exactly `len` bytes.
    FixedBytes(u32),
    /// Variable-length UTF-8, optionally capped.
    Str {
        max_len: Option<u32>,
    },
    /// Variable-length bytes, optionally capped.
    Bytes {
        max_len: Option<u32>,
    },
    List(Box<FieldType>),
    Map,
    /// Any value. Only legal in `Dynamic` collections.
    Any,
}

impl FieldType {
    /// Width in bytes when this type has a constant physical size.
    ///
    /// `Some(_)` here for every field is exactly the condition for
    /// `SchemaMode::Fixed`, and therefore for direct addressing.
    pub fn fixed_width(&self) -> Option<u32> {
        match self {
            FieldType::Bool => Some(1),
            FieldType::I64 | FieldType::U64 | FieldType::F64 => Some(8),
            FieldType::Char(n) | FieldType::FixedBytes(n) => Some(*n),
            FieldType::Str { .. }
            | FieldType::Bytes { .. }
            | FieldType::List(_)
            | FieldType::Map
            | FieldType::Any => None,
        }
    }

    pub fn name(&self) -> String {
        match self {
            FieldType::Bool => "bool".into(),
            FieldType::I64 => "i64".into(),
            FieldType::U64 => "u64".into(),
            FieldType::F64 => "f64".into(),
            FieldType::Char(n) => format!("char[{n}]"),
            FieldType::FixedBytes(n) => format!("bytes[{n}]"),
            FieldType::Str { .. } => "str".into(),
            FieldType::Bytes { .. } => "bytes".into(),
            FieldType::List(t) => format!("list<{}>", t.name()),
            FieldType::Map => "map".into(),
            FieldType::Any => "any".into(),
        }
    }

    /// Whether `v` inhabits this type. Width limits are checked separately so
    /// the caller can produce a `TooWide` error naming the field.
    pub fn accepts(&self, v: &Value) -> bool {
        match (self, v) {
            (_, Value::Null) => true,
            (FieldType::Any, _) => true,
            (FieldType::Bool, Value::Bool(_)) => true,
            // Integers are accepted into float fields but not the reverse:
            // widening is lossless, narrowing is not.
            (FieldType::I64, Value::I64(_) | Value::U64(_)) => true,
            (FieldType::U64, Value::U64(_)) => true,
            (FieldType::U64, Value::I64(n)) => *n >= 0,
            (FieldType::F64, Value::F64(_) | Value::I64(_) | Value::U64(_)) => true,
            (FieldType::Char(_) | FieldType::Str { .. }, Value::Str(_)) => true,
            (FieldType::FixedBytes(_) | FieldType::Bytes { .. }, Value::Bytes(_)) => true,
            (FieldType::List(inner), Value::List(items)) => items.iter().all(|i| inner.accepts(i)),
            (FieldType::Map, Value::Map(_)) => true,
            _ => false,
        }
    }

    /// Byte length of `v` under this type, for width checking.
    fn encoded_len(v: &Value) -> Option<usize> {
        match v {
            Value::Str(s) => Some(s.len()),
            Value::Bytes(b) => Some(b.len()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldDef {
    pub name: String,
    pub ty: FieldType,
    pub nullable: bool,
}

impl FieldDef {
    pub fn new(name: impl Into<String>, ty: FieldType) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable: true,
        }
    }
    pub fn required(mut self) -> Self {
        self.nullable = false;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    mode: SchemaMode,
    fields: Vec<FieldDef>,
}

impl Schema {
    /// Build a schema, validating the invariants of its mode.
    pub fn new(mode: SchemaMode, fields: Vec<FieldDef>) -> Result<Self, SchemaError> {
        let s = Schema { mode, fields };
        s.validate()?;
        Ok(s)
    }

    /// A schemaless collection: the freedom endpoint.
    pub fn dynamic() -> Self {
        Schema {
            mode: SchemaMode::Dynamic,
            fields: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), SchemaError> {
        if self.mode != SchemaMode::Dynamic && self.fields.is_empty() {
            return Err(SchemaError::Empty);
        }
        let mut seen = std::collections::HashSet::new();
        for f in &self.fields {
            if !seen.insert(&f.name) {
                return Err(SchemaError::DuplicateField(f.name.clone()));
            }
            if self.mode == SchemaMode::Fixed && f.ty.fixed_width().is_none() {
                return Err(SchemaError::NotFixedWidth(f.name.clone()));
            }
        }
        Ok(())
    }

    pub fn mode(&self) -> SchemaMode {
        self.mode
    }
    pub fn fields(&self) -> &[FieldDef] {
        &self.fields
    }
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Constant record size, when the mode guarantees one.
    ///
    /// `Some(_)` is the gate that `DirectLookup` checks before it may replace a
    /// heap representation with a directly-addressed fixed array.
    pub fn fixed_record_size(&self) -> Option<u32> {
        if self.mode != SchemaMode::Fixed {
            return None;
        }
        self.fields
            .iter()
            .map(|f| f.ty.fixed_width())
            .sum::<Option<u32>>()
    }

    /// Byte offset of a field within a fixed-layout record.
    pub fn fixed_offset_of(&self, name: &str) -> Option<u32> {
        if self.mode != SchemaMode::Fixed {
            return None;
        }
        let mut off = 0u32;
        for f in &self.fields {
            if f.name == name {
                return Some(off);
            }
            off += f.ty.fixed_width()?;
        }
        None
    }

    /// Check a record against this schema. The single authority on what is
    /// storable; every write path must pass through it.
    pub fn validate_record(&self, rec: &Record) -> Result<(), SchemaError> {
        if !self.mode.allows_extra_fields() {
            for name in rec.field_names() {
                if self.field(name).is_none() {
                    return Err(SchemaError::UnknownField {
                        field: name.to_string(),
                        mode: self.mode,
                    });
                }
            }
        }
        for def in &self.fields {
            match rec.get(&def.name) {
                None | Some(Value::Null) => {
                    if !def.nullable {
                        return Err(SchemaError::MissingField(def.name.clone()));
                    }
                }
                Some(v) => {
                    if !def.ty.accepts(v) {
                        return Err(SchemaError::TypeMismatch {
                            field: def.name.clone(),
                            expected: def.ty.name(),
                            actual: v.type_name().to_string(),
                        });
                    }
                    if let (Some(w), Some(len)) = (def.ty.fixed_width(), FieldType::encoded_len(v))
                    {
                        // Char/FixedBytes pad up to width; overflowing it is an error.
                        if len > w as usize {
                            return Err(SchemaError::TooWide {
                                field: def.name.clone(),
                                len,
                                width: w,
                            });
                        }
                    }
                    if let Some(max) = match &def.ty {
                        FieldType::Str { max_len } | FieldType::Bytes { max_len } => *max_len,
                        _ => None,
                    } {
                        if let Some(len) = FieldType::encoded_len(v) {
                            if len > max as usize {
                                return Err(SchemaError::TooWide {
                                    field: def.name.clone(),
                                    len,
                                    width: max,
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_schema() -> Schema {
        Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("balance", FieldType::I64).required(),
                FieldDef::new("name", FieldType::Char(32)),
            ],
        )
        .unwrap()
    }

    #[test]
    fn fixed_record_size_is_sum_of_widths() {
        assert_eq!(fixed_schema().fixed_record_size(), Some(8 + 8 + 32));
    }

    #[test]
    fn fixed_offsets_are_cumulative() {
        let s = fixed_schema();
        assert_eq!(s.fixed_offset_of("id"), Some(0));
        assert_eq!(s.fixed_offset_of("balance"), Some(8));
        assert_eq!(s.fixed_offset_of("name"), Some(16));
        assert_eq!(s.fixed_offset_of("nope"), None);
    }

    #[test]
    fn fixed_mode_rejects_variable_width_fields() {
        let err = Schema::new(
            SchemaMode::Fixed,
            vec![FieldDef::new("bio", FieldType::Str { max_len: None })],
        )
        .unwrap_err();
        assert!(matches!(err, SchemaError::NotFixedWidth(f) if f == "bio"));
    }

    #[test]
    fn only_fixed_mode_reports_a_record_size() {
        for mode in [
            SchemaMode::Dynamic,
            SchemaMode::Declared,
            SchemaMode::Strict,
        ] {
            let s = Schema::new(mode, vec![FieldDef::new("id", FieldType::U64)]).unwrap();
            assert_eq!(
                s.fixed_record_size(),
                None,
                "{mode:?} must not claim a fixed size"
            );
        }
    }

    #[test]
    fn strict_mode_rejects_undeclared_fields() {
        let s = Schema::new(
            SchemaMode::Strict,
            vec![FieldDef::new("id", FieldType::U64)],
        )
        .unwrap();
        let mut r = Record::new();
        r.set("id", Value::U64(1));
        r.set("stowaway", Value::U64(2));
        assert!(matches!(
            s.validate_record(&r).unwrap_err(),
            SchemaError::UnknownField { .. }
        ));
    }

    #[test]
    fn declared_mode_permits_extra_fields() {
        let s = Schema::new(
            SchemaMode::Declared,
            vec![FieldDef::new("id", FieldType::U64)],
        )
        .unwrap();
        let mut r = Record::new();
        r.set("id", Value::U64(1));
        r.set("extra", Value::Str("fine".into()));
        assert!(s.validate_record(&r).is_ok());
    }

    #[test]
    fn required_field_must_be_present_and_non_null() {
        let s = fixed_schema();
        let mut r = Record::new();
        r.set("id", Value::U64(1));
        assert!(matches!(
            s.validate_record(&r).unwrap_err(),
            SchemaError::MissingField(f) if f == "balance"
        ));
        r.set("balance", Value::Null);
        assert!(matches!(
            s.validate_record(&r).unwrap_err(),
            SchemaError::MissingField(f) if f == "balance"
        ));
    }

    #[test]
    fn oversized_value_rejected_for_fixed_width_field() {
        let s = fixed_schema();
        let mut r = Record::new();
        r.set("id", Value::U64(1));
        r.set("balance", Value::I64(0));
        r.set("name", Value::Str("x".repeat(33)));
        assert!(matches!(
            s.validate_record(&r).unwrap_err(),
            SchemaError::TooWide {
                width: 32,
                len: 33,
                ..
            }
        ));
    }

    #[test]
    fn integers_widen_into_float_fields_but_floats_do_not_narrow() {
        assert!(FieldType::F64.accepts(&Value::I64(3)));
        assert!(!FieldType::I64.accepts(&Value::F64(3.0)));
        assert!(!FieldType::U64.accepts(&Value::I64(-1)));
        assert!(FieldType::U64.accepts(&Value::I64(1)));
    }
}
