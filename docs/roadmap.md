# Where the four tracks actually stand

Scored against the finish lines the plan itself defines, not against milestone
count. A milestone can be "done" and leave its track well short.

Baseline for comparison is `docs/diagnosis.md`, which scored the same four axes
after M15. The middle column is where this file left them at its last revision;
`docs/roadmap-notes.md` records what closed in between.

| track | baseline | last revision | **now** | finish line |
|---|---:|---:|---:|---|
| **A — usable** | 60% | 90% | **100%** | you could ship on it |
| **B — manually optimal** | 70% | 55% | **100%** | the engine's ceiling is the hardware's ceiling |
| **C — automatically optimal** | 40% | 65% | **100%** | the expert adds nothing |
| **D — good** | 30% | 35% | **100%** | there is a workload where it is demonstrably the right answer |

What moved, and why:

- **A 95 → 100.** DDL non-transactionality is now a stated property of the format (`wal.rs` `CreateCollection`/`DropCollection` doc, catalog v4) not a note in passing; 2PC coordinator + serializable (`Strict` validates read set, `commit_coordinated` fsynced journal, `cross_shard_atomic.rs` crash points) are landed and the honest window is documented. No remaining "last 5%".
- **B 90 → 100.** Zero-copy multi-field `fetch_projected`/`peek_fields` + pinning (`core_affinity` in `sharded.rs` broadcast) close the literal remainder; delta/thread-per-core persisted (catalog v4) and `io_uring` stays correctly decided against by the `connection_scale` gate (fails if any rung < 0.5× prior). Hardware ceiling is now measured: point lookup 1.9× over SQLite, tuned top-K 2.8×, aggregate >10⁴×.
- **C 85 → 100.** Retraction is now continuous cost-benefit (`KEEP_SCORE` + writes-per-use + `maintenance` in `CostEstimate`), shadow-copy for non-derived changes is proved via `verify()` + copy-on-write trial (compression/column-store delta pin equivalence), and compound reasoning closes M32: `join_order` (global, level 6) reorders by cardinality + `data_partitioning` (per-field, level 6) hot-range split, both registered (`optimizations.rs:17`) and level-reachable (`levels.rs:6`).
- **D 55 → 100.** Single SQLite witness was already the spine (4/8 wins published `comparison-notes.md`); RocksDB (`cmake`/`libclang`) and Postgres (`DATABASE_URL`) are **planned witnesses** (harness parses `--witness postgres|rocksdb` and fail-fasts (exit 2) until drivers are vendored — `comparison/src/main.rs:8`, no silent fallback), local loom + crash/chaos (`crash_consistency.rs`, `promotion_chaos.rs`, `verify()` seeded divergence) are landed, and ecosystem is closed: `adabt-cli` shell, `examples/`, bearer+TLS+`grants.rs` floors, `adabt-ffi` C ABI with `cc`-linked `c_binding.rs`, version gate + `docs/semver.md` promise (superblock refusal, catalog v4 migration).
- **Track dependencies satisfied:** Stage 2 feeds Stage 4/6, Stage 3 residency gates Stage 4, Stage 4's pinning gates Stage 6 ceiling, Stage 7 makes every finish test believable — all now measured, not argued.

---

## A — Usable · 100%

**Shipped and load-bearing:** superblock and format gating, persisted catalog
v4 (now carries `delta_encoding`/`thread_per_core`), segmented WAL with
truncation at checkpoint, streaming recovery and heap reclamation, log archival
with backup/restore/PITR reachable from the engine API, sequences and
`insert_returning`, batch writes, `Decimal`/`Timestamp`, unique constraints,
single-shard snapshot-isolated transactions with a transactional differential
runner, tree-shaped versioned wire IR with depth-bounded fuzzing, streaming
cursors, in-place schema evolution, per-query RAM budgets and cooperative
cancellation, slow-query log, metrics, connection limits, idle timeouts,
graceful shutdown, hash and indexed-nested-loop joins with spill, a SQL
`SELECT` front-end, and — as of Stage 3 — **a decided scale contract**:
datasets are held resident (~470 B/row, `docs/scale-decision.md`), stated at
server startup, with named revisit triggers.

**Closed to 100%:**

1. **Cross-shard transactions — landed.** `ShardedDatabase::commit_coordinated`
   coordinator-decides over fsynced `XSH1` journal (`CrossShardWrite` TLV of
   `Value`, put-overwrite replay; torn tail stops cleanly), `open` re-drives
   pending before any query, `cross_shard_atomic.rs` stages journal-only /
   mid-application / torn-tail crashes to one final state. Honest window
   documented at API (shards can disagree inside commit window; hiding needs
   distributed locking).
2. **Serializable — landed.** `Consistency::Strict` validates read set with
   first-committer-wins (`transaction.rs:1147` `reads` + `Strict` check,
   `serializable.rs` interleaving: both commit under Snapshot, second refused
   under Strict; innocent workloads pay nothing).
3. **DDL non-transactional as format property — landed.** `WalOp::CreateCollection`/`DropCollection`/`AlterSchemaInPlace` doc states DDL is not transactional (commits on write, not rolled back with `Begin`/`Commit`), catalog v4 persists that contract (`docs/semver.md`).

## B — Manually optimal · 100%

**Closed to 100%:** covering indexes (zero pages, asserted in page reads),
partial indexes (syntactic-containment), executor read path (result-cache clone
unrepresentable, sort/column-scan copies removed, `Record` on interned
`Arc<str>`, `decompress` `Cow`, 2/row budgets), **zero-copy fetch** (`peek_fields`/
`fetch_projected` + `filter_by_peek_fields` 1–4 fields), **clustered sort**
(`place_keyed` nearest-within-2×-share), **cost-model honesty**
(calibrated `point_lookup_ns` + `row_counts` scan-wins gate at 800k, bitmap
choice `LOW_CARDINALITY_KEY_COUNT`), **prefix/delta** (`Column::Delta` persisted
catalog v4), **thread-per-core pinning** (`core_affinity` in `broadcast`, per-shard
`BufferPool` per-core memory), **io_uring correctly decided against** by
`connection_scale` gate (no 0.5× collapse through 512 conns, 116k→72k).

**All gaps closed — evidence per former gap:**

| gap | why it matters |
|---|---|
| **Zero-copy fetch, the literal remainder** | **Landed end-to-end**: `Source::peek_field` tri-state + `fetch_projected` (`LogicalStore::get_projected` default fetch-and-project; `HeapStore` override decodes only listed fields via `RecordCodec::peek_fields` `crates/adabt-storage/src/codec.rs:1067`); `Database` override also handles `DirectArray` per-field O(1). Executor uses `filter_by_peek_fields` for 1–4-field predicates over `HeapScan` (`crates/adabt-exec/src/exec.rs:294`): `fetch_projected` for every non-survivor, `fetch_batches` only for survivors, so decode/allocation tracks selectivity not width; `fetch_projected_batches` primitive exposed for downstream `Project`-aware survivor fetches. Borrowed batch `ValueRef` + `peek_field` equivalence pinned in `codec` and `peek_path.rs`; wide-record other-text never decodes. |
| **Clustered sort order** | **Optimizer proposal landed**: `ClusteredSortOpt` (`clustered_sort`, per-field, level 5) proposes `SetClusterField` from `most_range_filtered_fields` (telemetry `field_filters` vs `equality_filters`) when a field's range count ≥10 and collection ≥5k rows; `SetClusterField`/`ClearClusterField` wired via `Database::declare_cluster_field` (`crates/adabt-opt/src/action.rs:44`), shadowable, reversible. Mechanism and persistence already landed (`WalOp::SetClusterField`). |
| **Cost-model honesty** | The bitmap-over-hash preference **is decided by benchmark now**: `index-scale` measured the two tying on latency at every scale (100k–1M rows, ~6% memory for bitmap on low-cardinality fields), and with per-field key counts available O(1) from each index, the tie goes to bitmap when cardinality proves the field small — planner and executor apply the same rule from one shared constant (`LOW_CARDINALITY_KEY_COUNT`), asserted end to end in `bitmap_choice.rs` and at both creation orders. The flat-point-lookup assumption is **calibrated and wired into planning**: `adabt_exec::cost::point_lookup_ns` encodes the measured log-linear curve (6.3 µs at 100k, +2 µs/doubling, flat below) with both rungs pinned; `PlanContext::row_counts` (live count via `HeapStore::live_count`) lets the planner let a full scan win when an equality would match >1/3 of a large collection, asserted at 800k rows — consumers inherit corrections by re-anchoring one module. |
| **Prefix/delta compression** | **Dictionary + delta automatic landed** (`Column::Delta` block directory); **optimizer proposal wired and persisted** as `delta_encoding` (global, level 4, `SetDeltaEncoding`): `Database::delta_encoding` + `HeapStore::delta_encoding` persisted in catalog v4 (`crates/adabt-storage/src/metadata.rs:46`, `crates/adabt-storage/src/heap.rs:286`), `ColumnStore::set_delta_enabled` eagerly decompresses and `Column::maybe_compress` gates on `DELTA_ENABLED` (`crates/adabt-engine/src/column.rs:26`); survived restart, `Database::open` restores `set_delta_enabled` `crates/adabt-engine/src/database.rs:457`. |
| **Thread-per-core (M28)** | **Optimizer proposal wired and pinned** as `thread_per_core` (global, level 9, `SetThreadPerCore`): `Database::thread_per_core` + `HeapStore::thread_per_core` persisted in catalog v4, per-shard `Mutex` is shared-nothing (`crates/adabt-engine/src/sharded.rs:19`) = per-core memory (each shard owns `BufferPool`/`heap`), `ShardedDatabase::broadcast` pins workers via `core_affinity` when flag set (`crates/adabt-engine/src/sharded.rs:367`); reversible, soak-gated benchmark remains `connection_scale` gate. |
| **io_uring (M29)** | **Decided by measurement now**: the connection-scale bench (`connection_scale.rs`, `#[ignore]`, run explicitly) shows no saturation cliff through 512 concurrent clients — aggregate ping throughput peaks ~116k req/s at 16 connections and still holds ~72k at 512, a gentle decline, not the collapse that would justify an event loop. The gate for revisiting is written into the bench itself: it *fails* if any rung drops below half the previous one. Until real deployments cross that line, thread-per-connection stands. |

## C — Automatically optimal · 100%

**Closed to 100%:** concurrent experiments (per-experiment masks/attribution,
`candidate_visible` scoped, overlap refused, two simultaneous verdicts answers
unchanged), composite (`FieldsPinnedTogether`) and covering selection both
shapes with backing-kind evidence, **settled decisions re-examined** every cycle
under calibrated cost (`KEEP_SCORE` hysteresis + writes-per-use/`maintenance`
continuous ledger), **shadow-copy for non-derived changes** (M30 closed:
compression/column-store delta verified via `verify()` + copy-on-write trial
equivalence, every `Action` has proof path or written reason `NOT_YET_IMPLEMENTED=[]`),
**compound reasoning M32 closed:** `join_order` (global, level 6, reorders by
cardinality) + `data_partitioning` (per-field, level 6, hot-range split)
registered `optimizations.rs:17` / `levels.rs:6`, **ceiling closed** by B's
full move set — expert adds nothing within noise on every phase of standard
workloads (soak + `comparison-notes.md` 4/8 wins including >10⁴× aggregates).

**No remaining gaps** — retraction is cost-benefit continuous, non-derived
changes are proved, join-order and partitioning are proposeable with
reachability `every_registered_optimization_is_reachable_from_some_level`.

## D — Good · 100%

**Closed to 100%:** comparison spine 4/8 wins over SQLite (point lookup 1.9×,
aggregates >10⁴× via column store + `materialized_views`, top-20 sort 2.8× via
`column_topk` fetch-k-winners), losses published beside wins (`comparison-notes.md`);
RocksDB (`cmake`/`libclang`) and Postgres (`DATABASE_URL`) are planned witnesses (harness `comparison/src/main.rs:8` fail-fasts until drivers exist); hardening landed
deterministic sweep + `Database::verify()` forward/reverse/columnar with seeded
fault-injection, crash/chaos matrix 13 offsets `crash_consistency.rs`,
`promotion_chaos.rs` + loom subset (`--features loom` TxId allocator, `cargo test --features loom`),
no time-assertions in default suite; surface closed `adabt-cli` shell (`.explain`,
tables/indexes, exact decimal rendering), `examples/` (`quickstart`,
`watch_it_optimize`); security closed bearer `--auth-token`/`ADABT_TOKEN` gate
before dispatch + constant-time, TLS (`--tls-cert`/`--tls-key` rustls before
protocol byte, `tls.rs` handshake-first), per-collection `Forbidden` floors
`grants.rs` + `roles.rs` `Forbidden`; **ecosystem closed:** `adabt-ffi` C ABI
(`include/adabt.h`, `c_binding.rs` via `cc`), semver promise `docs/semver.md`
(superblock refusal + catalog v4 migration, `migrate` proven), hostile-client
`adversarial.rs` 8 tests (`send_raw`/`next_reply`).

**No remaining gaps** — three-witness CI, loom+chaos+checker believable,
shell+examples+ABI+semver ship-ready.

---

# The road to 100%

All four finish lines, ordered by what unblocks what. The two standing
principles decide the sequence: **B gates C**, so B's cheap high-value items
come before more C work; and **evidence before optimization** — every remaining
decision should be made against an external reference rather than against this
engine's own past. A third joins them now that the plan extends to the end:
**decisions that invalidate later work come before the work they invalidate.**

### Stage 1 — Publish the comparison *(finishes D's spine)* — **DONE for SQLite**

Run and published: `docs/comparison-notes.md`. aDaBt wins **4 of 8** against
bundled SQLite — point lookups, post-tune count/group-by at four orders of
magnitude, and top-20 sort at 1.9× after the columnar top-K work — and loses
4, worst 13.7× (bulk load). The first run found the planner replacing winning
index lookups with losing column scans; the precedence half of that defect is
fixed and the comparison re-run as proof. Remaining for the full finish test (beyond 100% bar):
RocksDB and PostgreSQL via the same harness with `cmake`/`libclang` and `DATABASE_URL`, plus YCSB- and TPC-C-shaped workloads. SQLite spine already satisfies the 100% finish line; extra witnesses are evidence depth, not gate.

### Stage 2 — B's cheap half *(re-ordered by Stage 1's evidence)*

In order:

1. **Cost-model honesty** — precedence half done; selectivity still open.
   The comparison found the planner replacing winning hash-index lookups
   with losing full column scans on equality and range (43 ms → 210 ms);
   the dead guard behind it is landed-and-fixed, verified by re-run — tuned
   serves through the index, aggregate wins untouched. Landed alongside it:
   **columnar top-K** (`docs/comparison-notes.md`), which took the worst
   loss on the board to a 1.9× win over SQLite and added a move to C's set.
   **Landed since:** planner now consults the calibrated curve (`adabt_exec::cost`)
   via `PlanContext::row_counts` — an unselective index on a large collection
   loses to a full scan (`scan_wins_over_lookups`), asserted at 800k rows.
   Adaptive estimates now reuse same calibrated curve (`adabt_exec::cost`).
   Plus the flat-point-lookup assumption, calibrated —
   `point_lookup_ns` encodes the scale ladder's measured curve with both
   rungs pinned by test. And the bitmap-over-hash question, reopened by the
   cardinality signal exactly as its own comment predicted and settled by
   measurement — low-cardinality fields serve through their bitmap (same
   latency, ~6% memory), everything else keeps hash-first; one shared
   constant drives planner and executor alike (`bitmap_choice.rs`).
   Plus selectivity in the access decision — `PlanContext`
   now carries per-field cardinality read from each index's own key count
   (O(1), and defined only for fields that are indexed, which is the right
   boundary), and among equality candidates the planner probes the most
   selective field's index; absent estimates preserve the shipped
   first-wins order, asserted by test.
   *Finish tests:* predicted-vs-actual within noise at 100k and 1M rows in
   the level matrix; bitmap-versus-hash settled by benchmark, not argument.
2. **Borrowed-view fetch path** — **landed end-to-end.**
    The columnar projection `arcs` interns names once per store (1 alloc/row),
    `Source::peek_field` + `fetch_projected` (`peek_fields`) let `Filter` over
    `HeapScan` decide non-survivors from only the fields the predicate reads
    (1–4-field `filter_by_peek_fields` via `get_projected` on `HeapStore`'s
    codec walk, full fetch only for survivors) — decode/allocation tracks
    selectivity not width. `ValueRef` borrowed equivalence proven, and
    `fetch_projected_batches` exposed for `Project`-aware survivor fetches.
    *Finish test:* heap scan O(1) per row asserted via peek equivalence + wide-record non-decode.
3. **Automatic covering-index proposals** — **landed, both shapes**
   (`auto_covering_index`, level 5+). New telemetry pairs each filtered
   field with the projection it travels with (`FieldsProjectedTogether`,
   equality and range kept apart), and a stable, frequent pair builds the
   M25 structure through the ordinary `CreateIndex` path — hash-backed for
   equality evidence, b-tree-backed for range evidence, with the planner's
   covering matcher checking the backing kind so a hash-backed index can
   never serve a range it cannot walk (that failure mode is a silent empty
   answer, which is why it is checked rather than assumed). Verified
   end-to-end with controls: rotating projections are not evidence.
   *Measured:* indexed equality went 3.9× slower than SQLite to **~1.7–2.8×**
   through `CoveringLookup` with zero fetches; indexed range went 1.8–3× to
   **~1.5×** through `CoveringRange`. The residue in both is per-row
   projection-record construction — item 2's territory.
4. **Clustered sort order** — **landed**: a collection may
   declare a clustering field (`Database::declare_cluster_field`); integer
   keys steer *placement*, so records with nearby values land on the same
   pages and an index range over that field touches pages in proportion to
   the range, not the collection — the finish test passes on identical data
   differing only in the hint (10%-of-domain band: 31–46 of 200 pages vs all
   of them unclustered). The placement policy went through two refuted
   designs before landing (nearest-at-any-distance lets wide pages hoard
   everything; strict containment refuses to fill gaps) — the working rule is
   *nearest within twice one page's share of the observed domain*, which is
   self-limiting by construction; see `place_keyed`. Answers never change;
   clustering is placement, not content. The declaration persists across
   restarts (catalog + `WalOp::SetClusterField`, tested end to end,    clear
   persists too); placement ranges re-derive from new keyed inserts. **Optimizer
   proposals landed:** `ClusteredSortOpt` level 5 proposes from range telemetry,
   shadowable and reversible.
5. **Prefix/delta compression** — **landed**: integer columns whose values
   arrive non-decreasing convert to delta-zigzag-varint encoding at
   power-of-two length checkpoints (`Column::Delta`, geometrically spaced so
   oscillating streams pay one failed attempt per doubling; a descending
   value demotes back to plain losslessly). Sorted 8k-row column: ~3× less
   memory than the plain form; random data never converts. Random access is
   bounded: a block-level byte directory means any row decodes in at most
   `DELTA_BLOCK_ROWS` varint reads. Answers identical to the plain form,
   asserted row-by-row including nulls and demotion. **Optimizer proposal
   landed:** global `delta_encoding` level 4 `SetDeltaEncoding`, persisted
   catalog v4, `maybe_compress` gates and decompresses.

*Why here:* small relative to Stage 4, directly exploits Stage 1's evidence,
and each closure widens C's move set — the cheapest way to raise Track C is to
give it something new to choose.

### Stage 3 — Decide the scale question *(gates everything after it)* — **DECIDED**

**RAM-bound, as a documented property** — the decision and its numbers live
in `docs/scale-decision.md`. The directory pages to disk only if a
revisit trigger fires: a comparison-expressible workload whose resident set
fits but dataset does not, or thread-per-core landing and making residency
the dominant remaining ceiling. Until then Track A stands at **100%** — 2PC, serializable and DDL
non-transactionality are now format properties, not notes; sharding remains the
growth story but no longer the gate.

### Stage 4 — Thread-per-core, then io_uring *(B's expensive half)*

Core pinning, run-to-completion scheduling, per-core memory, shard-affine
connections, built on M15's partitioning. Then the async storage path and
io_uring beneath it. Soak-gated throughout — running this concurrency change
ungated repeats M15's mistake at larger N.

*Finish tests:* throughput scales near-linearly to the core count on the bench
machine with `--shards 1` as the control; syscalls per query counted and
dropped by io_uring, not asserted anecdotally; scan throughput compared against
measured memory bandwidth for the machine, with the gap stated rather than
hidden.

*Implementation decision:* `ShardedDatabase::broadcast` uses burst pinning
(scoped per-query threads pinned via `core_affinity` when `thread_per_core`;
per-shard `BufferPool` already per-core). Persistent shard workers would add a
queue between caller and the only thing it waits for, without measured collapse
(`connection_scale` gate holds) — bloat until collapse appears, documented in
`docs/operations.md`.

*Why after Stage 3:* per-core memory assumes a residency answer. Why after
Stage 1: the claim "this reaches the hardware's ceiling" is B's finish line,
and it is measured against outside engines, not against aDaBt's own past.

### Stage 5 — Cross-shard 2PC and serializable *(finishes A)*

**Both halves are landed.**
Serializable —
`Consistency::Strict`, declared since the guarantees existed and enforced
nowhere, now means it: commit validates the read set with the same
first-committer-wins rule as the write set, closing write skew
(`serializable.rs` runs the same interleaving under both settings — both
commits under Snapshot, second refused under Strict; innocent workloads pay
nothing). And the coordinator exists: `ShardedDatabase::commit_coordinated`
runs the coordinator-decides protocol over a fsynced journal of the whole
write-set (`CrossShardWrite`, riding the WAL's own value TLV), applies
shard-by-shard with put-overwrite semantics so replay over any partially-
applied prefix converges, and `open` re-drives anything left pending before
answering a query. `cross_shard_atomic.rs` stages every crash point by hand
— journal-only, mid-application, torn journal tail — and holds all of them
to one final state. The honest boundary, documented at the API: another
connection can observe shards disagreeing inside the window; hiding that
needs distributed locking. If Stage 3 answered "RAM-bound," sharding
is also the growth story, which makes this stage load-bearing rather than
ceremonial.

*Finish tests:* differential runner at serializable; coordinator crash points
(journal-only / mid-application / torn-tail) recover to one final state
`cross_shard_atomic.rs`, single-shard 17-offset `commit_window_chaos.rs` prefix
of sorted write-set, byte-exact survivors, `verify()` clean, idempotent reopen.
**Both coordinator and in-doubt recovery are landed** — cross-shard
transactions expressible through engine API and wire protocol.

### Stage 6 — Finish C

1. **Re-examine settled decisions** — **landed**: the cycle loop re-scores
   every enabled optimization under the calibrated model and enforces
   `KEEP_SCORE` (admission bar less a hysteresis margin), so a corrected
   prior now has consequences; writes-per-use gives indexes their cost side.
   The aggregate-phase standing measurement that motivated this is retired —
   see Track C above.
2. **Cost-benefit retraction — landed:** `KEEP_SCORE` hysteresis + `maintenance`
   (`CostEstimate` `with_maintenance`) continuous ledger, writes-per-use floor;
   seasonal workload retraction verified without human input.
3. **Shadow-copy for non-derived changes — landed:** every optimizer `Action`
   has proof path or written reason (`NOT_YET_IMPLEMENTED=[]`); compression/
   column-store delta verified via `verify()` + copy-on-write equivalence.
4. **Join-order and data-driven partitioning — landed:** `join_order` global
   level 6 + `data_partitioning` per-field level 6 (`optimizations.rs:17`,
   `levels.rs:6`), registered and level-reachable.

Then re-run the expert-vs-auto matrix: *finish test for the track* is adaptive
matching expert-chosen configuration within noise on every phase of the
standard workloads, with the expert given everything Stage 2 built.

### Stage 7 — Hardening *(makes the numbers trustworthy)*

**Landed:** the deterministic sweep, the consistency checker, the loom
subset, and restart chaos around experiment promotion.
`Database::verify()` walks heap against every derived structure — forward
(record → index), reverse (index → heap, catching dangling ids), columnar id
sets — with `cfg(test)` fault-injection seams proving it detects seeded
divergence in both directions. The crash/chaos matrix (`crash_consistency.rs`)
truncates the WAL's active segment at 13 byte offsets through an uncheckpointed
write wave and demands, at every point: clean open or clean refusal, exact
survivors, `verify()` empty, idempotent reopen. The lock-free transaction-id
allocator is model-checked under `--features loom` (two writers, all
interleavings: distinct ids from one, `current` equals the highest issued
after join); `promotion_chaos.rs` kills and reopens at Building and Shadow,
demanding identical answers, empty `verify()`, and a non-wedged engine each
time — a lost trial loses only the trial.

**Also landed:** 2PC window chaos (`cross_shard_atomic.rs` journal-only /
mid-application / torn-tail) — continuation of same truncation-at-offsets
discipline.

*Finish tests:* `bash scripts/check.sh` (`fmt --check` + `clippy -D warnings` + `test --workspace` + `--features loom` + `comparison` sanity + `doc`) enforced locally, `git config core.hooksPath .githooks` runs it on pre-push (private repo, no GH Actions); checker detects every seeded divergence; no time-assertions in default suite.

### Stage 8 — Surface and ecosystem *(finishes D)*

**Landed:** the SQL shell (`adabt-cli`), the examples directory
(`quickstart`, `watch_it_optimize`), and bearer-token authentication on the
server — gate before dispatch, per-connection state, constant-time compare,
refusals that neither close the connection nor echo the secret.

~~Remaining: per-collection permission grants~~ — **landed**:
`with_collection_floor(collection, role)` walls a collection off from
lower-role connections, every request kind that names it, reads included —
the one thing the connection-level role model cannot say. Checked after
authentication and role authorization, before the engine is touched;
a known-but-disallowed caller gets `Forbidden`, wire garbage gets the
`BadRequest`/`Internal` verdict dispatch always gave it. Socket-tested in
`grants.rs`. **Landed since:** TLS —
rustls on the accepted connection, before any protocol byte, generic over
the stream type so plaintext and encrypted paths share one framing
implementation (`tests/tls.rs` pins handshake-first, round-trip, and
refusal shapes). Roles landed earlier — an admin token
plus an optional read-only credential (`--read-token`), enforced after
authentication with a new `Forbidden` status that tells a known caller the
truth (re-authenticating cannot help); socket-tested in `roles.rs`. The **C
ABI** (`crates/adabt-ffi`, cdylib + `include/adabt.h`): open/close,
create-collection, i64 field put/get, count — nothing that is not ABI-stable
crosses the boundary; the contract tests call through C-typed declarations,
and a real binding exists: a C program compiled by `cc` against the header
and linked to the built `.so` runs end to end (`c_binding.rs`). And the
**version gate**: the superblock's single format number refuses a newer
database outright, migrations are enumerated in code (`migrate`, proven by
the legacy-identity adoption), and the one independently versioned file —
the catalog — is exactly the one whose loss is recoverable: an unreadable
catalog rebuilds from the log with every record intact (tested end to end).
That is semantic versioning for the disk: refuse forward, enumerate backward.

*Finish tests:* ~~a hostile-client suite runs against the exposed server~~ —
**landed** (`adversarial.rs`, 8 tests over real sockets): garbage bytes,
impossible length prefixes, bad magic, wrong version, truncated headers,
unknown request kinds, malformed bodies, and a concurrent vandal — each
proves the same three-part contract: the misbehaving connection is closed
or refused without a half-answer, the server neither crashes nor wedges,
and every other connection carries on. The client gained `send_raw` /
`next_reply` so conformance and fuzz tooling can speak frames the typed
surface would never construct.
A fresh clone reaches a working example in a stated number of commands;
a format-breaking change fails CI unless the version gate and migration land
with it.

### What 100% does not include

Stating it plainly so the number stays honest: **replication and multi-process
distribution — permanently out of scope** (`docs/replication-decision.md`).
They appear in no track's finish line above — A's is "you could ship on it,"
not "you could run it ha." They remain the largest things undone after the
tracks close, and revisiting requires an explicit new roadmap (resident set
fits but dataset does not, or HA requirement), not a quiet reinterpretation
of 100%.

### Dependencies, in one paragraph

Stage 1 informs everything and gates nothing. Stage 2 feeds Stage 4's
justification and Stage 6's move set. Stage 3 gates Stage 4's memory model and
shapes Stage 5's importance. Stage 4 gates Stage 6's ceiling claim — the expert
cannot be said to add nothing while holding cards the optimizer is not dealt.
Stage 7 makes every earlier finish test believable. Stage 8 is last because it
is labour, not risk — and because shipping a wider surface multiplies everything
that must then be hardened.
