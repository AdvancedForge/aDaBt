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
- **Point-in-time recovery needs a backup** — `open_at` restores only from a base image at or before the target: a backup restored with `restore_from` and given the archived log (`docs/operations.md`). It refuses a live data directory once the buffer pool has written to its heap file since the last checkpoint, instead of restoring in place. Moving between timelines is manual: a restored database continues its log at target + 1, needs an archive directory of its own, and keeps the history it superseded in `superseded-log/` for an operator to deal with
- **Persistent thread-per-core workers** — burst pinning only (`core_affinity` per query)
- **Write throughput** — bulk load is ~11× slower than SQLite and single-row inserts also lose; per-record MVCC + WAL is the price (`docs/benchmarks/comparison-notes.md`)
- **Comparison witnesses** — SQLite only; the PostgreSQL and RocksDB witnesses are planned and the harness exits 2 rather than fake them
- **`loom` coverage** — `adabt-storage`'s transaction-id allocator is model-checked; `adabt-engine` does not yet compile with `--features loom` (loom's `Arc` meets std's at crate boundaries), so the engine has no model checks
- **C ABI** — open/close, create-collection, i64 field put/get, count; nothing more

## Current guarantees
- Catalog v4 (`FORMAT_VERSION`): reads v3; an unreadable catalog rebuilds from a complete WAL
- Superblock version gate refuses a newer on-disk format
- Crash recovery returns exactly the committed state — no deleted record, no superseded version — whatever the buffer pool wrote to the heap file before the crash: it starts from the last checkpoint's page-directory snapshot and replays the log, torn-tail safe (`crash_recovery.rs` and `crash_consistency.rs` compare crash points against a model under a two- and three-page pool). Like the catalog, the snapshot (`directory.adabt`) is authoritative: a checkpointed database without a readable one is refused rather than opened from a page scan
- Point-in-time recovery from a backup plus archived log returns exactly the committed state at the target LSN, whatever the buffer pool wrote to the source's heap file. It refuses, and changes nothing, if the base image or a log segment is missing (`backup_restore.rs`, `backup.rs` compare every LSN against a reference model under a two-page pool)
- Transactions and `insert_batch` are crash-atomic: the write-set is logged under one commit record, made durable once, and applied to pages only after; recovery applies it whole or not at all (`commit_window_chaos.rs`, `atomic_commit.rs`). Under `Strict` this holds through power loss; under `GroupCommit`/`Relaxed`, through a process crash
- Cross-shard writes: crash-convergent (`commit_coordinated` holds a serialized lock; `open` re-drives pending writes)
- Optimizations never change answers (differential runner + per-level query tests)

See `docs/getting-started.md` for the quickstart and `docs/benchmarks/` for comparison results.
