# Limitations

Status: `0.1.0-alpha.1` — experimental, not production-ready. API and on-disk
format stability are not promised until beta (`docs/semver.md` describes the
version gate that exists today).

This file is the authoritative list of what aDaBt does not do. Where another
document (including `docs/roadmap.md`'s "100%" scores) seems to say more, this
file wins.

## What it is
- A RAM-resident, single-node Rust database (embedded, or behind `adabt-server`)
- Adaptive physical optimization: 17 built-in optimizations on a cumulative level ladder 0–11 (`docs/architecture.md`)
- Cross-shard writes through a serialized coordinator with crash-convergent recovery and a documented visibility window
- Snapshot-isolated transactions; `Strict` additionally validates point reads (closes write skew)

## What it is not (and will not become without a new roadmap)
- **Replicated or highly available** — permanently out of scope (`docs/replication-decision.md`)
- **Out-of-core** — the page directory and every index stay resident (~470 B/row, `docs/benchmarks/scale-decision.md`); more data means more shards

## Known gaps
- **Full SQL** — a `SELECT` subset only; writes and DDL go through the Rust API
- **Full serializable isolation** — `Strict` validates point reads; predicate phantoms remain possible (`serializable.rs` documents this)
- **Atomic cross-shard visibility** — coordinator-decides durability (`XSH1` journal), not 2PC: other connections can see shards disagree inside the commit window
- **DDL is not transactional** — `CreateCollection`/`DropCollection`/schema changes commit on write
- **Persistent thread-per-core workers** — burst pinning only (`core_affinity` per query)
- **Write throughput** — bulk load is ~11× slower than SQLite and single-row inserts also lose; per-record MVCC + WAL is the price (`docs/benchmarks/comparison-notes.md`)
- **Comparison witnesses** — SQLite only; the PostgreSQL and RocksDB witnesses are planned and the harness exits 2 rather than fake them
- **`loom` coverage** — `adabt-storage` is model-checked; `adabt-engine` is feature-gated but cross-crate `Arc`/`Mutex` substitution needs broader design work
- **C ABI** — open/close, create-collection, i64 field put/get, count; nothing more
- **CI** — no hosted CI; the gate is `scripts/check.sh`, optionally run by the pre-push hook

## Current guarantees
- Catalog v4 (`FORMAT_VERSION`): reads v3; an unreadable catalog rebuilds from a complete WAL
- Superblock version gate refuses a newer on-disk format
- Crash recovery: WAL replay, torn-tail safe
- Cross-shard writes: crash-convergent (`commit_coordinated` holds a serialized lock; `open` re-drives pending writes)
- Optimizations never change answers (differential runner + per-level query tests)

See `docs/getting-started.md` for the quickstart and `docs/benchmarks/` for comparison results.
