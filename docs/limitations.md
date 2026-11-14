# Limitations

Status: `0.1.0-alpha.1` — experimental, not production-ready. API and on-disk
format stability are not promised until beta (`docs/semver.md` describes the
version gate that exists today).

This file is the authoritative list of what aDaBt does not do. Where another
document (including the goal status in `docs/roadmap.md`) seems to say more, this
file wins.

## What it is
- A RAM-resident, single-node Rust database (embedded, or behind `adabt-server`)
- Adaptive physical optimization: 15 built-in optimizations on a cumulative level ladder 0–11 (`docs/architecture.md`)
- Cross-shard writes through a serialized coordinator with crash-convergent recovery and a documented visibility window
- Snapshot-isolated transactions; `Strict` makes them serializable by validating point reads and scans at commit (closes write skew and predicate phantoms; a scan conflicts with any concurrent write to its collection)

## What it is not (and will not become without a new roadmap)
- **Replicated or highly available** — permanently out of scope (`docs/replication-decision.md`)
- **Out-of-core** — the page directory and every index stay resident (~470 B/row, `docs/benchmarks/scale-decision.md`); more data means more shards

## Known gaps
- **Full SQL** — a `SELECT` subset only; writes and DDL go through the Rust API
- **Atomic cross-shard visibility** — coordinator-decides durability (`XSH1` journal), not 2PC: other connections can see shards disagree inside the commit window
- **DDL is not transactional** — `CreateCollection`/`DropCollection`/schema changes commit on write, and sit outside `Strict`'s guarantee: a collection dropped and re-created under a running transaction is judged only by writes to the new one
- **Point-in-time restore over a newer heap file** — `open_at` replays the log up to the target over the heap file as it stands. The buffer pool writes dirty pages to that file on eviction, so a page carrying writes from after the target may already be there, and those writes show through (transactional or not). It is sound when no page written after the target has reached the heap file; restoring from a base image older than the target is not yet supported
- **Persistent thread-per-core workers** — burst pinning only (`core_affinity` per query)
- **Write throughput** — bulk load is ~11× slower than SQLite and single-row inserts also lose; per-record MVCC + WAL is the price (`docs/benchmarks/comparison-notes.md`)
- **Comparison witnesses** — SQLite only; the PostgreSQL and RocksDB witnesses are planned and the harness exits 2 rather than fake them
- **`loom` coverage** — `adabt-storage`'s transaction-id allocator is model-checked; `adabt-engine` does not yet compile with `--features loom` (loom's `Arc` meets std's at crate boundaries), so the engine has no model checks
- **C ABI** — open/close, create-collection, i64 field put/get, count; nothing more

## Current guarantees
- Catalog v4 (`FORMAT_VERSION`): reads v3; an unreadable catalog rebuilds from a complete WAL
- Superblock version gate refuses a newer on-disk format
- Crash recovery: WAL replay, torn-tail safe
- Transactions and `insert_batch` are crash-atomic: the write-set is logged under one commit record, made durable once, and applied to pages only after; recovery applies it whole or not at all (`commit_window_chaos.rs`, `atomic_commit.rs`). Under `Strict` this holds through power loss; under `GroupCommit`/`Relaxed`, through a process crash
- Cross-shard writes: crash-convergent (`commit_coordinated` holds a serialized lock; `open` re-drives pending writes)
- Optimizations never change answers (differential runner + per-level query tests)

See `docs/getting-started.md` for the quickstart and `docs/benchmarks/` for comparison results.
