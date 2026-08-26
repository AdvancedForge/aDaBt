# Operations

## Build & verify (private repo, no GH Actions)

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --features loom   # TxId allocator model-check
cargo doc --workspace --no-deps
```

Separate workspace harness:

```sh
cargo run --manifest-path comparison/Cargo.toml --release              # SQLite witness (bundled, always)
cargo run --manifest-path comparison/Cargo.toml --release -- --witness postgres  # requires DATABASE_URL, fail-fast if unavailable
cargo run --manifest-path comparison/Cargo.toml --release --features rocksdb -- --witness rocksdb  # requires cmake/libclang, fail-fast
cargo build --profile bench-fast -p adabt-bench
target/bench-fast/adabt-bench soak --data-dir /var/tmp/soak --size 3000 --ops-per-phase 6000 --log
```

## Runtime

Scale is **RAM-bound by design** (`docs/scale-decision.md`): ~470 B/row, page directory + every index resident. Ceiling stated at server startup; revisit triggers: resident set fits but dataset does not, or thread-per-core makes residency dominant.

Sharding is the growth story: `ShardedDatabase::open(dir, shards, policy)` → `dir/shard-{i}`, `RecordId % shards` routing, `commit_coordinated` for cross-shard durability (see `docs/api.md` windowed visibility). `--shards 1` is the honest unpartitioned control.

Per-core: `SetThreadPerCore(true)` (level 9, `SetDataPartitioning`/`SetJoinOrder` level 6) pins `broadcast` workers via `core_affinity` (best-effort) — per-shard `Mutex` + `BufferPool` already gives per-core memory (shards == cores). Burst pinning (scoped per-query threads) is intentional: persistent shard workers would add a queue between caller and the only thing it waits for, without measured collapse (`connection_scale` gate: fails if any rung <0.5× prior, currently 116k→72k gentle decline).

## Checkpoints, backup, PITR

```rust
db.checkpoint()? // pages fsynced, WAL Checkpoint, directory + catalog fsynced, discard below flushed_lsn
db.backup_to("/tmp/bak")? // checkpoint + copy heap/wal/superblock/catalog
Database::restore_from("/tmp/bak", "/tmp/restored")?
db.set_log_archive(Some("/var/log/adabt"))? // keep segments for open_at
Database::open_at(dir, policy, RecoverTarget::Lsn(lsn))?
```

Catalog v4 (`delta_encoding`/`thread_per_core`) is authoritative; loss rebuilds from WAL if log is complete, otherwise `Corruption` with `log_start_lsn`.

## Tuning

`Policy::manual(level)` or `Policy::adaptive`. Levels are cumulative presets (`adabt-opt/src/levels.rs`): 0 conventional → 11 maximum. `adabt-engine/src/optimizations.rs` 17 optimizations, all level-reachable (`every_registered_optimization_is_reachable_from_some_level`). `explain_optimizations()` / decision log + `to_prometheus_text`.

## Hardening you can run

- `cargo test -p adabt-storage --test catalog_upgrade` — v3 accepted with defaults, v4 round-trips, future version refused
- `cargo test -p adabt-engine --test cross_shard_concurrent` — coordinator soak under concurrent readers/writers
- `cargo test -p adabt-engine --test serializable -- predicate_phantom` — documents Strict phantom limit
- `crash_consistency.rs` (13 offsets), `promotion_chaos.rs` (Building/Shadow kill-reopen), `verify()` seeded divergence
