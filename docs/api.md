# API — the stable surface

`LogicalStore` (`crates/adabt-core/src/store.rs`) is the **only** stable interface. Everything below it — `HeapStore`, `ColumnStore`, `DirectArray`, indexes, caches — is rebuildable and never leaks into the signature. `add` to the trait is a decision about the *logical* database, not about optimization.

## Engine (`Database`)

```rust
Database::open(dir, Policy::conventional()) -> Result<Self>
Database::open_shared(dir, policy, versions)
Database::open_at(dir, policy, RecoverTarget::Lsn(lsn)) // PITR
db.create_collection("users", Schema::dynamic())?
db.insert("users", RecordId(1), Record::new().with("name","ada"))?
db.get("users", RecordId(1))?
db.scan("users")? // ascending RecordId, the contract
db.query(&LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("country","NO"))))?  // via Planner + Executor
db.begin() -> Transaction; txn.get/insert/update/delete/scan; db.commit(txn)? // snapshot isolation; Strict validates read set
db.query_cancellable(&plan, cancel: Arc<AtomicBool>)? // ExecBudget::cancel polled every 4096 rows
db.set_pool_capacity(pages)?; db.set_plan_cache_entries(n); db.set_result_cache_entries(n)
db.checkpoint()?; db.backup_to(dest)?; db.restore_from(src,dest)?; db.lsn_at_or_before(dir, nanos)?
db.explain(&plan) -> String; db.explain_optimizations() -> String
db.verify()? // forward (heap→index) + reverse (index→heap) + columnar id-set
db.policy() -> &Policy; db.telemetry() -> Snapshot; db.last_exec_stats() -> ExecStats
```

`Transaction` is a value buffering writes against a fixed `Snapshot`; `commit` is all-or-nothing, sorted-key order, first-committer-wins, `Strict` also validates point reads (write-skew closed, predicate phantoms documented as not yet prevented — `serializable.rs:phantom`).

**DDL is not transactional** (`wal.rs:61`): `CreateCollection`/`DropCollection`/`AlterSchemaInPlace`/`AdoptMigration` commit on write, not rolled back with a `Transaction`. Catalog v4 persists `delta_encoding`/`thread_per_core`.

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

`Source::fetch` + `peek_field` (tri-state) + `fetch_projected`/`get_projected` (`RecordCodec::peek_fields`, `LogicalStore::get_projected`, `Database::fetch_projected` handles `DirectArray` per-field O(1) and `HeapStore` codec walk; `filter_by_peek_fields` 1–4 fields, `fetch_projected_batches` for Project-aware callers). `ExecBudget` (`max_ram_bytes` + `cancel` polled every 4096 rows). `Optimization` 17/17 level-reachable (`plan_cache`, `result_cache`, `buffer_pool`, `auto_index`, `auto_composite_index`, `auto_covering_index`, `record_compression`, `column_store`, `freeze_schema`, `direct_lookup`, `prefetch`, `materialized_view`, `clustered_sort`, `delta_encoding`, `thread_per_core`, `join_order`, `data_partitioning`) via `Action::{CreateIndex,SetColumnStore,SetDeltaEncoding,SetThreadPerCore,SetJoinOrder,SetDataPartitioning,SetClusterField,...}` — all `is_shadowable` or written reason, `NOT_YET_IMPLEMENTED=[]`.

## Errors

`Error::RecordExists`, `TransactionConflict {collection,id}`, `Corruption`, `Unsupported`, `RestoreTargetUnreachable`, `NoSuchCollection`. `Frame::decode` is lenient (7-byte TLS alert parses, not half-answer).
