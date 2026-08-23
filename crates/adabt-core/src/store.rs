//! The logical API contract.
//!
//! This trait is the user's stable surface. Everything below it — heap files,
//! column stores, caches, directly-addressed fixed arrays — may be replaced at
//! runtime without any signature here changing. That invariance is the whole
//! point of the project, so this trait is deliberately hard to extend: adding
//! to it is a decision about the *logical* database, never about optimization.
//!
//! Both the reference model and the real engine implement it, which is what
//! makes differential testing across optimization levels possible.

use crate::error::Result;
use crate::ids::RecordId;
use crate::record::Record;
use crate::schema::Schema;

pub trait LogicalStore {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()>;
    fn drop_collection(&mut self, name: &str) -> Result<()>;
    fn collection_names(&self) -> Vec<String>;
    fn schema_of(&self, collection: &str) -> Result<&Schema>;

    /// Insert, failing if `id` is already present.
    fn insert(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<()>;

    fn get(&self, collection: &str, id: RecordId) -> Result<Option<Record>>;

    /// Replace an existing record. Returns whether it existed.
    fn update(&mut self, collection: &str, id: RecordId, rec: Record) -> Result<bool>;

    /// Returns whether the record existed.
    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool>;

    /// Full scan in ascending `RecordId` order.
    ///
    /// Order is part of the contract: differential testing compares scan output
    /// directly, and an engine that returned physical order would diverge from
    /// the reference model for reasons that are not bugs.
    fn scan(&self, collection: &str) -> Result<Vec<(RecordId, Record)>>;

    fn count(&self, collection: &str) -> Result<usize>;
}
