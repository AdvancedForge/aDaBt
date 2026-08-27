# Getting Started

**Status:** `0.1.0-alpha.1` — experimental, not production-ready. See `limitations.md`.

## Five-minute embedded quickstart

```rust
use std::path::Path;
use adabt_core::record::Record;
use adabt_engine::{Database};
use adabt_core::policy::{Policy, Mode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Policy: conventional (strict durability, strict consistency, adaptive disabled)
    let policy = Policy::conventional();

    // Open (directory is created automatically; uses real Path, not a string)
    let mut db = Database::open(Path::new("/tmp/demo"), policy)?;

    // Create a dynamic-schema collection
    use adabt_core::schema::{Schema, SchemaMode};
    db.create_collection("users", Schema::dynamic())?;

    // Insert via Record
    let mut rec = Record::new();
    rec.set("name", "Alice");
    db.insert_auto("users", rec)?;

    // Query: use a logical plan (subset SQL; full SQL not supported)
    use adabt_ir::plan::LogicalPlan;
    // Actual query API requires LogicalPlan construction; see architecture.md
    // for the planner interface. The quick version:
    println!("See docs/architecture.md for query examples.");

    Ok(())
}
```

Notes:
- `Database::open` takes `&Path`, not `&str`.
- `Policy::conventional()` is the safe default; `Policy::manual(level)` selects an optimization ladder level.
- SQL subset only: no arbitrary `CREATE`/`INSERT` string statements; use `Schema`, `Record`, `insert_auto`.
- See `docs/architecture.md` for planner and optimization demonstration.

## Adaptive optimization

Start with `Policy::conventional()` or `Policy::manual(level)`. The optimizer creates indexes, covering indexes, columnar scans and materialized views based on telemetry (`docs/benchmarks/`). Print decisions with `db.optimize()`.

## Server quickstart

```bash
adabt-server \
  --data ./demo-data \
  --listen 127.0.0.1:8443 \
  --tls-cert server.crt \
  --tls-key server.key
```

Required: `--data`. Flags: `--listen`, `--tls-cert`, `--tls-key` (not `--cert`, `--key`, `--port`).

Connect with TLS and static admin/reader tokens.

## Limitations

Not production-ready. See `docs/limitations.md`:
- No replication (permanently out of scope)
- Memory-bound (resident; no disk overflow)
- Predicate phantoms remain under `Strict`
- Cross-shard writes: coordinator-decides durability (`XSH1`), not atomic 2PC
