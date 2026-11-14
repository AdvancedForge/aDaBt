# Changelog

## Unreleased

A correctness and code-quality pass. No on-disk format change.

### Fixed
- **Two optimizations that did nothing were removed.** `join_order` and
  `data_partitioning` were registered, enabled at level 6 and reported as
  applied, but only set flags nothing read. 17 optimizations are now 15; real
  join ordering is on the roadmap. `thread_per_core` no longer claims
  run-to-completion scheduling.
- **C ABI:** an engine panic is reported as `ADABT_INTERNAL` instead of
  aborting the host process. The `raw` declarations, which let safe Rust pass
  arbitrary pointers, are now `unsafe`.
- **Command lines:** `adabt-server`, `adabt-cli`, `adabt-bench` and the
  comparison harness refuse malformed values (`--level abc`, `--shards x`)
  with exit status 2 instead of silently using a default. The server
  validates its whole configuration before touching disk or network.
- **`watch_it_optimize`** timed result-cache hits and reported "not faster";
  it now measures queries (~63 ms → ~7 µs, identical rows).
- **`scripts/check.sh`** could never pass: its loom step ran ordinary tests
  under loom, and `tail` hid the failures. It now runs exactly the loom
  models and shows full failure output.

### Changed
- **Minimum supported Rust is 1.88**, the version the dependency tree has
  actually required; 1.82 was never buildable.
- **The check gate** (`scripts/check.sh`) now also covers the telemetry
  feature flags, the comparison workspace, and rustdoc warnings.
- **Structure:** `Database` (3,700 lines, 32 fields) is now a facade composed
  of seven cohesive parts across responsibility modules; the executor's
  430-line `run` is a dispatcher over per-operator functions; the heap, codec
  and optimization library are split by responsibility.
- **Documentation:** every comment and document was checked against the code;
  about 70 claims that described an earlier state were corrected, and
  internal milestone codes were replaced with plain explanations. The roadmap
  states an honest status per goal; its stage-by-stage history moved to
  `docs/history/`. Limitations now include that commits are not crash-atomic.

## 0.1.0-alpha.1 — 2026-08-26

Initial public alpha release. Experimental, RAM-resident, single-node Rust database with adaptive physical optimization (`docs/getting-started.md`). Not production-ready (`docs/limitations.md`).

**Workspace version `0.1.0-alpha.1`**, `catalog` v4 (`delta_encoding`/`thread_per_core` persisted, v3 forward-compatible), `superblock` gate `FORMAT_VERSION` unchanged.

**Engine `crates/adabt-engine/src/database.rs:135` / `crates/adabt-storage/src/heap.rs:70`:**
- `LogicalStore::get_projected` / `Source::fetch_projected` (`RecordCodec::peek_fields` `codec.rs:1067`, `HeapStore::get_projected` `heap.rs:2023`, `Database::fetch_projected` `database.rs:3126` with `DirectArray` per-field O(1)), `filter_by_peek_fields` 1–4 fields `exec.rs:294`, `fetch_projected_batches` primitive.
- `ShardedDatabase` `sharded.rs:1` coordinator-decides durability (windowed) `XSH1` `commit_coordinated` `sharded.rs:301` now serialized via `coordinator: Arc<Mutex<()>>` `sharded.rs:70` generic `lock<T>` `sharded.rs:604`, put-overwrite replay, torn-tail safe, `open` re-drives; `broadcast` `sharded.rs:354` burst pinning via `core_affinity` when `thread_per_core`.
- `Strict` read-set validation `transaction.rs:133`/`database.rs:1147` (`serializable.rs` write-skew, `predicate_phantom_is_not_prevented…` `tests/serializable.rs:155` documents phantom limit).
- `Database::{has_index,index_kind}` `database.rs:1618` for `manual_policy.rs` cost-model-correct assertions.

**Optimizer `crates/adabt-opt` / `crates/adabt-engine/src/optimizations.rs:23`:** 17 registered (was 15), `join_order` global level 6 + `data_partitioning` per-field level 6 `optimizations.rs:1393` `levels.rs:6`, `Action::{SetJoinOrder,SetDataPartitioning}` `action.rs:66` (`is_shadowable` `action.rs:87`), `NOT_YET_IMPLEMENTED=[]`, `every_builtin_registers_and_orders` expects 17.

**Storage `crates/adabt-storage/src/metadata.rs:46`:** `FORMAT_VERSION 4`, `Catalog{delta_encoding,thread_per_core}` persisted via `HeapStore::{delta_encoding,thread_per_core}` `heap.rs:286` and `Database::{delta_encoding,thread_per_core,join_order,data_partitioning}` `database.rs:219`, `Database::open` restores `set_delta_enabled` `database.rs:457`. `catalog_upgrade.rs` proves v3→v4 migration.

**Execution `crates/adabt-exec/src/exec.rs:85`:** `filter_by_peek_fields` for 1–4-field predicates, `ExecBudget` cancel every 4096 rows.

**Hardening/tests:** `cross_shard_concurrent.rs:1` soak under concurrent readers/writers with unique `seq` per coordinated txn, every `Result` must succeed, acked vector verified after reopen; `cross_shard_atomic.rs` 4 crash points; `catalog_upgrade.rs` 3; `serializable.rs` 4.

**Surface/docs:** `adabt-ffi` `c_binding.rs`, `docs/api.md` / `docs/operations.md` (`scripts/check.sh` + `.githooks/pre-push` replacing deleted `.github/workflows/ci.yml` private repo), `docs/semver.md`, `docs/replication-decision.md`, `comparison/` separate workspace correct invocation `cargo run --manifest-path comparison/Cargo.toml` fail-fast `--witness` `comparison/src/main.rs:8`.

**Build:** `cargo fmt --check` 0, `clippy -D warnings` 0, `cargo test -p adabt-engine --test manual_policy` 8/8 (was 4 failures before `has_index` fix), `cargo test --workspace --lib` 56 passed.
