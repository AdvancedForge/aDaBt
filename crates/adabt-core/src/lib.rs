//! Shared type layer for the aDaBt engine.
//!
//! This crate has no dependencies on any storage, index or execution code. It
//! defines the logical vocabulary — values, records, schemas, identifiers,
//! policy — plus the `LogicalStore` contract that the physical layers must
//! preserve no matter how aggressively they specialise.

pub mod error;
pub mod ids;
pub mod policy;
pub mod record;
pub mod schema;
pub mod store;
pub mod value;

pub use error::{Error, Result, SchemaError};
pub use ids::{CollectionId, IndexId, Lsn, RecordId, RepId, TxnId, ViewId};
pub use policy::{
    Consistency, Constraints, Durability, GuaranteeRequirements, Guarantees, Mode, Policy,
    Priorities,
};
pub use record::Record;
pub use schema::{FieldDef, FieldType, Schema, SchemaMode};
pub use store::LogicalStore;
pub use value::Value;
