//! Directly-addressed storage: the Level 10 endpoint.
//!
//! When a collection's schema guarantees a constant record size and its ids are
//! dense, a lookup needs no page directory, no slot table and no search:
//!
//! ```text
//! address = base + id * stride
//! ```
//!
//! This is deliberately built early, far out of level order. It is the extreme
//! end of the design, and if the optimization framework could not express both
//! a plan cache and *this* without special-casing, the abstraction would be
//! wrong — better to find that out now than in year three.
//!
//! It is a **derived** representation. The heap remains authoritative, every
//! byte here is reconstructible from it, and dropping it costs a rebuild rather
//! than a record. That is what makes it safe for an optimizer to switch on and
//! off under a live workload.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::value::Value;
use adabt_storage::codec::{read_fixed_field, RecordCodec, HEADER_LEN};

/// A flat array of fixed-size records with a presence bitmap.
pub struct DirectArray {
    codec: RecordCodec,
    stride: usize,
    bytes: Vec<u8>,
    present: Vec<u64>,
    /// One past the highest id ever stored.
    capacity: u64,
    live: u64,
}

impl DirectArray {
    /// Build one, if the schema permits a constant stride.
    pub fn new(schema: Schema) -> Option<Self> {
        let codec = RecordCodec::new(schema);
        let stride = codec.physical_record_size()? as usize;
        Some(Self {
            codec,
            stride,
            bytes: Vec::new(),
            present: Vec::new(),
            capacity: 0,
            live: 0,
        })
    }

    pub fn stride(&self) -> usize {
        self.stride
    }
    pub fn live(&self) -> u64 {
        self.live
    }
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Fraction of the addressable range actually occupied.
    ///
    /// The whole trade: a dense array is a multiplication, a sparse one is
    /// mostly wasted memory. Below a threshold this representation costs more
    /// than it saves, which is exactly what the optimization's applicability
    /// check is for.
    pub fn density(&self) -> f64 {
        if self.capacity == 0 {
            return 0.0;
        }
        self.live as f64 / self.capacity as f64
    }

    pub fn memory_bytes(&self) -> usize {
        self.bytes.len() + self.present.len() * 8 + std::mem::size_of::<Self>()
    }

    #[inline]
    fn is_present(&self, id: u64) -> bool {
        let (w, b) = ((id / 64) as usize, id % 64);
        self.present.get(w).is_some_and(|v| v & (1 << b) != 0)
    }

    #[inline]
    fn set_present(&mut self, id: u64, on: bool) {
        let (w, b) = ((id / 64) as usize, id % 64);
        if w >= self.present.len() {
            self.present.resize(w + 1, 0);
        }
        if on {
            self.present[w] |= 1 << b;
        } else {
            self.present[w] &= !(1 << b);
        }
    }

    fn grow_to(&mut self, id: u64) {
        let needed = (id as usize + 1) * self.stride;
        if self.bytes.len() < needed {
            self.bytes.resize(needed, 0);
        }
        if id + 1 > self.capacity {
            self.capacity = id + 1;
        }
    }

    /// The address calculation, in full.
    #[inline]
    fn slice_of(&self, id: u64) -> &[u8] {
        let base = id as usize * self.stride;
        &self.bytes[base..base + self.stride]
    }

    /// Read one field without decoding the rest of the record.
    ///
    /// The Level 11 idea taken literally:
    ///
    /// ```text
    /// address = base + id * stride + field_offset
    /// ```
    ///
    /// A `Fixed` schema puts every field at a constant offset, so a query that
    /// wants one field of one record need not touch the other bytes at all —
    /// no full decode, no `Record`, no `BTreeMap` allocation. What is removed
    /// here is not overhead in the usual sense; it is *generality*, the ability
    /// to answer for a record whose shape is not known in advance.
    pub fn field_at(&self, id: RecordId, field: &str) -> Result<Option<Value>> {
        if !self.is_present(id.0) {
            return Ok(None);
        }
        let schema = self.codec.schema();
        let Some(def) = schema.field(field) else {
            return Ok(None);
        };
        let Some(offset) = schema.fixed_offset_of(field) else {
            return Ok(None);
        };
        let Some(width) = def.ty.fixed_width() else {
            return Ok(None);
        };

        // Header and presence bitmap precede the field region.
        let bitmap_len = schema.fields().len().div_ceil(8);
        let base = id.0 as usize * self.stride;
        let field_start = base + HEADER_LEN + bitmap_len + offset as usize;
        let end = field_start + width as usize;
        if end > self.bytes.len() {
            return Ok(None);
        }

        // Presence is one bit; an absent field is absent without reading it.
        let index = schema
            .fields()
            .iter()
            .position(|f| f.name == field)
            .expect("field resolved above");
        let bitmap = &self.bytes[base + HEADER_LEN..base + HEADER_LEN + bitmap_len];
        if bitmap[index / 8] & (1 << (index % 8)) == 0 {
            return Ok(None);
        }

        Ok(Some(read_fixed_field(
            &def.ty,
            &self.bytes[field_start..end],
        )?))
    }

    pub fn get(&self, id: RecordId) -> Result<Option<Record>> {
        if !self.is_present(id.0) {
            return Ok(None);
        }
        // `is_present` implies the byte range was allocated, but a corrupt
        // bitmap must not index out of bounds.
        let base = id.0 as usize * self.stride;
        if base + self.stride > self.bytes.len() {
            return Ok(None);
        }
        Ok(Some(self.codec.decode(self.slice_of(id.0))?))
    }

    /// Store pre-encoded bytes. Rejects anything that is not exactly a stride,
    /// because a wrong-sized write would silently corrupt its neighbours.
    pub fn put_encoded(&mut self, id: RecordId, encoded: &[u8]) -> Result<()> {
        if encoded.len() != self.stride {
            return Err(adabt_core::error::Error::Corruption(format!(
                "direct array stride is {} but record encoded to {}",
                self.stride,
                encoded.len()
            )));
        }
        self.grow_to(id.0);
        let base = id.0 as usize * self.stride;
        self.bytes[base..base + self.stride].copy_from_slice(encoded);
        if !self.is_present(id.0) {
            self.live += 1;
        }
        self.set_present(id.0, true);
        Ok(())
    }

    pub fn put(&mut self, id: RecordId, rec: &Record) -> Result<()> {
        let encoded = self.codec.encode(rec)?;
        self.put_encoded(id, &encoded)
    }

    pub fn remove(&mut self, id: RecordId) {
        if self.is_present(id.0) {
            self.live -= 1;
            self.set_present(id.0, false);
        }
    }

    /// Rebuild from the authoritative representation.
    pub fn rebuild<'a>(
        schema: Schema,
        rows: impl Iterator<Item = (RecordId, &'a Record)>,
    ) -> Result<Option<Self>> {
        let Some(mut arr) = Self::new(schema) else {
            return Ok(None);
        };
        for (id, rec) in rows {
            arr.put(id, rec)?;
        }
        Ok(Some(arr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::schema::{FieldDef, FieldType, SchemaMode};
    use adabt_core::value::Value;

    fn fixed() -> Schema {
        Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("balance", FieldType::I64).required(),
                FieldDef::new("name", FieldType::Char(24)),
            ],
        )
        .unwrap()
    }

    fn rec(i: u64) -> Record {
        Record::new()
            .with("id", i)
            .with("balance", (i * 3) as i64)
            .with("name", format!("n{i}"))
    }

    #[test]
    fn only_a_fixed_schema_can_be_directly_addressed() {
        assert!(DirectArray::new(fixed()).is_some());
        assert!(DirectArray::new(Schema::dynamic()).is_none());
        let strict = Schema::new(
            SchemaMode::Strict,
            vec![FieldDef::new("s", FieldType::Str { max_len: None })],
        )
        .unwrap();
        assert!(DirectArray::new(strict).is_none());
    }

    #[test]
    fn put_and_get_round_trip() {
        let mut a = DirectArray::new(fixed()).unwrap();
        for i in 0..100u64 {
            a.put(RecordId(i), &rec(i)).unwrap();
        }
        for i in 0..100u64 {
            assert_eq!(a.get(RecordId(i)).unwrap(), Some(rec(i)));
        }
        assert_eq!(a.get(RecordId(500)).unwrap(), None);
    }

    #[test]
    fn every_record_occupies_exactly_one_stride() {
        let mut a = DirectArray::new(fixed()).unwrap();
        let stride = a.stride();
        a.put(RecordId(0), &rec(0)).unwrap();
        assert_eq!(
            a.memory_bytes() - std::mem::size_of::<DirectArray>() - 8,
            stride
        );
        a.put(RecordId(9), &rec(9)).unwrap();
        // Ten slots allocated even though only two are live: that is the cost
        // of direct addressing, and why density gates it.
        assert!(a.bytes.len() >= 10 * stride);
    }

    #[test]
    fn a_neighbouring_record_is_not_disturbed_by_a_write() {
        let mut a = DirectArray::new(fixed()).unwrap();
        for i in 0..5u64 {
            a.put(RecordId(i), &rec(i)).unwrap();
        }
        a.put(RecordId(2), &rec(999)).unwrap();
        assert_eq!(a.get(RecordId(1)).unwrap(), Some(rec(1)));
        assert_eq!(a.get(RecordId(3)).unwrap(), Some(rec(3)));
        assert_eq!(
            a.get(RecordId(2)).unwrap().unwrap().get("balance"),
            Some(&Value::I64(999 * 3))
        );
    }

    #[test]
    fn removal_hides_a_record_without_disturbing_others() {
        let mut a = DirectArray::new(fixed()).unwrap();
        for i in 0..10u64 {
            a.put(RecordId(i), &rec(i)).unwrap();
        }
        a.remove(RecordId(5));
        assert_eq!(a.get(RecordId(5)).unwrap(), None);
        assert_eq!(a.get(RecordId(4)).unwrap(), Some(rec(4)));
        assert_eq!(a.get(RecordId(6)).unwrap(), Some(rec(6)));
        assert_eq!(a.live(), 9);
    }

    #[test]
    fn reinserting_a_removed_id_does_not_double_count() {
        let mut a = DirectArray::new(fixed()).unwrap();
        a.put(RecordId(1), &rec(1)).unwrap();
        a.remove(RecordId(1));
        a.put(RecordId(1), &rec(1)).unwrap();
        assert_eq!(a.live(), 1);
    }

    #[test]
    fn overwriting_the_same_id_does_not_double_count() {
        let mut a = DirectArray::new(fixed()).unwrap();
        for _ in 0..5 {
            a.put(RecordId(3), &rec(3)).unwrap();
        }
        assert_eq!(a.live(), 1);
    }

    #[test]
    fn density_reflects_how_much_of_the_range_is_used() {
        let mut dense = DirectArray::new(fixed()).unwrap();
        for i in 0..100u64 {
            dense.put(RecordId(i), &rec(i)).unwrap();
        }
        assert!((dense.density() - 1.0).abs() < 1e-9);

        // Ten records spread across a million-id range: the representation
        // would allocate a million slots to hold ten records.
        let mut sparse = DirectArray::new(fixed()).unwrap();
        for i in 0..10u64 {
            sparse.put(RecordId(i * 100_000), &rec(i)).unwrap();
        }
        assert!(sparse.density() < 0.0001, "{}", sparse.density());
        assert_eq!(DirectArray::new(fixed()).unwrap().density(), 0.0);
    }

    #[test]
    fn a_wrong_sized_write_is_rejected_rather_than_corrupting_neighbours() {
        let mut a = DirectArray::new(fixed()).unwrap();
        assert!(a.put_encoded(RecordId(0), &[0u8; 3]).is_err());
        assert!(a.put_encoded(RecordId(0), &vec![0u8; 10_000]).is_err());
    }

    #[test]
    fn rebuild_reconstructs_the_whole_array() {
        let rows: Vec<(RecordId, Record)> = (0..50u64).map(|i| (RecordId(i), rec(i))).collect();
        let a = DirectArray::rebuild(fixed(), rows.iter().map(|(i, r)| (*i, r)))
            .unwrap()
            .unwrap();
        assert_eq!(a.live(), 50);
        for i in 0..50u64 {
            assert_eq!(a.get(RecordId(i)).unwrap(), Some(rec(i)));
        }
    }

    #[test]
    fn rebuild_declines_a_schema_it_cannot_address() {
        let rows: Vec<(RecordId, Record)> = vec![];
        assert!(
            DirectArray::rebuild(Schema::dynamic(), rows.iter().map(|(i, r)| (*i, r)))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_sparse_high_id_does_not_lose_low_ids() {
        let mut a = DirectArray::new(fixed()).unwrap();
        a.put(RecordId(0), &rec(0)).unwrap();
        a.put(RecordId(10_000), &rec(10_000)).unwrap();
        assert_eq!(a.get(RecordId(0)).unwrap(), Some(rec(0)));
        assert_eq!(a.get(RecordId(10_000)).unwrap(), Some(rec(10_000)));
        assert_eq!(a.get(RecordId(5_000)).unwrap(), None);
        assert_eq!(a.live(), 2);
        assert_eq!(a.capacity(), 10_001);
    }
}

#[cfg(test)]
mod field_tests {
    use super::*;
    use adabt_core::schema::{FieldDef, FieldType, SchemaMode};

    fn schema() -> Schema {
        Schema::new(
            SchemaMode::Fixed,
            vec![
                FieldDef::new("id", FieldType::U64).required(),
                FieldDef::new("balance", FieldType::I64).required(),
                FieldDef::new("active", FieldType::Bool),
                FieldDef::new("name", FieldType::Char(24)),
            ],
        )
        .unwrap()
    }

    fn filled(n: u64) -> DirectArray {
        let mut a = DirectArray::new(schema()).unwrap();
        for i in 0..n {
            a.put(
                RecordId(i),
                &Record::new()
                    .with("id", i)
                    .with("balance", (i as i64) * 3)
                    .with("active", i % 2 == 0)
                    .with("name", format!("n{i}")),
            )
            .unwrap();
        }
        a
    }

    #[test]
    fn a_single_field_read_matches_a_full_decode() {
        // The specialisation must be a shortcut, not a different answer.
        let a = filled(200);
        for i in 0..200u64 {
            let whole = a.get(RecordId(i)).unwrap().unwrap();
            for field in ["id", "balance", "active", "name"] {
                assert_eq!(
                    a.field_at(RecordId(i), field).unwrap().as_ref(),
                    whole.get(field),
                    "record {i} field {field}"
                );
            }
        }
    }

    #[test]
    fn every_field_offset_is_read_correctly_not_just_the_first() {
        // An offset error would give plausible-looking wrong values for later
        // fields while the first one still looked fine.
        let a = filled(50);
        assert_eq!(a.field_at(RecordId(7), "id").unwrap(), Some(Value::U64(7)));
        assert_eq!(
            a.field_at(RecordId(7), "balance").unwrap(),
            Some(Value::I64(21))
        );
        assert_eq!(
            a.field_at(RecordId(7), "active").unwrap(),
            Some(Value::Bool(false))
        );
        assert_eq!(
            a.field_at(RecordId(7), "name").unwrap(),
            Some(Value::Str("n7".into()))
        );
    }

    #[test]
    fn an_absent_field_is_absent_without_being_read() {
        let mut a = DirectArray::new(schema()).unwrap();
        a.put(
            RecordId(1),
            &Record::new().with("id", 1u64).with("balance", 0i64),
        )
        .unwrap();
        assert_eq!(a.field_at(RecordId(1), "name").unwrap(), None);
        assert_eq!(a.field_at(RecordId(1), "id").unwrap(), Some(Value::U64(1)));
    }

    #[test]
    fn a_missing_record_and_an_unknown_field_both_report_nothing() {
        let a = filled(10);
        assert_eq!(a.field_at(RecordId(999), "id").unwrap(), None);
        assert_eq!(a.field_at(RecordId(1), "nonexistent").unwrap(), None);
    }

    #[test]
    fn neighbouring_records_are_not_disturbed_by_a_field_read() {
        let a = filled(20);
        assert_eq!(a.field_at(RecordId(0), "id").unwrap(), Some(Value::U64(0)));
        assert_eq!(
            a.field_at(RecordId(19), "id").unwrap(),
            Some(Value::U64(19))
        );
        // And the value read for one id is not the value of its neighbour.
        for i in 0..20u64 {
            assert_eq!(a.field_at(RecordId(i), "id").unwrap(), Some(Value::U64(i)));
        }
    }
}
