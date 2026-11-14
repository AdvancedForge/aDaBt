# Getting Started

**Status:** `0.1.0-alpha.1` — experimental, not production-ready. See `docs/limitations.md`.

Requires a C linker and Rust 1.82+.

## Embedded: two runnable examples

```sh
cargo run -p adabt-engine --example quickstart          # open, batch insert, query a logical plan, explain it
cargo run -p adabt-engine --example watch_it_optimize   # same queries before and after; the plan changes, answers don't
```

The sources are in `crates/adabt-engine/examples/`. The core of the quickstart:

```rust
let mut policy = Policy::manual(0);                    // level 0: conventional, nothing specialised
policy.guarantees.durability = Durability::Relaxed;    // fast demo; keep Strict when data matters
let mut db = Database::open(&dir, policy)?;            // &Path

db.create_collection("users", Schema::dynamic())?;
db.insert_batch("users", batch)?;                      // Vec<(RecordId, Record)>

let plan = LogicalPlan::new(
    LogicalOp::scan("users")
        .filter(Expr::eq("country", "NO"))
        .project(vec!["age".into()]),
);
let rows = db.query(&plan)?;
println!("{}", db.explain(&plan));
```

Notes:
- Queries from Rust are logical plans (`adabt-ir`); there is no SQL string API in the engine. The `adabt-cli` shell accepts a `SELECT` subset.
- `Policy::conventional()` is the safe default; `Policy::manual(level)` selects a ladder level (0–11); set `policy.mode = Mode::Adaptive` to let the engine choose.
- `db.optimize()` applies what the policy allows and returns a report; `db.explain_optimizations()` prints the decision log.

## Server quickstart

```bash
cargo run -p adabt-server --release -- \
  --data ./demo-data \
  --listen 127.0.0.1:8443 \
  --auth-token "$ADMIN_TOKEN" \
  --tls-cert server.crt \
  --tls-key server.key
```

Required: `--data`. Other flags: `--listen`, `--level`, `--adaptive`, `--shards`,
`--auth-token`, `--read-token`, `--tls-cert`, `--tls-key`, `--max-connections`,
`--idle-timeout-secs`, `--slow-query-log-ms`, `--version`, `--help`.

## Next

`docs/architecture.md` for how it fits together, `docs/api.md` for the surface,
`docs/operations.md` for running it, `docs/limitations.md` before relying on it.
