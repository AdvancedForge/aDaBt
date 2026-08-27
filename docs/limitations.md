# Limitations

Status: `0.1.0-alpha.1` — experimental, not production-ready.

## What it is
- RAM-resident single-node Rust database
- Adaptive physical optimization (17 built-in optimizers, level ladder 1-6)
- Safe cross-shard coordinated writes (serialized coordinator, windowed visibility)
- Strict read-set validation (closes some write-skew, not full serializable isolation)

## What it is not (and will not become without a new roadmap)
- Replication or multi-machine HA — permanently out of scope (`docs/history/replication-decision.md`)
- Disk overflow / out-of-core datasets — resident design; more data → more shards
- Full SQL — subset only
- Full serializable isolation — `Strict` validates point reads; predicate phantoms remain possible
- Persistent thread-per-core workers — burst pinning only (`core_affinity` per query)
- Atomic cross-shard visibility — coordinator-decides durability (`XSH1`), not 2PC

## Current guarantees
- Catalog v4 (`FORMAT_VERSION`): forward-compatible, backward-compatible with v3
- Crash recovery: WAL replay, torn-tail safe
- Cross-shard writes: crash-convergent (`commit_coordinated` holds serialized lock)

See `docs/getting-started.md` for quickstart and `docs/benchmarks/` for comparison results.
