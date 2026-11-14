# Operations

## Build & verify

`scripts/check.sh` is the gate (enable it as a pre-push hook with `git config core.hooksPath .githooks`):

```sh
bash scripts/check.sh          # fmt --check + clippy -D warnings + test --workspace + loom subset + comparison sanity + doc
git config core.hooksPath .githooks  # enables pre-push gate (runs check.sh)
```

Or manually:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p adabt-storage --features loom --lib loom_tests   # TxId allocator model-check (engine not yet covered)
cargo doc --workspace --no-deps
```

Separate workspace harness (planned witnesses are *not yet real drivers* — harness is fail-fast until `postgres`/`rocksdb` deps exist, see the header of `comparison/src/main.rs`):

```sh
cargo run --manifest-path comparison/Cargo.toml --release              # SQLite witness (bundled, always)
cargo run --manifest-path comparison/Cargo.toml --release -- --witness postgres  # planned: requires DATABASE_URL, currently exits 2 fail-fast
cargo run --manifest-path comparison/Cargo.toml --release --features rocksdb -- --witness rocksdb  # planned: requires cmake/libclang, currently exits 2 fail-fast
cargo build --profile bench-fast -p adabt-bench
target/bench-fast/adabt-bench soak --data-dir /var/tmp/soak --size 3000 --ops-per-phase 6000 --log
```

## Runtime

Scale is **RAM-bound by design** (`docs/benchmarks/scale-decision.md`): ~470 B/row, page directory + every index resident. Ceiling stated at server startup; revisit triggers: resident set fits but dataset does not, or thread-per-core makes residency dominant.

Sharding is the growth story: `ShardedDatabase::open(dir, shards, policy)` → `dir/shard-{i}`, `RecordId % shards` routing, `commit_coordinated` for cross-shard durability (see `docs/api.md` windowed visibility). `--shards 1` is the honest unpartitioned control.

Per-core: `SetThreadPerCore(true)` (level 9) pins `broadcast` workers via `core_affinity` (best-effort) — per-shard `Mutex` + `BufferPool` already gives per-core memory (shards == cores). Burst pinning (scoped per-query threads) is intentional: persistent shard workers would add a queue between caller and the only thing it waits for, without measured collapse (`connection_scale` gate: fails if any rung <0.5× prior, currently 116k→72k gentle decline).

## Checkpoints, backup, PITR

```rust
db.checkpoint()? // pages fsynced, WAL Checkpoint, directory + catalog fsynced, discard below flushed_lsn, base.adabt
db.backup_to("/backups/mon")? // checkpoint + copy heap/wal/superblock/catalog, write directory + base.adabt
Database::restore_from("/backups/mon", "/srv/restored")? // plain copy; `open` it to get the backup's state
```

### Point-in-time recovery

PITR restores a **base image** taken at or before the target and replays the log forward to the target. A base image is a heap file exactly as a checkpoint left it, plus that checkpoint's page directory; `base.adabt` records that a directory holds one, and the first write to the heap file after the checkpoint removes it. Backups are base images. A live data directory usually is not: the buffer pool writes dirty pages to the heap file whenever it needs room, so pages written after any earlier target may already be there, and replay cannot take them back out.

1. **Before you need it**, on the running database: `db.set_log_archive(Some(archive_dir))`, then `db.backup_to(backup_dir)` on a schedule. Checkpoints move the segments they no longer need into the archive instead of deleting them; a restore needs every segment from its backup to its target.
2. **Restore the newest backup taken before the target** into an empty directory: `Database::restore_from(backup_dir, dest)`.
3. **Supply the log.** Copy every segment file (`seg-*.adabt`) from the archive into `dest/wal/`, replacing same-named files; if the target is later than the last archived segment, then also copy the source's current `wal/` segments the same way.
4. **Find the target.** By time: `Database::lsn_at_or_before(dest, nanos)`; or an LSN read from the log.
5. **Open at the target:** `Database::open_at(dest, policy, RecoverTarget::Lsn(lsn))`. The result is exactly the committed state as of that entry. A transaction whose commit record comes after the target is excluded completely.
6. **Carry on with `dest` as the database.** `open_at` ends `dest`'s log at the target. Entries after it move to `dest/superseded-log/after-<lsn>/` and are kept there, not replayed and not deleted. Give the restored database **its own archive directory**: it continues its log at target + 1, so its segment names collide with the source's.

`open_at` checks everything before it writes anything, so if it refuses, `dest` is left unchanged:

| Refusal | Cause | Fix |
|---|---|---|
| `InvalidRestore`: not a base image | `dest` is a live directory whose heap file changed after its last checkpoint (or `base.adabt` is missing) | Restore a backup (step 2) |
| `RestoreTargetUnreachable` | The target is before the base image's checkpoint | Use an older backup |
| `InvalidRestore`: log missing lsns / log ends at | A segment between the backup and the target was not supplied | Copy the missing segments (step 3) |
| `InvalidRestore`: page directory missing | `directory.adabt` was removed from the restored directory or damaged | Restore the backup again |

If `open_at` fails or is interrupted partway through replay, restore the backup again. Replay may already have written pages to `dest`'s heap file, and then `dest` is no longer a base image. Running `open_at` straight after a checkpoint on a live directory works only if nothing has written to the heap file since, so do not rely on it.

Catalog v4 (`delta_encoding`/`thread_per_core`) is authoritative; loss rebuilds from WAL if log is complete, otherwise `Corruption` with `log_start_lsn`.

The page directory (`directory.adabt`, rewritten by every checkpoint) is authoritative too: crash recovery starts from it and replays the log, because the heap file alone cannot tell a live record from a deleted or superseded one. A checkpointed database whose directory is missing, damaged or copied from another database refuses to open with `Corruption` naming the page directory, and nothing is changed; put the file back or recover from a backup.

### A log that ends below the last checkpoint

A crash cannot do this: a checkpoint syncs the log before it writes the catalog, the page directory or `base.adabt`. If the log ends below the LSN one of those files records anyway — a segment deleted or cut short, a device that lost writes it had acknowledged, a backup copied without its whole `wal/` — the log was damaged afterwards. On open:

- **If the catalog reaches the checkpoint** (the normal case), the database opens as that checkpoint left it. Writes made after the checkpoint that were only in the lost part of the log are gone, as with any lost log tail. The log then restarts just past the checkpoint, so nothing written from now on can be mistaken for something the checkpoint already holds. The segments it held move to `superseded-log/below-<lsn>/` (all of their entries are below `<lsn>`, the new start of the log), kept rather than deleted, and the derived-structure cache is discarded and rebuilt. After this, the log no longer reaches back to the database's first write, so a later loss of `catalog.adabt` is refused instead of rebuilt from the log.
- **If the catalog is older than the checkpoint** (a crash between a checkpoint's directory write and its catalog write, then the log damaged too), the open is refused with `Corruption` naming the catalog and the checkpoint LSN, and nothing is changed: collection definitions logged between the two were only in the lost part of the log. Recover from a backup.

## Tuning

`Policy::manual(level)`, or `policy.mode = Mode::Adaptive`. Levels are cumulative presets (`adabt-opt/src/levels.rs`): 0 conventional → 11 maximum. `adabt-engine/src/optimizations/` 15 optimizations, all level-reachable (`every_registered_optimization_is_reachable_from_some_level`). `explain_optimizations()` / decision log + `to_prometheus_text`.

## Hardening you can run

- `cargo test -p adabt-storage --test catalog_upgrade` — v3 accepted with defaults, v4 round-trips, future version refused
- `cargo test -p adabt-engine --test cross_shard_concurrent` — coordinator soak under concurrent readers/writers
- `cargo test -p adabt-engine --test serializable -- phantom` — write skew through a phantom commits under `Snapshot` and is refused under `Strict`
- `crash_consistency.rs` (13 offsets), `promotion_chaos.rs` (Building/Shadow kill-reopen), `verify()` seeded divergence
