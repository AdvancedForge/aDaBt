# Getting Started

Experimental alpha (`0.1.0-alpha.1`). RAM-resident, single-node Rust database with adaptive physical optimization.

## Five-minute quickstart

```rust
use adabt_engine::{Database, ManualPolicy};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::open("/tmp/demo", ManualPolicy::default())?;
    db.execute("CREATE COLLECTION users").unwrap();
    db.execute("INSERT INTO users (name) VALUES ('Alice')").unwrap();
    let rows = db.execute("SELECT * FROM users").unwrap();
    println!("{:?}", rows);
    Ok(())
}
```

## Adaptive optimization

Start with `ManualPolicy::default()` or use adaptive mode. The optimizer creates indexes, covering indexes, columnar scans and materialized views based on telemetry. Print decisions with `db.optimize()`.

## Server

```bash
adabt-server --cert server.crt --key server.key --port 8443
```

Connect with TLS and static admin/reader tokens.

## Limitations (see `limitations.md`)

Not production-ready. No replication. Memory-bound. Predicate phantoms remain possible under strict reads. SQL subset only.
