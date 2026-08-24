//! The codec, checked against the reference model.
//!
//! `CodecStore` keeps records as encoded bytes and decodes on every read, so
//! any information the codec loses shows up as a disagreement with the
//! reference model within a few hundred operations. It is the first real
//! customer of the differential rig, and the pattern every later physical
//! layer follows.

use adabt_core::error::Result;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::{normalize_for_storage, LogicalStore};
use adabt_core::Error;
use adabt_storage::codec::RecordCodec;
use adabt_testkit::differential::{run, seeds};
use adabt_testkit::generator::GenConfig;
use adabt_testkit::reference::ReferenceStore;
use std::collections::BTreeMap;

struct Collection {
    codec: RecordCodec,
    records: BTreeMap<RecordId, Vec<u8>>,
}

/// A store that round-trips every record through the codec.
#[derive(Default)]
struct CodecStore {
    collections: BTreeMap<String, Collection>,
}

impl CodecStore {
    fn coll(&self, name: &str) -> Result<&Collection> {
        self.collections
            .get(name)
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }
    fn coll_mut(&mut self, name: &str) -> Result<&mut Collection> {
        self.collections
            .get_mut(name)
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }
}

impl LogicalStore for CodecStore {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        if self.collections.contains_key(name) {
            return Err(Error::CollectionExists(name.to_string()));
        }
        self.collections.insert(
            name.to_string(),
            Collection {
                codec: RecordCodec::new(schema),
                records: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn drop_collection(&mut self, name: &str) -> Result<()> {
        self.collections
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }

    fn collection_names(&self) -> Vec<String> {
        self.collections.keys().cloned().collect()
    }

    fn schema_of(&self, collection: &str) -> Result<&Schema> {
        Ok(self.coll(collection)?.codec.schema())
    }

    fn insert(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<()> {
        normalize_for_storage(&mut rec);
        let c = self.coll_mut(collection)?;
        c.codec.schema().validate_record(&rec)?;
        if c.records.contains_key(&id) {
            return Err(Error::RecordExists(id));
        }
        let bytes = c.codec.encode(&rec)?;
        c.records.insert(id, bytes);
        Ok(())
    }

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        let c = self.coll(collection)?;
        match c.records.get(&id) {
            None => Ok(None),
            Some(b) => Ok(Some(c.codec.decode(b)?)),
        }
    }

    fn update(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<bool> {
        normalize_for_storage(&mut rec);
        let c = self.coll_mut(collection)?;
        c.codec.schema().validate_record(&rec)?;
        let bytes = c.codec.encode(&rec)?;
        Ok(c.records.insert(id, bytes).is_some())
    }

    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        Ok(self.coll_mut(collection)?.records.remove(&id).is_some())
    }

    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        let c = self.coll(collection)?;
        c.records
            .iter()
            .map(|(id, b)| Ok((*id, c.codec.decode(b)?)))
            .collect()
    }

    fn count(&mut self, collection: &str) -> Result<usize> {
        Ok(self.coll(collection)?.records.len())
    }
}

/// One collection per schema mode, so a single run exercises all four layouts.
fn config() -> GenConfig {
    GenConfig::with_collections(vec![
        (
            "fixed".into(),
            Schema::new(
                SchemaMode::Fixed,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    FieldDef::new("balance", FieldType::I64),
                    FieldDef::new("active", FieldType::Bool),
                    FieldDef::new("name", FieldType::Char(24)),
                ],
            )
            .unwrap(),
        ),
        (
            "strict".into(),
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
            .unwrap(),
        ),
        (
            "declared".into(),
            Schema::new(
                SchemaMode::Declared,
                vec![
                    FieldDef::new("id", FieldType::U64).required(),
                    FieldDef::new("note", FieldType::Str { max_len: Some(64) }),
                ],
            )
            .unwrap(),
        ),
        ("dynamic".into(), Schema::dynamic()),
    ])
}

fn fresh(cfg: &GenConfig) -> (ReferenceStore, CodecStore) {
    let (mut a, mut b) = (ReferenceStore::new(), CodecStore::default());
    for (name, schema) in &cfg.collections {
        a.create_collection(name, schema.clone()).unwrap();
        b.create_collection(name, schema.clone()).unwrap();
    }
    (a, b)
}

#[test]
fn codec_matches_the_reference_model_across_all_schema_modes() {
    let cfg = config();
    for seed in seeds(0x0DEC0DE, 48) {
        let (mut a, mut b) = fresh(&cfg);
        run(&mut a, &mut b, "reference", "codec", &cfg, seed, 600)
            .unwrap_or_else(|d| panic!("{d}"));
    }
}

#[test]
fn codec_survives_a_long_single_seed_run() {
    let cfg = config();
    let (mut a, mut b) = fresh(&cfg);
    run(&mut a, &mut b, "reference", "codec", &cfg, 0xABCDEF, 20_000)
        .unwrap_or_else(|d| panic!("{d}"));
}
