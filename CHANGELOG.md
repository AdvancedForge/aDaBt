# Changelog

## Unreleased

A correctness and code-quality pass. The existing formats are unchanged; two
files are new (`base.adabt`, `superseded-log/`).

### Fixed
- **Counting live records walked every id in the directory**, on every plan
  and every optimization cycle. A `Directory` type now keeps the live count
  beside the id map, changed only by the mutations that can affect it.
- **Every optimization cycle scanned every record to find each
  collection's highest id.** At 20k rows that took ~3.7 ms of a ~4.2 ms
  cycle. The ~100 µs of identity lookups between cycles then ran on caches
  the scan had flushed. That was the whole 20k identity gap: a manual
  database driven through the same cycles was just as slow, and a quiet
  adaptive one was as fast as manual. The id now comes off the page
  directory (`HeapStore::max_live_id`), with the same value and the same
  decisions in the soak. At 20k rows identity went from 1.33× the expert
  to 1.03× (matches) and identity again from 1.52× to 1.12×
  (`docs/benchmarks/expert-vs-adaptive.md`).
- **Every optimization cycle read every live record twice.** Summing stored
  bytes and measuring the compression footprint each touched a page per
  record, every 500 operations and inside measured windows. Both now read
  an evenly spaced sample of at most 256 records per collection
  (`HeapStore::sampled_stored_bytes`). At 20k rows, adaptive's range filter
  went from 1.74× to 1.23× the expert and aggregate from 1.99× to 1.12×.
- **Record compression's estimate claimed to free more than the database
  held.** It assumed a fixed 64 bytes saved a row. The expert comparison's
  records are about 43 bytes, below the size the store compresses at all,
  so adaptive carried compression through every phase at 20k rows. The
  estimate now claims the stored records' uncompressed bytes times
  (1 − the compression ratio measured on a sample of them)
  (`HeapStore::record_footprint`, `OptContext::record_bytes`). Both figures
  read the same with compression on or off, so the claim does not shrink
  once made. Decode CPU and latency are charged in proportion to the reads
  that reach stored records (`WorkloadMix::record_reads`). Adaptive no
  longer enables compression where records do not compress. The 20k
  identity gaps (1.34–1.50×) remain with the same configuration as the
  expert, so compression was not their cause
  (`docs/benchmarks/expert-vs-adaptive.md`).
- **The adaptive driver scores on evidence instead of fixed priors.** The
  result cache is judged on its measured or ghost hit rate and resident
  bytes; scan-, grouping-, lookup- and range-helping optimizations claim
  their gain only on the share of traffic they can affect; memory spent and
  saved weigh the same; calibration measures each change on its own query's
  samples since it was made, skips windows an experiment overlapped, and
  reads a measurement as a correction to the prediction; and the driver
  decides nothing while an experiment runs. On `expert-vs-adaptive`,
  adaptive now matches the expert on 3 of 5 phases at 5k rows and 1 of 5 at
  20k (was 1 and 0), and the point-filter and aggregate gaps fell from
  ~1,000× to at most 1.7× (`docs/benchmarks/expert-vs-adaptive.md`).
  Structures from ended phases are now retracted. Record compression's
  over-claiming estimate, suspected next, is fixed above.
- **Crash recovery brought deleted records back and could return an
  overwritten record's old version.** Version reclamation is lazy, so a
  checkpoint can flush the slots of records deleted or overwritten before
  it, and the buffer pool can write copies of later writes to the heap file
  before a crash. Recovery used the checkpoint's page-directory snapshot only
  while the heap file was the length it recorded; once a page had been
  allocated since (any insert-heavy stretch), it rebuilt the directory by
  scanning pages, which cannot tell those slots from live records. In the
  reproduction (10 rows, delete id 3, checkpoint, 190 more inserts, crash) id
  3 came back as row 200; with the update's new version in an earlier slot
  than its old one, the old version came back instead. Recovery now always
  starts from the newest directory snapshot, whatever the heap file went
  through since, and replays the log over it; it never scans pages. That is
  exact because every change to where a record lives is logged, and a
  record the log rewrites has its snapshot slot dropped, and freed only if
  it still holds that record, before replay places anything for it
  (`crates/adabt-storage/src/heap/recovery.rs` gives the argument). Pages
  allocated after the snapshot are truncated away on recovery; a database
  never checkpointed is rebuilt from its complete log over an empty heap.
  Related changes on the same path:
  - **Behaviour change:** a checkpointed database whose `directory.adabt`
    is missing, damaged or from another database is refused with
    `Corruption`, as a missing catalog already is, instead of being opened
    from a page scan that could invent data. A checkpoint that cannot write
    the directory now fails instead of carrying on without it.
  - `vacuum` logs every move as an `Update` of the record's own bytes and
    makes the log durable before freeing the old copy; a crash after a
    vacuum and before the next checkpoint used to make the database
    unopenable. Vacuum also frees slots nothing refers to (left by a crash)
    and no longer truncates a page an open snapshot still reads.
  - a checkpoint reclaims every version no snapshot can read before it
    flushes, and makes the log durable before the heap file.
  - every autocommit write is handed to the operating system before the
    page change it describes, under `Relaxed` too, so a page the pool writes
    can never outlive its log entry in a process crash.
  The on-disk formats are unchanged.
- **Point-in-time recovery could show writes from after its target.**
  `open_at` replayed the log up to the target over the heap file as it
  stood, but the buffer pool writes dirty pages to that file whenever it
  needs room, so later writes were already there and showed through. In the
  reproduction (10 rows, a checkpoint, a two-page pool, 20 more inserts) a
  restore to the first of the 20 returned 20 rows instead of 11. A restore
  now starts from a *base image*: a heap file exactly as a checkpoint left
  it, plus that checkpoint's page directory. Every backup is one, and it
  replays the log forward from there. **Behaviour change:** `open_at` on a
  directory whose heap file has changed since its last checkpoint (in
  practice, a live data directory) is refused with `InvalidRestore`, and so is
  a log with a gap between the base image and the target. The supported
  procedure is `restore_from(backup)`, copy the archived segments in, then
  `open_at` (`docs/operations.md`). Related fixes on the same path:
  - a restore ends the restored directory's log at the target and moves the
    entries after it to `superseded-log/`, where they are kept. Before this, a
    restored database replayed that later history on its next open and mixed
    it with its own new writes.
  - replay starts at the backup's checkpoint, not at whichever later
    checkpoint the supplied log records. That also applies to a plain `open`
    of a restored backup that has been given later log; before this, such an
    open skipped every write between the backup and the source's next
    checkpoint.
  - a backup carries its page-directory snapshot. Without it, opening a
    backup rebuilt the directory by scanning pages and brought back records
    deleted or overwritten before the backup.
  No API signature changed.
- **Writes made after the log was damaged back past a checkpoint were lost
  at the next restart.** The log picked its next LSN from its own last
  surviving entry. If the log ended below the checkpoint that the catalog,
  page directory or `base.adabt` records (a lost or truncated segment; a
  crash cannot do this, because a checkpoint syncs the log first), new
  writes got LSNs at or below that checkpoint, and the next recovery skipped
  them as already in the pages. In the reproduction (20 rows, a checkpoint,
  the log cut to its header, 10 more inserts acknowledged under `Strict`, a
  restart) the 10 inserts were gone, with no error. The same happened after
  `open_at` the base image's own LSN and on a restored backup. Opening now
  restarts the log at the checkpoint's LSN + 1, moves the old segments to
  `superseded-log/below-<lsn>/`, and discards the derived-structure cache.
  **Behaviour change:** if the catalog is older than the checkpoint as well,
  the definitions in between were only in the lost log, and the open is
  refused with `Corruption` instead of dropping those collections' records
  (`crates/adabt-storage/src/heap/recovery.rs` has the argument;
  `log_behind_checkpoint.rs`). The on-disk formats are unchanged.
- **Manual mode undid its own work:** a second `optimize()` at the same
  level disabled the per-field indexes the level had created, because the
  level's unscoped request was diffed against its own scoped expansion.
- **Transaction commits and `insert_batch` are crash-atomic.** The write-set
  is logged as frames tagged with a transaction id plus one `Commit` record,
  made durable once, and applied to pages only after; recovery applies a
  transaction only if its `Commit` is in the log. A crash mid-commit used to
  leave a prefix of the write-set, and a torn batch a prefix of its rows. A
  100-write commit under `Strict` now costs 1 fsync instead of 100. No format
  change: both fields already existed, and logs without them replay as before.
  Alongside: a transaction whose writes collide on a unique value with each
  other is refused before anything is written (it used to apply a prefix and
  then fail), one that moves a unique value between records commits, and a
  record too large for a page is refused before it is logged rather than
  logged and then refused, which made every later open fail.
- **`Consistency::Strict` is serializable.** It validated only the ids a
  transaction read, so a scan did not conflict with a concurrent insert of a
  row it never saw: two transactions could each find no matching row and
  each insert one (a predicate phantom). Commit now also refuses a
  transaction any of whose scanned collections received a committed write
  after its snapshot, using a per-collection newest-write stamp the heap keeps
  in memory. The check is at collection granularity, so under `Strict` a scan
  conflicts with any concurrent write to that collection; point reads are
  judged per id as before, and `Snapshot` is unchanged.
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

### Added
- **`adabt-bench expert-vs-adaptive`**, published in
  `docs/benchmarks/expert-vs-adaptive.md`. Adaptive matches the best manual
  configuration on 1 of 5 soak phases at 5k rows and 0 of 5 at 20k. It is
  behind by 1.2× to about 1,000×, mostly because it enables the result cache
  late or never.

### Changed
- **Writes are about 5× faster, answers and durability unchanged.** Against
  SQLite, bulk load went from 7–12× slower to 1.5–2.0× slower and single-row
  inserts from 2.2–2.5× slower to 2.3–2.5× faster (comparison harness, before
  and after run interleaved: 4,276–4,344 → 848–875 ns per loaded row,
  8,667–8,891 → 1,512–1,613 ns per single-row insert). Most of the time was
  placement: the free-space map scanned every full page on every insert. It
  is now ordered by free space and finds the fullest page that fits in
  O(log pages). Also: clock eviction records a hit with one lookup instead of
  a search of the ring; data log frames are encoded from borrowed parts into
  a reused buffer (bytes on disk unchanged); `Database::insert_batch` lends
  its records to the store instead of cloning them, and the store encodes a
  batch into one buffer; slot payloads use a reused buffer; pages skip their
  live leading slots when looking for one to reuse; new pages are written
  from a pre-sealed empty page; page-id maps hash with one multiply.
  Allocations per row: 10.2 → 1.3 for a batch insert, 11.2 → 5.2 for an
  autocommit insert, both now pinned by tests. New
  `HeapStore::insert_batch_from`, `RecordCodec::encode_into`,
  `Wal::append_record` with `RecordFrame`, and `page::PageMap`.
- **Covering-index reads are faster, answers unchanged.** Stored projections
  share one name per field, the id → projection map is hashed, and `Project`
  narrows owned rows in place: tuned indexed range went from ~1.3× slower
  than SQLite to faster (1.3–2.9× across runs), tuned indexed equality from
  1.7–2.9× slower to ~1.6×. `Record::retain_fields` is new.
- **Covering rows are filtered before they are copied, answers and stats
  unchanged.** The executor runs `Filter` and `Project` on the covering
  index's stored rows in place, so a rejected row is never cloned and the
  filtered-only field never is: equality allocates 4.0 instead of 5.0 times
  per returned row and runs roughly 10–25% faster, ~1.4–1.5× behind SQLite in
  the quietest runs. Range is unchanged. New `Source::visit_covering_lookup`
  and `visit_covering_range` lend the rows; their defaults wrap the existing
  methods, so other sources need no change.
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
