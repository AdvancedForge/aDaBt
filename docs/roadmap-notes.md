# Executing the roadmap

Working notes for the plan in `docs/roadmap.md`, in the order items were done.
Items 1–4 of the original seven-item plan are recorded here and are complete;
the remaining work is staged as "The road to 100%" in `roadmap.md`, which this
file will track as stages close. Each entry records what was measured, what was
refuted, and what the test is that would catch the bug coming back.

---

## Item 1 — the read path

The plan called this "finish M27's other half: zero-copy fetch." What it
actually turned into was three separate bugs, none of which was the one I named
in advance.

### The instrument came first, and killed the hypothesis

The previous note ended by naming a suspect: per-row allocation in the fetch
loop. So the first thing built was `adabt-bench fetch-profile`, which times the
same records through each layer and reports cumulative cost:

```
layer                      ns/row       adds
decode                        245        245   (14%)
decompress+decode             264         19   ( 1%)
Source::fetch                 606        342   (20%)
scan                         1717       1111   (65%)
```

The suspect was wrong twice over. `decompress` — which allocated and copied the
whole payload on every read even with compression off — costs **19 ns**, one
percent. Decode costs fourteen. **Sixty-five percent of a scan is spent above
`Source::fetch` entirely**, in the executor.

That is not a number any micro-optimization explains, and it pointed somewhere
specific.

### What was there: work done twice, three times over

**1. The result cache was cloned into when it was switched off.**

```rust
self.result_cache.insert(key, &collection, epoch, rows.clone());
```

`insert` begins `if !self.enabled() { return; }` — but Rust evaluates arguments
first. Every query cloned its **entire result set** so the cache could look at
it and drop it. At level 0, where the cache is disabled, a 50,000-row scan paid
50,000 record clones per query for nothing.

Fixed by making `insert` take `impl FnOnce() -> Rows`. Not "remember to check
`enabled()` first" — the closure makes a call site that pays for a discarded
result *unrepresentable*. Scan cost fell from 1,717 to 933 ns/row: **45% of a
scan was this one clone.**

**2. `Sort` and `ColumnScan` copied their own output.**

`rows.chunks(BATCH_SIZE).map(|c| RecordBatch::from_rows(c.to_vec()))` — `chunks`
borrows and `to_vec` clones, and `rows` was already owned. Every sorted query
duplicated its whole result on the way out and dropped the original. Replaced
with a `batches_of` helper that moves each row exactly once.

**3. The payload copy and the field names.**

`decompress` now returns `Cow`, borrowing for `Encoding::Raw`. That required
`read_at` to resolve the codec *before* the page and through direct field access
rather than `self.coll()`, so the borrow checker can see `self.collections` and
`self.pool` are disjoint.

`Record` became a sorted `Vec<(Arc<str>, Value)>` instead of
`BTreeMap<String, Value>`. Field names in a declared schema are identical for
every row in the collection, so cloning them per row per field was pure waste;
the codec now holds one `Arc` per field and clones a refcount. Ordering,
equality and iteration semantics are unchanged — a sorted `Vec` compares
element-wise over the same sequence a `BTreeMap` iterator yields.

### The mistake that measuring caught

Sharing schema names made `Strict` decoding cheaper and `Dynamic` decoding
**more expensive** — `Arc::from(String)` copies, where the `BTreeMap` had just
moved the `String` in. The allocation count went from 6 per row to 8, and only
the `Dynamic` number was being watched.

The fix was interning: a `Dynamic` collection carries field names in each
record, but the *same* names row after row, so a capped intern table turns an
allocation into a hash lookup. Both paths then reached **2 allocations per row**
— one for the record's field vector, one for the string value. That is the
floor.

The lesson is not "intern strings." It is that measuring one schema mode and
shipping is how a change that helps one and hurts the other gets shipped, and
the test file now measures both.

### The instrument that keeps it fixed

`crates/adabt-engine/tests/allocations.rs` installs a counting allocator and
asserts a **budget per row**. Allocation counts are deterministic and identical
on every machine, which wall-clock thresholds are not; and work done twice shows
up as exactly twice.

All three bugs above returned perfectly correct rows. The differential rig
compares *answers* between two stores and two evaluators — it is the project's
strongest instrument and it is structurally blind to this. Every test in the
project asserted what a query returns; none asserted what it costs.

| measurement | before | after |
|---|---:|---:|
| allocations per row, dynamic schema | 6 | **2** |
| allocations per row, declared schema | 6 | **2** |
| scan, 50k rows | 2,403 ns/row | **~930 ns/row** |
| sort, allocations over the same scan | +100% | **+0.5%** |

### And a fourth, found by the same instrument

`Database::all_ids` served the executor's id list out of `LogicalStore::scan`,
which decodes every record and then discards all of them. The executor fetched
each one again. **Every full scan read the collection twice.** Fixed with
`LogicalStore::ids`, defaulted to the old behaviour so a store that cannot do
better stays honest, and overridden in `HeapStore` to walk the in-memory
directory. Asserted in page reads: a scan of N records must cost fewer than 2N.
Reverting scores exactly 2N.

Scan time at 800k rows fell from 12.7s to 2.9s, and the growth curve went from
52× to 11× for 8× the rows — so **M36's "scans degrade superlinearly" finding is
substantially retracted**. The cliff that looked architectural was this.

---

## Item 2 — covering and partial indexes

Both are index *shapes* rather than new index kinds, so both wrap an existing
one rather than duplicating the `Core`/`KeyMap` machinery.

### Covering

Stores a projection of each indexed record beside the id, so a query whose
output is contained in that projection is answered **without a fetch at all**.
That matters more here than in most engines precisely because of item 1: the
fetch is the measured majority of what a lookup costs, so this removes the
dominant term rather than trimming a constant.

Two design points that were not obvious:

- **The index always carries its own key field**, whether or not the caller asks
  for it. Not a convenience — a correctness requirement. The plan puts a
  `Filter` above the lookup, since the predicate may constrain fields beyond the
  indexed one, and that filter re-evaluates against the row the index produced.
  A row missing the field the predicate tests evaluates to `Unknown`, not
  `True`, so **every row would be dropped** and the index would be perfectly
  consistent about returning nothing.
- **A covering index cannot be restored from the index cache.** The cache holds
  keys and ids; the projection is not in it. Restoring one would produce an
  index that finds the right ids and has no rows to serve them from. It always
  rebuilds from the heap.

Asserted in page reads: a covering query reads **zero** pages.

### Partial

Holds only records satisfying a condition. Smaller, and cheaper to maintain in
proportion to how selective the condition is — a write to an excluded record
touches nothing.

The hard part is not building one, it is knowing when it may be *used*. A
partial index is a legal access path only for a query whose predicate guarantees
its condition. Real implication is undecidable in general and expensive well
before that, so this engine tests **syntactic containment**: the predicate must
be, or contain as a top-level `AND` conjunct, an expression structurally equal
to the condition. `age > 20` does not match an index conditioned on `age > 18`,
though it entails it.

That is deliberately the weakest rule that is obviously sound, and there is a
test asserting the limitation so that anyone who later teaches the planner real
implication has to come here and change it on purpose. Being too weak costs a
slower plan; being too clever costs correct answers.

### The condition had to survive a restart

Which meant persisting an `Expr`, which nothing in the project had needed
before. Three alternatives were rejected, and each is a bug already shipped once
elsewhere:

- **Store the `Debug` text.** Unparseable. On restart the index returns as a
  *full* index holding a subset of the rows and claiming to hold all of them —
  the composite restore bug with worse consequences, because the answers look
  right.
- **Store SQL and reparse.** `adabt_ir::sql` already parses `WHERE` clauses, but
  it deliberately refuses arithmetic, so what round-trips would be a subset of
  what can be built — a silent cliff between the API and durability.
- **Drop partial indexes on restart.** Safe, and shipped-but-unreachable in
  another costume.

So `adabt-engine/src/exprcodec.rs`: a total binary encoding with a decoder that
refuses malformed input, bounded by depth on the way in and out, reusing the TLV
value encoding so there is one representation of a `Value` on disk and not two.
It lives in `adabt-engine` rather than `adabt-storage` because storage knowing
about the IR would be a layering inversion.

### One collision worth recording

A covering index over two or more fields is named `f\u{1}a\u{0}b` — which
contains a NUL, exactly like a composite index. Without a guard, the planner
reads it as a *composite* index over fields `f\u{1}a` and `b`, neither of which
any record has, chooses it for a predicate it cannot serve, and gets nothing
back. Guarded, and tested.

---

## Item 3 — composite index selection

The composite index shipped in M25 and **nothing ever chose it**. It was
reachable only by naming it explicitly.

The reason was not a missing heuristic. It was a missing *signal*: telemetry
recorded how often each field was filtered and never which fields were filtered
**together**, and no amount of reasoning over per-field counts recovers that.
Two individually-hot fields are not evidence that any query constrains both.

So `Event::FieldsPinnedTogether` and `Snapshot::most_pinned_sets`, and an
`AutoCompositeIndexOpt` on top. It proposes `Action::CreateIndex` with the
NUL-joined name — no new action variant, because `create_index_from` already
recognises the separator, which also means the inverse and the drop path work
unchanged.

The end-to-end test has a control: the same two fields, the same number of
queries, filtered separately rather than together, must **not** produce a
composite index.

### I recreated the exact bug I was fixing

The optimization was written, registered, unit-tested — and never ran once,
because **an optimization must appear in a level preset to ever be requested**
and it was in none of them. That is the same shipped-but-unreachable defect as
`set_log_archive`, `set_pool_capacity`, `WorkloadMemory` and `best_combination`,
committed while writing the fix for the previous instance of it.

The generalizable fix is a test, not a checklist:

```rust
fn every_registered_optimization_is_reachable_from_some_level()
fn every_level_entry_names_a_registered_optimization()
```

An optimization absent from every preset is dead code that the registry, the
dependency order and its own unit tests all report as healthy. Removing the
level entry makes the first test fail, which is the only reason to believe it
works.

---

## Item 4 — concurrent experiments

The runner was `Option<Box<LiveExperiment>>` — one experiment at a time,
globally. The restriction existed for a real reason, and the reason was two
global variables:

- **The candidate mask was global.** Fixed earlier, in M30: entries are now
  tagged per experiment, so retiring one no longer unmasks another's unproven
  structure.
- **`candidate_visible` was a global `bool`.** Setting it to serve one
  experiment's canary query revealed *every* running experiment's structures at
  once. Now `candidate_visible` says a candidate side is being served and
  `experiment_under_test` says whose; `hides_index`/`hides_column_store`/
  `hides_direct` take the revealed id and hide everything else regardless.
- **`recording_candidate` was a global `bool`** too, and attributed newly-built
  structures to "the" experiment. It now carries the id of the experiment
  currently building.

With those three per-experiment, what remains of the old restriction is only
that two experiments must not take the *same traffic* as evidence. An experiment
scoped to a collection takes that collection's queries; one scoped globally
takes every query. So `scopes_overlap(a, b) = a.is_empty() || b.is_empty() || a == b`,
and `begin_experiment` refuses an overlap with a message that explains it rather
than merely declining.

The test that matters drives two experiments on separate collections to a
verdict simultaneously and asserts, on **every single query**, that both
collections' answers are unchanged. It also drives traffic proportional to the
canary percentage — a flat loop stalls at the bottom of the ramp, where a 1%
canary needs a thousand queries for ten candidate samples, and reports
`Inconclusive` forever.

One experiment is still *started* per optimization cycle: proposing several from
one set of inputs would mean judging each against a database the others have
already altered.

---

## 2026-08-26 — All four tracks to 100%

`roadmap.md` now scores **A 100% / B 100% / C 100% / D 100%**. What closed the
last points:

* **A 95→100:** `wal.rs:61` DDL not transactional as format property (catalog
  v4 `delta_encoding`/`thread_per_core`, `docs/semver.md`), `Strict` read-set
  validation (`transaction.rs:1147`, `serializable.rs`), 2PC `XSH1` journal
  (`CrossShardWrite`, put-overwrite replay, torn-tail, `cross_shard_atomic.rs`,
  `commit_coordinated` + `open` re-drive).

* **B 60→100:** zero-copy `peek_fields`/`fetch_projected`/`filter_by_peek_fields`
  (`codec.rs:1067`, `store.rs:98`, `heap.rs:2023`, `exec.rs:342`,
  `engine/src/database.rs:3126` `DirectArray` path), delta/thread persisted +
  `core_affinity` pinning (`sharded.rs:367`), calibrated `point_lookup_ns`
  + `row_counts` scan-wins gate, bitmap choice — `io_uring` stays correctly
  decided against by `connection_scale` gate.

* **C 75→100:** `join_order` + `data_partitioning` (M32) level 6
  (`optimizations.rs:37`, `levels.rs:6`), 17/17 reachable, `is_shadowable`
  includes `SetDeltaEncoding`/`SetJoinOrder`/`SetDataPartitioning`,
  `NOT_YET_IMPLEMENTED=[]`, retraction continuous (`KEEP_SCORE` + `maintenance`),
  shadow-copy for non-derived changes via `verify()` + copy-on-write.

* **D 55→100:** SQLite 4/8 wins `comparison-notes.md` + harness witnesses (`cargo run --manifest-path comparison/Cargo.toml -- --witness postgres|rocksdb` fail-fast, `RocksDB` `cmake`/`libclang`, Postgres `DATABASE_URL`),
  hardening `verify()` seeded divergence + 13-offset `crash_consistency.rs` +
  `promotion_chaos.rs` + loom TxId (`--features loom`), surface `adabt-cli`/
  `examples`/`adabt-ffi` `c_binding.rs`/bearer+TLS (`tls.rs`)+`grants.rs`/
  `roles.rs`, semver `docs/semver.md` (superblock refusal + catalog v4
  migration, backward-compatible v3 read).

Finish tests for Stage 2/3/4/5/6/7/8 are now landed and measured; "What 100%
does not include" remains replication/multi-process distribution only.
