//! Deterministic operation-sequence generation.
//!
//! The generator deliberately produces schema-*invalid* records some of the
//! time. Agreement on the happy path is easy; the valuable property is that two
//! implementations reject the same writes for the same reason, because that is
//! where an optimized physical layout is most likely to quietly diverge.

use crate::ops::Op;
use crate::rng::Rng;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::{FieldType, Schema, SchemaMode};
use adabt_core::value::Value;

#[derive(Debug, Clone)]
pub struct OpWeights {
    pub insert: u32,
    pub get: u32,
    pub update: u32,
    pub delete: u32,
    pub scan: u32,
    pub count: u32,
}

impl Default for OpWeights {
    fn default() -> Self {
        // Write-leaning so collections actually fill up; scans are rare because
        // they are O(n) and would dominate long runs.
        Self {
            insert: 30,
            get: 30,
            update: 15,
            delete: 10,
            scan: 3,
            count: 2,
        }
    }
}

impl OpWeights {
    fn total(&self) -> u32 {
        self.insert + self.get + self.update + self.delete + self.scan + self.count
    }
}

#[derive(Debug, Clone)]
pub struct GenConfig {
    pub collections: Vec<(String, Schema)>,
    /// Ids are drawn from `[0, id_space)`. A small space forces collisions,
    /// which is what exercises duplicate-insert and overwrite paths.
    pub id_space: u64,
    pub weights: OpWeights,
    /// Probability that a generated record deliberately violates its schema.
    pub invalid_rate: f64,
    /// Probability that an operation names a collection that does not exist.
    pub missing_collection_rate: f64,
}

impl Default for GenConfig {
    fn default() -> Self {
        Self {
            collections: Vec::new(),
            id_space: 64,
            weights: OpWeights::default(),
            invalid_rate: 0.08,
            missing_collection_rate: 0.02,
        }
    }
}

impl GenConfig {
    pub fn with_collections(collections: Vec<(String, Schema)>) -> Self {
        Self {
            collections,
            ..Default::default()
        }
    }
}

pub struct Generator<'a> {
    cfg: &'a GenConfig,
    rng: Rng,
}

impl<'a> Generator<'a> {
    pub fn new(cfg: &'a GenConfig, seed: u64) -> Self {
        assert!(
            !cfg.collections.is_empty(),
            "generator needs at least one collection"
        );
        assert!(cfg.id_space > 0, "id_space must be positive");
        Self {
            cfg,
            rng: Rng::new(seed),
        }
    }

    fn value_for(&mut self, ty: &FieldType, valid: bool) -> Value {
        if !valid {
            // A value of the wrong shape for almost any declared type.
            return match self.rng.below(3) {
                0 => Value::Str("!!invalid!!".into()),
                1 => Value::List(vec![Value::Null]),
                _ => Value::Bool(true),
            };
        }
        match ty {
            FieldType::Bool => Value::Bool(self.rng.chance(0.5)),
            FieldType::I64 => Value::I64(self.rng.below(1000) as i64 - 500),
            FieldType::U64 => Value::U64(self.rng.below(1000)),
            FieldType::F64 => Value::F64(self.rng.below(1000) as f64 / 8.0),
            FieldType::Timestamp => Value::Timestamp(self.rng.below(1 << 40) as i64),
            // Occasionally an integer, because a decimal field accepts one and
            // the conversion is exactly the kind of thing worth exercising.
            FieldType::Decimal { scale } => {
                if self.rng.chance(0.2) {
                    Value::I64(self.rng.below(1000) as i64 - 500)
                } else {
                    Value::Decimal {
                        units: self.rng.below(1_000_000) as i128 - 500_000,
                        scale: *scale,
                    }
                }
            }
            // Draw against content capacity, not slot width: part of a
            // fixed-width slot is spent on its inline length prefix, so
            // generating up to `w` would silently inflate the invalid rate.
            FieldType::Char(w) => {
                let cap = FieldType::Char(*w).content_capacity().unwrap_or(0) as u64;
                let n = self.rng.below(cap + 1) as usize;
                Value::Str("x".repeat(n))
            }
            FieldType::FixedBytes(w) => {
                let cap = FieldType::FixedBytes(*w).content_capacity().unwrap_or(0) as u64;
                let n = self.rng.below(cap + 1) as usize;
                Value::Bytes(vec![7u8; n])
            }
            FieldType::Str { max_len } => {
                let cap = max_len.unwrap_or(24) as u64;
                Value::Str("s".repeat(self.rng.below(cap + 1) as usize))
            }
            FieldType::Bytes { max_len } => {
                let cap = max_len.unwrap_or(24) as u64;
                Value::Bytes(vec![3u8; self.rng.below(cap + 1) as usize])
            }
            FieldType::List(inner) => {
                let n = self.rng.below(4);
                let items = (0..n).map(|_| self.value_for(inner, true)).collect();
                Value::List(items)
            }
            FieldType::Map => Value::Map(Default::default()),
            FieldType::Any => Value::I64(self.rng.below(100) as i64),
        }
    }

    fn record_for(&mut self, schema: &Schema) -> Record {
        let invalid = self.rng.chance(self.cfg.invalid_rate);
        let mut r = Record::new();

        if schema.mode() == SchemaMode::Dynamic {
            // No declared fields: invent some.
            for i in 0..self.rng.below(4) + 1 {
                let v = self.value_for(&FieldType::Any, true);
                r.set(format!("f{i}"), v);
            }
            return r;
        }

        // Pick at most one field to corrupt, so the failure reason is unambiguous.
        let corrupt_idx = if invalid && !schema.fields().is_empty() {
            Some(self.rng.below_usize(schema.fields().len()))
        } else {
            None
        };

        let defs: Vec<(String, FieldType, bool)> = schema
            .fields()
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone(), f.nullable))
            .collect();

        for (i, (name, ty, nullable)) in defs.into_iter().enumerate() {
            if Some(i) == corrupt_idx {
                let v = self.value_for(&ty, false);
                r.set(name, v);
                continue;
            }
            // Occasionally omit a nullable field; omitting a required one is a
            // schema violation the generator produces only via `corrupt_idx`.
            if nullable && self.rng.chance(0.15) {
                continue;
            }
            // And occasionally set it to an *explicit* null. Whether that is
            // distinguishable from omission is a real semantic question, so the
            // generator must actually ask it rather than quietly never trying.
            if nullable && self.rng.chance(0.15) {
                r.set(name, Value::Null);
                continue;
            }
            let v = self.value_for(&ty, true);
            r.set(name, v);
        }

        if invalid && corrupt_idx.is_none() && !schema.mode().allows_extra_fields() {
            r.set("__stowaway", Value::I64(1));
        }
        r
    }

    fn pick_collection(&mut self) -> (String, Option<Schema>) {
        if self.rng.chance(self.cfg.missing_collection_rate) {
            return ("__no_such_collection".to_string(), None);
        }
        let i = self.rng.below_usize(self.cfg.collections.len());
        let (name, schema) = &self.cfg.collections[i];
        (name.clone(), Some(schema.clone()))
    }

    pub fn next_op(&mut self) -> Op {
        let (collection, schema) = self.pick_collection();
        let id = RecordId(self.rng.below(self.cfg.id_space));
        let w = &self.cfg.weights;
        let mut pick = self.rng.below(w.total() as u64) as u32;

        for (weight, kind) in [
            (w.insert, 0u8),
            (w.get, 1),
            (w.update, 2),
            (w.delete, 3),
            (w.scan, 4),
            (w.count, 5),
        ] {
            if pick < weight {
                return match kind {
                    0 => {
                        let rec = schema
                            .as_ref()
                            .map(|s| self.record_for(s))
                            .unwrap_or_default();
                        Op::Insert {
                            collection,
                            id,
                            rec,
                        }
                    }
                    1 => Op::Get { collection, id },
                    2 => {
                        let rec = schema
                            .as_ref()
                            .map(|s| self.record_for(s))
                            .unwrap_or_default();
                        Op::Update {
                            collection,
                            id,
                            rec,
                        }
                    }
                    3 => Op::Delete { collection, id },
                    4 => Op::Scan { collection },
                    _ => Op::Count { collection },
                };
            }
            pick -= weight;
        }
        unreachable!("weights sum to total()")
    }

    pub fn take(&mut self, n: usize) -> Vec<Op> {
        (0..n).map(|_| self.next_op()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::schema::{FieldDef, SchemaMode};

    fn cfg() -> GenConfig {
        GenConfig::with_collections(vec![(
            "users".into(),
            Schema::new(
                SchemaMode::Strict,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    FieldDef::new("name", FieldType::Char(8)),
                ],
            )
            .unwrap(),
        )])
    }

    #[test]
    fn generation_is_reproducible_from_the_seed() {
        let c = cfg();
        let a = Generator::new(&c, 1234).take(500);
        let b = Generator::new(&c, 1234).take(500);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_produce_different_sequences() {
        let c = cfg();
        assert_ne!(
            Generator::new(&c, 1).take(200),
            Generator::new(&c, 2).take(200)
        );
    }

    #[test]
    fn every_operation_kind_is_reachable() {
        let c = cfg();
        let ops = Generator::new(&c, 99).take(5_000);
        for want in ["insert", "get", "update", "delete", "scan", "count"] {
            assert!(
                ops.iter().any(|o| o.name() == want),
                "never generated {want}"
            );
        }
    }

    #[test]
    fn ids_stay_within_the_configured_space() {
        let mut c = cfg();
        c.id_space = 8;
        for op in Generator::new(&c, 5).take(1_000) {
            if let Op::Get { id, .. } | Op::Insert { id, .. } = op {
                assert!(id.0 < 8, "id {id} outside space");
            }
        }
    }

    #[test]
    fn invalid_records_are_actually_produced() {
        let c = cfg();
        let schema = &c.collections[0].1;
        let ops = Generator::new(&c, 7).take(2_000);
        let rejected = ops
            .iter()
            .filter_map(|o| match o {
                Op::Insert { rec, .. } => Some(rec),
                _ => None,
            })
            .filter(|r| schema.validate_record(r).is_err())
            .count();
        assert!(
            rejected > 0,
            "generator never produced a schema-invalid record"
        );
    }

    #[test]
    fn missing_collections_are_actually_referenced() {
        let c = cfg();
        let ops = Generator::new(&c, 11).take(2_000);
        assert!(ops.iter().any(|o| o.collection() == "__no_such_collection"));
    }
}
