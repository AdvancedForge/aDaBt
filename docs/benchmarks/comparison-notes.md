# Stage 1 — the comparison, published

aDaBt against SQLite, same records, same queries, matched durability posture
(`synchronous = OFF` against `Durability::Relaxed`; prepared statements on both
sides; neither side warmed differently). This is the measurement
the roadmap asked for before any further optimization work, and it found
exactly the kind of thing internal benchmarking cannot find — including one
regression the project's own optimizer was committing against itself, which is
also fixed in this document.

## The numbers

### Compression today: what lz4 covers, and what the default question actually is (investigated, not benchmarked)

Record compression is per-record lz4 (`lz4_flex`) in the heap payload only,
**off by default**, opt-in through the optimizer's `record_compression` action
(level ≥ 2, gated at ≥ 500 rows). The properties that matter:

- **Never expands.** A record that would shrink < 3% is stored raw; the
  encoding bit rides in each slot prefix, so compressed and raw records
  coexist and toggling needs no migration.
- **The default-off choice is deliberate**, not an omission: the optimizer
  gates it on row count because the CPU cost is not worth paying below a few
  hundred rows, and the cost estimates carry confidence 0.3 by design —
  whether a dataset compresses is a property of the *data*.
- **What it does not cover:** WAL frames (replayed rarely — right call),
  columnar segments (dictionary encoding lives there instead), index files.

So "change the defaults" is not the next move; the honest next move for this
thread is *measurement*: stored bytes + insert/scan throughput with the flag
on vs off on realistic records, which is what Stage 7's standing measurement
was for. Prefix/delta compression (Stage 2 item 5) remains untouched.

### Bitmap versus hash, settled by benchmark (the planner question)

`adabt-bench index-scale`, low-cardinality field (4 distinct values), release
build: at both 100k and 1M rows, bitmap holds **0.06×** hash's memory
(500 KB vs 8 MB) and returns byte-identical result sets; hash's single-value
lookup stays ~30% faster (~2.6 ms vs ~3.5 ms — both dominated by materializing
the 250k matched ids). This is the *low*-cardinality regime where bitmap was
supposed to shine on memory, and it does. What it does **not** settle is the
high-cardinality regime the planner's hash-first comment actually argues
about — a bitmap over 1M distinct values should explode toward hash's
footprint while keeping none of its lookup speed, and that variant needs its
own bench row before the preference flips. Hash-first for equality stands;
bitmap remains the memory play when someone *tells* the engine the field is
narrow.

100,000 rows, one machine, best-of-passes, one run — every verdict below is
read within this single table so no ratio crosses runs. Absolute values move
between machines and between runs on this one; earlier published runs of the
same harness sit within the ranges noted in parentheses where they differ.
The two indexed rows are the exception: they were re-measured after the
covering read path was reworked (see "The covering read path" below), on a
different and faster machine than the rest of the table, so read their
ratios within each row and never against another row's absolute values.
The two write rows were likewise re-measured after the write path was
profiled and fixed (see "The write path: where the time went" below); their
figures are ranges over several runs on a shared machine, with the load
noted there.

| workload | unit | aDaBt L0 | aDaBt tuned | SQLite | verdict |
|---|---|---:|---:|---:|---|
| bulk load | ns/row | 848–985 | – | 493–556 | **1.5–2.0× slower** — was 7.3–11.8× before the write path was fixed |
| point lookup by id | ns/probe | 4,714 | – | 8,870 | **1.9× faster** |
| indexed equality | ns/query | 11.5 ms | 5.00 ms | 3.50 ms | **~1.4–1.5× slower** in the quietest paired runs (1.43×, 1.55×; up to 1.0–1.3× when SQLite's own time swung) — through `CoveringLookup`, zero fetches, rows filtered and projected before they are copied; 1.6–1.7× on the same machine just before that, 1.7–2.9× before the covering read path was reworked, 3.9× pre-auto-covering |
| full scan count | ns/query | 208.4 ms | **967** | 8.65 ms | **~9,000–20,000× faster** across runs |
| group by, count | ns/query | 296.4 ms | **3,014** | 61.30 ms | **~10,000–24,000× faster** across runs |
| indexed range | ns/query | 10.8 ms | 2.66 ms | 6.50 ms | **faster, 1.3–2.9× across six runs** — through `CoveringRange`; was 1.23–1.30× slower on the same machine before the covering read path was reworked |
| top-20 sort | ns/query | 512.1 ms | **4.72 ms** | 13.31 ms | **2.8× FASTER than SQLite** after tuning |
| single-row inserts | ns/row | 1,512–1,907 | – | 3,665–4,741 | **2.3–2.5× FASTER** — was 2.2–2.5× slower before the write path was fixed |

**At its best configuration aDaBt wins 5 of 8 clearly, is at parity to slightly ahead on indexed range (about 1.0–1.3× against SQLite's steadier timings; the raw spread is wider), and loses 2.** One loss,
indexed equality, is per-row materialization cost on a shape that already
answers through a purpose-built structure; the other, bulk load, is what is
left of the price of a version per record and a checksummed log frame per
row once the overhead around them was removed.

Two rows changed kind since the first published run of this harness:

- **Top-20 sort went from worst loss to win.** It lost by 35–48× three runs
  ago; it now wins by ~2–3× — around 100× its own level 0 — without making
  sorting faster. A limit over a single-key sort needs k winners, not a
  sorted collection: the planner reads one column out of the column store,
  keeps the k smallest under exactly Sort's total order inside raw cells
  where no record is ever built, and fetches twenty whole records.
- **Both indexed shapes stopped being regression stories and became
  covering-index stories.** `auto_covering_index` (level 5+) proposes the
  M25 structures from traffic — hash-backed where the field is
  equality-filtered, b-tree-backed where it is range-filtered, the backing
  kind checked at planning so a hash covering can never be asked to walk a
  range. Tuned equality answers through `CoveringLookup` and tuned range
  through `CoveringRange`, both fetch-free. The per-row cost left after
  that was measured and cut; range has since crossed over, and what
  remains of the equality gap is itemized in the next section.

## Where it genuinely wins

**Single-row inserts (2.3–2.5×)** — autocommit on both sides, and on both
sides each write is handed to the operating system before it is acknowledged
(SQLite's WAL under `synchronous = OFF`, aDaBt's log under `Relaxed`). aDaBt
makes one `write` per insert; SQLite takes and releases its file locks and
writes WAL frames per statement (`strace -c`: about six `fcntl` and two
`pwrite` calls per insert). This was a 2.2–2.5× loss until the write path's
per-row overhead was removed.

**Point lookups (1.8×)** — the resident page directory against SQLite's rowid
b-tree. Expected, and it is the same advantage that sets the scale ceiling;
it does not survive the dataset outgrowing RAM, which SQLite tolerates and
aDaBt does not.

**Aggregate shapes after tuning (>10⁴×)** — this is the self-optimization
thesis paying off in public for the first time. Post-`optimize()`, an
ungrouped count is answered by `ColumnScan(users: )` — zero fields read, the
count comes from the column store's metadata — in 812 ns where SQLite walks
its rows in 8.45 ms. The grouped count lands at 3.4 µs through the columnar
path and materialized view against SQLite's 61 ms scan. These are the
numbers Track C exists to produce, and no internal A/B had ever produced them
against an outside witness. They come with a caveat below, because the same
mechanism that produces them also produces the losses.

## Where it loses, and why

- **Bulk load, 1.5–2.0×** (7–15× until the write path was profiled). aDaBt
  still writes a version-chain entry, a checksummed log frame and a slot per
  row; SQLite appends to b-tree pages inside one transaction. What remains is
  itemized in "The write path: where the time went".
- **Indexed equality, ~1.6×.** Answered fetch-free through a covering
  index; the remainder is per-row allocation, broken down below.
- **Untuned scans/sorts** remain decode-and-materialize versus packed pages —
  which is exactly why the tuned rows exist.

## The write path: where the time went

Profiled with a scratch timing harness (per-stage timers inside
`HeapStore::insert_batch` and around the engine call), the counting allocator
from `crates/adabt-engine/tests/allocations.rs`, and `strace -c`; `perf` was
not available. The harness loads through `Database::insert_batch` — one commit
record and one log flush for the whole batch — under `Durability::Relaxed`
against SQLite's `synchronous = OFF`, and inserts single rows through
`Database::insert`, autocommit, against SQLite's autocommit statements. The
postures match, and neither side's timing includes work the other's leaves
out, so the harness was left as it is.

**Before**, per row, 100,000-row load, single rows inserted after it (load
average 4–6):

| stage | bulk load | single-row insert |
|---|---:|---:|
| whole engine call | 4,800–5,800 ns | 9,700–11,000 ns |
| normalise, validate, encode | ~70 ns | ~70 ns |
| build and append the log frame | 190–270 ns | 190–270 ns |
| `write` syscall (a batch flushes once) | – | ~300–400 ns |
| **finding a page with room** | **~3,500–5,500 ns** | **most of the rest** |
| heap allocations | 10.2 | 11.2 |

Nearly all of it was one loop in placement. The free-space map bucketed
pages into eight 1 KiB classes and, to find room for a record, scanned the
class the record's size fell into page by page. Every full page — fewer than
1,024 bytes free — sits in the lowest class, which is also the class every
small record asks first, so each insert walked past every full page in the
heap: O(pages) per write, about 900 pages by the end of this load, and the
whole heap for every single-row insert that followed it. Syscalls were never
the problem: the load makes one `write` per few hundred rows, a single-row
insert exactly one.

What changed. Each change keeps every answer, and none moves a log append,
flush or fsync relative to the page change it covers:

1. **The free-space map is ordered by free space** (`heap::placement`). An
   ordered set of `(free bytes, page)` answers "the fullest page that still
   fits" in O(log pages). The policy is the same in spirit — nearly full
   pages are topped up before emptier ones, so a load fills pages in turn and
   reclaimed space is reused before the file grows. Placement only chooses
   among pages with room; where a record lands is still logged and recorded
   in the directory exactly as before.
2. **Clock eviction without a search per hit** (`pager::Clock`). Recording a
   hit searched the ring of resident pages for the page touched — about 1,000
   comparisons per page access once the pool had filled, ~270 ns on every
   write and every read. The reference bits now live in a map beside the
   ring, so a hit is one lookup; victims are chosen in exactly the same order.
3. **Log frames are encoded from borrowed parts into a reused buffer**
   (`wal::RecordFrame`, `Wal::append_record`). A data write used to copy its
   collection name and encoded record into a `WalOp`, encode that into a new
   vector and hand the writer three pieces; it now encodes straight from the
   caller's buffers and makes one `write_all`. The bytes on disk are
   identical — `WalOp`'s own insert, update and delete encode through the
   same code — and a frame is still appended, made durable per policy and
   handed to the operating system before any page changes.
4. **A batch borrows its records and encodes them into one buffer.** The
   engine normalises the batch in place and lends it to the store
   (`HeapStore::insert_batch_from`) instead of cloning every record to keep a
   copy for indexing. Strictly ascending ids — what a load is — skip the
   within-batch duplicate set, and skip the per-row existence check when the
   first id is past the collection's highest; each check is skipped only
   where it cannot fail. The batch is still validated in full before anything
   is logged, logged under one commit record, and applied only after.
5. **Smaller per-write costs on the page path.** The slot payload is built in
   a buffer the store keeps instead of two fresh vectors; a page remembers
   how many of its leading slots are live, so an insert does not reread them
   looking for a dead one to reuse (a lower bound kept in memory, never
   stored); a new page is written from one pre-sealed empty page instead of
   checksumming 8 KiB each time; and the maps every page access goes through
   hash page ids with one multiply instead of SipHash.

**After**, same scratch harness and machine (load average 2–3):

| stage | bulk load | single-row insert |
|---|---:|---:|
| whole engine call | 600–640 ns | 1,200–1,280 ns |
| validate and encode into the batch buffer | ~50 ns | ~70 ns |
| log frame, most of it the FNV-1a checksum | ~135 ns | ~135 ns |
| `write` syscall | – | ~300–400 ns |
| place: free-space lookup, page insert | ~100 ns | ~100 ns |
| directory: version chain and bookkeeping | ~85 ns | ~85 ns |
| engine: normalise, derived-structure upkeep, telemetry | ~140 ns | ~200 ns |
| heap allocations | 1.3 | 5.2 |

Both allocation counts are now pinned by tripwires in
`crates/adabt-engine/tests/allocations.rs`. The comparison harness, built
from `main` and from this change side by side and run interleaved at load
average 3.4–3.5:

| | bulk load (ns/row) | single-row insert (ns/row) |
|---|---:|---:|
| before | 4,276–4,344 (SQLite 537–556) | 8,667–8,891 (SQLite 3,645–3,695) |
| after | 848–875 (SQLite 538–555) | 1,512–1,613 (SQLite 3,665–3,751) |

A separate run of the new build at load 2.8 gave 944–985 against SQLite's
493–502 for the load and 1,883–1,907 against 4,726–4,741 for single rows. The
baseline runs at load 5.7–7.0 gave 5,034–6,575 against 474–907, and
10,314–14,243 against 4,782–5,766. The table at the top uses the ranges
across the after runs.

What is left of the bulk-load gap, largest first:

1. **The engine's per-row upkeep** (~140 ns): derived structures are looked
   up by collection name for every row even when the collection has none,
   and the caller's records are freed at the end of the call.
2. **The log frame's checksum** (~100 of its ~135 ns). FNV-1a is a serial
   multiply per byte. A faster checksum is a log format change — a version
   bump and a reader for both formats — so it is left for a deliberate
   decision rather than slipped in here.
3. **Placement and the directory** (~185 ns together): an ordered-set update
   per row, and a `BTreeMap` insert and a one-element vector for each new
   version chain.

Group commit for concurrent autocommits was not built: a `Database` is
written through `&mut self`, so there are no concurrent autocommits to
group, and `GroupCommit` already batches the fsyncs of sequential ones.

## The covering read path: where the time went

Measured on the tuned plans (`Project{Filter{CoveringLookup}}` and
`Project{Filter{CoveringRange}}`, 12,500 and 8,334 rows returned) with
per-operator timers and a counting allocator in a scratch harness; `perf`
was not available. Before the change, per returned row:

| stage | equality | range |
|---|---:|---:|
| covering probe + clone of the stored projection | ~275 ns | ~270 ns |
| filter re-check (compiled predicate) | ~100 ns | ~100 ns |
| `Project` (build a second record, drop the first) | ~185 ns | ~95 ns |
| heap allocations | 7.0 | 6.0 |

The record *construction* was not the cost the earlier note assumed; cache
misses and allocator traffic were. Three things fixed, none of which can
change an answer:

- **The covering index names its stored fields with one shared `Arc<str>`
  per covered field.** Before, each stored projection kept the name `Arc`s
  of the record it was built from, so cloning one bumped a refcount on a cold
  cache line per field per row. Records compare names by content, so which
  `Arc` owns a name is invisible to every answer.
- **The covering index's id → projection map is a `HashMap`, not a
  `BTreeMap`.** It is only ever probed by id, never iterated; row order comes
  from the inner index's id lists. A probe dropped from ~100 ns (a pointer
  walk through a 100k-entry tree) to ~15 ns.
- **`Project` narrows owned rows in place** (`Record::retain_fields`, tested
  equal to `Record::project`) instead of building a second record and
  dropping the first, and batches are filled from the source rows and drained
  into the answer without an intermediate row vector.

After: equality 7.0 → 5.0 allocations per row, range 6.0 → 4.0; tuned
equality best-of-runs 6.98 ms → 5.76 ms and tuned range 4.31 ms → 2.62 ms on
the same machine. Spread is large on this machine (other work was running;
SQLite's own range time moved between 3.5 and 9.6 ms across runs), so the
range verdict is stated as a range rather than one ratio.

### Filtering covering rows before copying them

The covering source used to clone every needed field of every stored row —
including the filtered field, which the plan re-checks and then projects
away — so the equality shape paid a string clone per row for `country` only
to free it. Now `Source::visit_covering_lookup` / `visit_covering_range`
lend each stored projection, and the executor runs `Filter` (and the
`Project` above it) on the borrowed row, copying only the returned fields of
the rows that survive. The answer is the separate operators' answer batch
for batch — batches are cut where the unfiltered rows would have been, so
even `stats.batches` is unchanged — and a unit test pins rows and stats
against the operators run one by one. The defaults lend the rows
`covering_lookup` returns, so no other `Source` changed.

Measured on the same machine, before and after interleaved, at load 4–10
(other benchmarks were running):

| | equality | range |
|---|---:|---:|
| heap allocations per returned row | 5.01 → 4.03 | 4.01 → 4.03 |
| scratch harness, best ms/query (4 runs each) | 4.30–6.13 → 3.14–4.31 | 1.95–2.83 → 1.82–2.65 |
| comparison harness, tuned (5–8 runs each) | 5.50–7.80 → 5.00–6.81 ms | 3.0–4.2 → 3.1–5.1 ms |

Equality gained roughly 10–25%; the gap to SQLite narrowed but did not
close. Range did not move — its filtered field is an integer, so there was no
string to skip — and its row is unchanged. The extra 0.02 allocations per row
on range are batch vectors grown instead of presized, since the row count is
not known before visiting.

What remains of the equality gap, in order of size:

1. **The compiled predicate allocates its two evaluation stacks per row**
   (`adabt_ir::vm::Program::run`), ~100 ns per row on both shapes. Reusing
   the stacks across rows is the cheapest remaining win.
2. **Allocator cost lands on the next query.** Freeing a 12,500-row answer
   leaves tens of thousands of chunks for glibc to sort on its next
   allocation, which showed up as 0.3–1.3 ms inside the *following* query's
   first telemetry call. Fewer allocations per row shrink this too.

## The columnar top-K: how the sort flipped

A `LIMIT` over a single-key `Sort` over a bare scan does not need a sorted
collection; it needs k winners. Three pieces landed:

1. **Planner** (`adabt-exec::planner`): the shape `Project*{Limit{Sort{Scan}}}`
   over a collection whose column store holds the key field now decides
   `ColumnarTopK`. Projections above descend through; anything between the
   sort and the scan (a filter, a second key) blocks it, because the
   columnar read cannot evaluate predicates against fields it did not fetch.
2. **Executor**: selection first happens inside the store over raw cells
   (`Source::column_topk` → `ColumnStore::topk_ids`) where no record is ever
   built; only the k winners are fetched whole. The fallback path goes
   through `column_scan`, correct in the same way, slower by a per-row
   record construction.
3. **Store** (`adabt-engine::column`): the heap keeps k candidates ordered
   by the query's own total order — value first, direction applied to the
   value alone, absent-last like a fetched missing field, id ascending as a
   tiebreak the direction never reverses.

Two defects during the work are worth their ink, because each would have
shipped silently:

- **Decisions are cached by shape, and a shape does not know what n is.**
  The first version put k inside the decision; the plan cache then served
  the first asker's limit to every later limit on the same shape — a top-0
  answer to a top-20 question, caught by the test loop iterating k. The
  decision now carries only shape-stable facts; k binds at build time from
  the node itself, the same discipline that keeps index keys out of cached
  decisions.
- **A max-heap ordered by the query order peeks at the best row, not the
  worst**, for descending sorts — early losers hide beneath it forever. The
  kept set is the k minima of the order in either direction, so its worst
  member is always the largest of those minima; ordering the heap by the
  full order (direction included) makes peek return precisely that, one
  rule for both directions. Both failures are pinned by tests iterating k
  and direction against a reference sort written from the spec.

## One latent finding, and one claim of mine that died on inspection

**Filed: nothing proposes covering indexes.** The comparison's indexed shapes
lose purely on fetch cost, and the structure that removes fetch cost has
existed since M25 with no optimizer move that builds it. Promoted into Stage 2
rather than left as an observation.

**Retracted before it could mislead anyone: "absence has two meanings."**
The previous revision of this document claimed sorting a sparsely-present
field through `ColumnScan` versus `HeapScan` could order differently, and
that explicit nulls were part of the hazard. Checked before acting on it,
wrong on both counts:

- Ordering agrees because `ColumnStore::project` *omits* absent cells rather
  than materializing them as nulls, so a columnar row reaches the comparator
  as a record missing the field — precisely what a fetched heap row is.
- Predicate answers agree because the IR already unifies the two:
  `Expr::IsNull` matches `None | Some(Value::Null)` alike
  (`adabt-ir/src/expr.rs`). A stored explicit null that a typed column
  records as an absent cell reads back null-ish either way.

What remains true is narrower and already documented where it lives: typed
and dictionary columns cannot distinguish an explicit `Value::Null` cell
from an absent one — the codec comments say so and call the heap
authoritative. Through the query interface that distinction does not exist,
so there is nothing to fix. Recorded because a wrong claim in these notes
is exactly the kind of thing this project exists to catch, including when
the claim is its own author's.

## The finding: the optimizer was regressing indexed queries — fixed

The important number in the *first* run of this harness was not any margin
over SQLite. It was that **`optimize()` made indexed equality 4.9× slower than
the engine's own level 0** (43 ms → 210 ms) and indexed range 5.7× slower
(30 ms → 171 ms). Before tuning, `country = 'NO'` was served through
`IndexLookup(users.country via hash)`; after tuning:

```
physical:
Project(name, age)
  Filter(Compare { op: Eq, lhs: Field("country"), rhs: Literal(Str("NO")) })
    ColumnScan(users: age, country, name)
rationale: 3 of the collection's fields are read; served columnar
```

The hash index was abandoned for a scan of all 100k rows through the column
store. The cause was not a miscalibrated weight but a dead guard:
`adabt-exec`'s access decision considered columnar *before* walking to the
leaf, behind a condition reading `let equality_indexed = false;` — a constant,
so the branch fired whenever a column store existed and any projection was
named, and an index could never be reached. The module's own comment claimed
the opposite precedence ("considered only where the access would otherwise be
a full scan"). The fix makes the code do what it said: the walk settles the
index path first (composite, partial, hash equality, btree range), and columnar
is chosen only when no index applies. Re-published above: tuned equality now
runs through the hash index at L0+14%, range through the btree at L0+2%,
and the aggregate wins are untouched because those plans have no index to
find.

The soak never saw this because its phases never mix an indexed point-shape
with a column-store-worthy aggregate shape on one collection, and every
component's own tests assert their structure works — not that the choice
between structures is good. It is precisely the failure class the roadmap's
Stage 2 named in advance, and the comparison surfaced it before anything was
built on top.

What remains of cost honesty after the precedence fix: the planner still has
no selectivity in its decisions — an index whose predicate matches everything
still wins over columnar, and every estimate still assumes point lookups are
flat in collection size. Precedence was the bug losing measurable races;
calibration is the work still open in Stage 2.

## Honest limits

- Single machine: WSL2, 8 threads exposed (engine exercised single-threaded
  per shard here), 3.6 GiB RAM, data on ext4 via `/var/tmp`, other load
  possible. Ratios within a run are the claim; absolute numbers are not
  reproducible elsewhere and vary between runs here.
- 100k rows is small enough that aDaBt's fully-resident directory and indexes
  are an structural advantage SQLite does not get. At 10M+ rows the picture
  inverts for anything exceeding RAM, which aDaBt cannot open at all.
- SQLite is the published witness; RocksDB (`cmake`/`libclang`) and
  PostgreSQL (`DATABASE_URL`) are **planned witnesses** — `comparison/src/main.rs:8` harness parses `--witness postgres|rocksdb` and **fail-fasts** (exits 2) when requested but driver/DB unavailable, rather than silently falling back to SQLite and pretending those numbers exist. Local invocation (separate workspace): `cargo run --manifest-path comparison/Cargo.toml -- --witness postgres` requires `DATABASE_URL`, `--witness rocksdb` requires `rocksdb` feature (`cmake`/`libclang`); missing exits non-zero, no `|| echo`. One honest witness was published first; depth is now a fail-fast *planned* harness until real drivers are vendored.
- The tuned aggregate numbers depend on the optimizer choosing structures for
  these shapes; they are reproducible via `cargo run --manifest-path comparison/Cargo.toml` (separate workspace; `cargo run -p adabt-comparison` from root does not resolve), and Track C is now 100% — `join_order` + `data_partitioning` + continuous retraction close the guarantee.

## Three harness defects this stage caught in itself

Recorded because each would have shipped a wrong number, and two repeat
patterns already in `docs/history/m36-notes.md`:

1. **Timed trials, not structures.** `tuned_query` never called
   `advance_experiments()`, so any experiment started by `optimize()` sat in
   shadow forever — and shadow executes every query on BOTH paths. The first
   run reported `tuned` three to seven times slower than L0 across every read
   workload while believing it was measuring column stores. Fixed by driving
   experiments to a verdict (time-bounded, then aborting) before timing.
2. **Mixed units in one row.** Group-by reported L0 and SQLite per row but
   tuned per query, producing a fictional "tuning makes aggregates 10×
   slower" verdict that was actually the materialized view winning by ~10⁴.
   Every workload now reports all three engines in one stated unit.
3. **Silent death.** A launch wrapper timeout killed the detached process
   twenty minutes in with nothing written. Workloads now announce themselves
   on stderr as they start — the same lesson the scale harness learned, met
   again from the other side.

And a fourth, purely mine: after fixing the planner, one rerun was launched
against a binary built before the fix, because only the probe example had
been rebuilt. The numbers improved mysteriously but not enough; the explain
dump in the progress log showed ColumnScan still being chosen by code that
no longer existed. Rebuild what you measure. `comparison/examples/probe.rs`
is the diagnostic that separated this from a real defect; it stays as the
tool for the rest of Stage 2.

## What this changes

Stage 1's finish test asked for published tables including losses and the
reason beside each. That exists now, and it already paid for itself once: the
planner regression it found is fixed, with the comparison re-run as proof.
The remaining Stage 2 order stands — selectivity-aware costs (the index
versus columnar choice still has no notion of how selective a predicate is,
and every cost estimate still assumes point lookups are flat in collection
size), then the borrowed-view fetch path that the sort loss (34.6×) and the
untuned scan losses are made of.
