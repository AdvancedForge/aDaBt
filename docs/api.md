# API — the stable surface

`LogicalStore` (`crates/adabt-core/src/store.rs`) is the **only** stable interface. Everything below it — `HeapStore`, `ColumnStore`, `DirectArray`, indexes, caches — is rebuildable and never leaks into the signature. `add` to the trait is a decision about the *logical* database, not about optimization.

## Engine (`Database`)

```rust
Database::open(dir, Policy::conventional()) -> Result<Self>
Database::open_shared(dir, policy, versions)
Database::open_at(dir, policy, RecoverTarget::Lsn(lsn)) // PITR: dir must be a restored backup plus its log; see below
db.create_collection("users", Schema::dynamic())?
db.insert("users", RecordId(1), Record::new().with("name","ada"))?
db.get("users", RecordId(1))?
db.scan("users")? // ascending RecordId, the contract
db.query(&LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country","NO"))))?  // via Planner + Executor
db.begin() -> Transaction; txn.get/insert/update/delete/scan; db.commit(txn)? // snapshot isolation; serializable under Strict
db.query_cancellable(&plan, cancel: Arc<AtomicBool>)? // ExecBudget::cancel polled every 4096 rows
db.set_pool_capacity(pages)?; db.set_plan_cache_entries(n); db.set_result_cache_entries(n)
db.checkpoint()?; db.backup_to(dest)?; Database::restore_from(src,dest)?; Database::lsn_at_or_before(dir, nanos)?
db.set_log_archive(Some(dir)) // keep the segments checkpoints discard, for PITR
db.explain(&plan) -> String; db.explain_optimizations() -> String
db.verify()? // forward (heap→index) + reverse (index→heap) + columnar id-set
db.policy() -> &Policy; db.telemetry() -> Snapshot; db.last_exec_stats() -> ExecStats
```

**Point-in-time recovery** starts from a base image: a backup restored with `restore_from`, with the archived log segments that reach the target copied into its `wal/` (`docs/operations.md` has the procedure). `open_at` replays from the backup's checkpoint to the target and returns exactly the committed state as of that log entry. It refuses, without changing `dir`, in three cases: a heap file that may hold writes from after the target, which is the usual state of a live data directory (`InvalidRestore`); a target before the backup (`RestoreTargetUnreachable`); and a log with a gap before the target (`InvalidRestore`). If it succeeds, `dir` becomes the restored database. Its log ends at the target, and later entries are moved to `dir/superseded-log/` rather than replayed.

`Transaction` is a value buffering writes against a fixed `Snapshot`. `commit` first validates everything with nothing written — first-committer-wins over the write-set, the schema, and unique constraints judged against the state the whole write-set leaves (so a value may move between records in one transaction, and two writes claiming one value fail) — then logs the write-set under one commit record, makes it durable once (one fsync under `Strict`, however many writes), and only then applies it. A crash at any point recovers all of the transaction or none of it, and an `open_at` target inside a transaction exposes none of it (`commit_window_chaos.rs`). `insert_batch` is all-or-nothing the same way. Under `Consistency::Strict` transactions are serializable: `commit` also refuses, with `TransactionConflict`, a transaction where any id it read with `get` (absent ids included, and the existence checks `insert`/`update`/`delete` make) was written after its snapshot, or any collection it read with `scan` received a committed insert, update or delete after its snapshot. That closes write skew and predicate phantoms; every committed transaction is equivalent to running alone at its commit point. Scan validation is coarse by design: under `Strict`, a scan conflicts with any concurrent committed write to that collection, whether or not the row would have matched what the caller was looking for, so long scanning transactions over busy collections should expect to retry. A transaction that only reads by id is judged by those ids alone. For a scan conflict the error's `id` is the newest record written to the collection since the snapshot. A read-only transaction always commits. DDL is outside the guarantee. Under `Snapshot` none of this is checked and nothing extra is paid (`serializable.rs`).

**DDL is not transactional** (see `WalOp` in `adabt-storage`): `CreateCollection`/`DropCollection`/`AlterSchemaInPlace`/`AdoptMigration` commit on write, not rolled back with a `Transaction`. Catalog v4 persists `delta_encoding`/`thread_per_core`.

## Sharded (`ShardedDatabase`)

```rust
ShardedDatabase::open(dir, shards, policy) -> Result<Self> // dir/shard-{i}, one VersionTracker
shard_of(id) = id.0 % shards
db.insert/ get /update/delete/scan/count  // &self, per-shard Mutex, no outer lock
db.commit_coordinated(Vec<CrossShardWrite>) // coordinator-decides durability (windowed visibility): fsynced XSH1 journal, put-overwrite replay, torn-tail safe, open re-drives
db.query(&plan) // pushdown Scan+Filter per shard, merge_by_id, central Sort/Limit/Agg
```

Guarantee is **coordinator-decides durability**, not linearizable atomicity: between journal fsync and last shard apply, concurrent readers can see shards disagree (documented window, needs distributed locking to hide).

## Wire (`adabt-server`)

`Database` behind `Server::with_auth` / `with_tls` (`rustls` before any protocol byte), per-connection state, constant-time token compare, `Status::Forbidden`/`Unauthorized`, `with_collection_floor`. Client is `adabt-server::Client` generic over `Read+Write` + `send_raw`/`next_reply` for conformance.

## Indexes / Derived

`IndexKind::{Hash,BTree,Bitmap}`, `LOW_CARDINALITY_KEY_COUNT=256`, covering (`COVER_SEP` `\x01`) and composite (`COMPOSITE_SEP` `\0`) names, `partial` predicate syntactic-containment. `ColumnStore` (`column_topk` k-winners), `DirectArray` (Fixed schemas, `physical_record_size` stride), `MaterializedViews` (exact while integer, `verify` excludes).

## Execution / Optimizer

`Source::fetch` + `peek_field` (tri-state) + `fetch_projected`/`get_projected` (`RecordCodec::peek_fields`, `LogicalStore::get_projected`, `Database::fetch_projected` handles `DirectArray` per-field O(1) and `HeapStore` codec walk; `filter_by_peek_fields` 1–4 fields, `fetch_projected_batches` for Project-aware callers). `ExecBudget` (`max_ram_bytes` + `cancel` polled every 4096 rows). `Optimization`: 15, each level-reachable (`plan_cache`, `result_cache`, `buffer_pool`, `auto_index`, `auto_composite_index`, `auto_covering_index`, `record_compression`, `column_store`, `freeze_schema`, `direct_lookup`, `prefetch`, `materialized_view`, `clustered_sort`, `delta_encoding`, `thread_per_core`) via `Action::{CreateIndex,SetColumnStore,SetDeltaEncoding,SetThreadPerCore,SetClusterField,...}` — all `is_shadowable` or written reason, `NOT_YET_IMPLEMENTED=[]`.

## Errors

`Error::RecordExists`, `TransactionConflict {collection,id}`, `Corruption`, `Unsupported`, `RestoreTargetUnreachable`, `NoSuchCollection`. `Frame::decode` is lenient (7-byte TLS alert parses, not half-answer).
