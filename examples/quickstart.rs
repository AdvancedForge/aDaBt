//! Quickstart: open, create, insert, query, close.
//!
//! Run with: cargo run --example quickstart

use std::path::Path;
use adabt_core::record::Record;
use adabt_engine::Database;
use adabt_core::policy::Policy;
use adabt_core::schema::{Schema, SchemaMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let policy = Policy::conventional();
    let mut db = Database::open(Path::new("/tmp/demo"), policy)?;

    db.create_collection("users", Schema::dynamic())?;

    let mut rec = Record::new();
    rec.set("name", "Alice");
    db.insert_auto("users", rec)?;

    println!("Inserted Alice into users.");
    println!("See docs/getting-started.md and docs/architecture.md for full query examples.");

    Ok(())
}
