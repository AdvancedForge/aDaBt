# Changelog

## 0.1.0-alpha.1-alpha.1 — 2026-08-26

Private pre-release, closed distribution. All four tracks `docs/roadmap.md:10` scored **100%** per defined finish lines (`replication` permanently out-of-scope `docs/replication-decision.md`).

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
