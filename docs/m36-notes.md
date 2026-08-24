# M36 — scale evidence

Every performance claim in this project's notes rested on a 5,000-row soak.
This measures them where they might break. `adabt-bench scale` and
`adabt-bench index-scale` are the harness; the numbers below are from a
2-core, 3 GB WSL2 machine with a real disk.

## The headline: the 100M-row target is architecturally unreachable

| rows | load/s | RSS MB | marginal B/row | point lookup | filtered scan |
|---|---|---|---|---|---|
| 100,000 | 111,170 | 42.6 | — | 3.2 µs | 0.24 s |
| 200,000 | 67,026 | 118.4 | 758 | 6.0 µs | 1.20 s |
| 400,000 | 11,381 | 231.3 | 565 | 9.1 µs | 3.64 s |
| 800,000 | 8,607 | 449.2 | 545 | 16.5 µs | 12.69 s |
| 1,600,000 | 3,400 | 825.6 | 471 | 44.2 µs | 18.92 s |

**~470–570 bytes of resident memory per row**, converging near 470. That is
not the record — it is the bookkeeping: `HeapStore` holds a
`BTreeMap<RecordId, VersionChain>` entry for *every* record, and every index
lives entirely in memory (`adabt-index`'s module docs say so plainly, as a
deliberate M-early choice).

At 470 B/row, **100M rows needs roughly 47 GB of RAM**. The plan's "100M+
records" finish line is not reachable by adding disk or patience; it needs a
paged directory and an index that can spill. On this machine the ceiling is
about 3M rows. That is the single most useful thing this milestone found,
and no amount of benchmarking at 100k would have surfaced it.

## A hypothesis I had, measured, and had to discard

Scan cost looked like buffer-pool thrashing: `Database::open` fixes the pool
at 1024 pages (4 MiB) regardless of data size, and at 1.6M rows the heap is
far larger than that. The obvious conclusion is that nearly every fetch is a
cold read.

**Measured, and it is not true.** Re-running with a 256 MiB pool — 64× larger
— produced no improvement:

| rows | scan @ 4 MiB pool | scan @ 256 MiB pool |
|---|---|---|
| 100,000 | 0.244 s | 0.372 s |
| 200,000 | 1.166 s | 0.904 s |
| 400,000 | 2.512 s | 2.661 s |

Within noise, identical. So the scan bottleneck is **CPU-side, not I/O**:
roughly 6 µs per row at 400k, spent in the per-row `fetch` path — directory
lookup, page decode, and `Record` construction with its per-field `String`
allocations. That is consistent with what M27 found when it removed clones
from the *filter* loop; the same allocation cost still sits in the fetch
loop, which M27 did not touch.

Recording this because the wrong fix was one commit away. `set_pool_capacity`
is now reachable from `Database` regardless — it was another storage-layer
capability exposed nowhere above it — but it is not the answer here.

## Claims that survived

**M25's bitmap index, at 200× the scale it was validated at:**

| rows | hash bytes | bitmap bytes | ratio |
|---|---|---|---|
| 5,000 | 40,441 | 2,961 | 0.07× |
| 1,000,000 | 8,000,441 | 500,433 | **0.06×** |

The claim holds and gets slightly better. Both return identical rows —
asserted in the harness, not eyeballed.

**One nuance that does not survive.** M25 documented the planner preferring
hash over bitmap for equality, reasoning that materializing a bitmap's
matches costs a scan of its word range. At 5,000 rows that is right — bitmap
lookup was 3.3× slower. At 1M rows the bitmap is *faster* (1.83 ms vs
1.95 ms). The preference is written as a reasoned ordering and the data no
longer clearly supports it. Left in place for now because the difference is
within noise, but flagged: it is a reasoned claim contradicted by
measurement at the scale that matters, and it should be decided by a
benchmark rather than an argument.

## Claims that did not survive

**"An indexed point lookup stays roughly flat as rows grow."** Every cost
estimate in `adabt-opt` assumes this. Measured, it grows: 3.2 µs → 44.2 µs
across 16× more rows — roughly 14×, close to linear. It is genuinely using
the index (a full scan at 1.6M takes 18.9 s, not 44 µs), so the index is
working; what is not flat is the fetch that follows it.

**Write throughput degrades badly with size**: 111k rows/s at 100k, 3.4k
rows/s at 1.6M — a 33× collapse. Not investigated further here; recorded so
it is not discovered later as a surprise.

## Honest limits of this measurement

Single machine, 2 cores, 3 GB RAM, no isolation from other load, three
repetitions per query. Scan ratios between adjacent rungs range from 1.5× to
5× for a doubling, which is too noisy to call the growth curve precisely —
the claims above are stated only at the confidence the data supports.

**No comparison against SQLite, RocksDB or Postgres was run.** The plan asks
for one, including the workloads where aDaBt should lose. None of those are
installed here, and a comparison against absent software is not something to
approximate.

## A defect this milestone's own harness had

The first version checked the memory budget *after* running each rung, so
the run that exhausts memory is the one that has already happened. It was
killed at 30 minutes having printed nothing — the output was also lost to
pipe buffering on SIGTERM. It now projects the next rung's cost from the
measured per-row figure and stops *before* attempting it. Recorded because a
benchmark harness that dies without evidence is worse than no harness.

---

# M30 (attribution), composite indexes, M37 (SQL)

## M30 — the attribution change, and what still blocks concurrency

`retire_experiment` cleared the *entire* candidate mask, so retiring one
experiment would unmask another's unproven structure into live traffic the
instant an unrelated experiment finished. Every masked entry is now tagged
with the id of the experiment that built it, and retirement calls
`Candidates::forget(id)` — dropping only its own. `is_empty()` checks that
were really per-experiment questions asked globally became `is_empty_for(id)`.

`retiring_one_experiment_leaves_anothers_mask_intact` is the regression test.

**Still not done**: the runner itself holds `Option<Box<LiveExperiment>>`,
so only one experiment runs at a time. The two defects that made
concurrency *unsafe* rather than merely unimplemented — the global mask
flags (fixed earlier) and clear-everything retirement (fixed here) — are
gone, but converting the runner to a `Vec` with per-collection routing is
the remaining work. Also unstarted: the shadow-copy mechanism for
non-derived changes, M30's other half.

## Composite indexes

`CompositeIndex` over N fields, keyed by `Value::List` of their values — no
new key type was needed, because `Value::List` already has a total `Ord` and
a `Hash` that agrees with it, so the whole existing `Core`/`KeyMap`
machinery works unchanged.

The index answers to the NUL-joined name of its fields, which means the
existing single-field planner path *cannot* reach one by accident — it
simply never matches, rather than matching wrongly. `PlanContext` gained a
separate `composite` map because the question asked of one is different:
"does the predicate pin every one of these fields", not "is this field
indexed". Longest match wins, since an index over three pinned fields
narrows harder than one over two.

**Equality on every covered field, or nothing.** A hash-backed composite
index over `(a, b)` does not serve `a = 1` alone; that needs a prefix
ordering and a range scan the structure cannot do. `supports_prefix_lookup`
returns false rather than a comment nobody checks, and
`a_predicate_pinning_only_part_of_the_key_does_not_use_it` proves the
planner respects it.

**A bug the tests caught:** on restart, index definitions are replayed
through `create_index_from`, which built a *single-field* `HashIndex` over a
field literally named `"country\0age"`. No record has that field, so the
index came back empty and every query through it returned nothing —
silently, because an empty index is a valid index. Reconstruction now
recognises the composite name.

## M37 — SQL front-end

A `SELECT` parser onto the IR: `WHERE` with the full boolean grammar and
correct precedence, comparisons, `IS [NOT] NULL`, `[NOT] IN`, `[NOT] LIKE`,
`GROUP BY` with `COUNT/SUM/MIN/MAX/AVG`, `ORDER BY ... ASC|DESC`, `LIMIT`,
and `[INNER|LEFT] JOIN ... ON`.

Built last on purpose, and the reason held up: it is a parser onto a *stable*
IR. Nothing in it reimplements an evaluation rule — three-valued logic,
`NULL` never equalling `NULL`, `LIKE` wildcards are all the IR's existing
behaviour, and the parser only maps syntax onto it. A qualified column
(`users.id`) is kept joined precisely because that is the string a joined
row already carries from `merge_joined_fields`, so there is no translation
layer that could disagree.

**It refuses far more than it accepts, by name.** `INSERT`/`UPDATE`/
`DELETE`/`CREATE`, a second `JOIN`, `GROUP BY` without an aggregate, and
trailing garbage all produce an error naming the problem with a byte offset.
A SQL front-end that silently does something *near* what was asked is worse
than one that declines: the caller cannot tell until the answer is wrong.
Arithmetic is parsed by the IR but deliberately not by this front-end —
`a + 1 > 2` needs precedence rules it does not yet have, and
half-implemented precedence is how a query quietly means something else.

`crates/adabt-engine/tests/sql.rs` proves the plans actually execute against
the real engine — including that a SQL `WHERE` picks the same `IndexLookup`
the builder API does, which is the check that keeps SQL a front-end rather
than a second engine.

---

## The double scan

M36 left one finding unexplained: full scans degraded *superlinearly* in row
count, and the obvious cause was refuted. Eight times the rows cost fifty-two
times the scan time, and a buffer pool sixty-four times larger changed nothing.
"CPU-bound somewhere above the storage layer" was as far as that went.

### Measuring instead of guessing, again

The previous note ended by naming a suspect: the per-row allocation cost, on the
theory that the fetch loop pays what M27 had removed from the filter loop. That
was a hypothesis, so it got a harness rather than a patch — `adabt-bench
fetch-profile`, which times the same records through each layer of the path and
reports the cumulative cost, so the *difference* between two rows is what that
layer costs:

```
50000 rows — nanoseconds per row, cumulative
layer                      ns/row       adds
--------------------------------------------
decode                        251        251
decompress+decode             274         23
heap get                      748        474
scan                         2403       1655
```

The suspect was wrong. `decompress`, which allocates and copies the entire
payload on every single read even with compression off, costs **23 ns** — one
percent of a scan. Decoding, the other candidate, is ten percent. **Sixty-nine
percent of the cost of a scan is above the store entirely.**

That is not a number a micro-optimization explains, and it pointed somewhere
specific.

### What was actually there

`Database::all_ids` — the executor's way of asking "which records exist" —
answered out of `LogicalStore::scan`:

```rust
fn all_ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
    Ok(self.store.scan(collection)?.into_iter().map(|(id, _)| id).collect())
}
```

`scan` reads and decodes every record in the collection. `.map(|(id, _)| id)`
throws every one of them away. `fetch_batches` then fetches and decodes each of
those same records again, one at a time.

**Every full scan read and decoded the entire collection twice**, and the first
pass produced nothing the second did not produce again. The ids it wanted were
sitting in the in-memory page directory the whole time, available without
touching a page at all.

### Why nothing caught it

The answers were never wrong. Right rows, right order, right count. The
differential rig compares *results* between two stores and two evaluators — it
is the project's strongest instrument and it is structurally blind to this,
because both sides return the same correct rows. Every existing test asserts
what a query returns; none asserts what it costs.

This is a new failure class for this project, and worth naming next to the
others: not "an optimization changed the answer" but "the answer was right and
was computed twice." A suite that only checks answers cannot see it, and the
cost of it grows with the collection.

### The fix, and the instrument that keeps it fixed

`LogicalStore::ids` — a defaulted trait method returning ids without reading
records. The default delegates to `scan`, so a store that genuinely cannot do
better is unchanged and honest about it; `HeapStore` overrides it to walk the
directory it already holds in memory. The contract is that its ids are exactly
`scan`'s, including the tombstone rule and the ordering, and a test asserts that
across deletes rather than trusting it.

A cost bug needs a cost test. The buffer pool already counted every page it
handed out, and every record read goes through it, so `hits + misses` counts
record reads exactly — deterministic, no wall clock, no threshold that drifts
with the machine. `Database::buffer_stats` exposes it (it existed on `HeapStore`
and was reachable from nowhere above — the same gap as `vacuum` and
`restore_from`, found the same way). The test asserts a scan of N records costs
fewer than 2N page reads. Reverting the fix scores exactly 4000 on 2000 records,
and the test fails, which is the only reason to believe it works.

### What it was worth

Same machine, same harness, same row counts:

| rows | scan before | scan after | faster |
|---:|---:|---:|---:|
| 100k | 242 ms | 255 ms | — |
| 200k | 1,203 ms | 719 ms | 1.7× |
| 400k | 3,644 ms | 1,461 ms | 2.5× |
| 800k | 12,687 ms | 2,862 ms | **4.4×** |

The speedup *grows with the collection*, which is the part that matters. Doing
the work twice is a constant factor on CPU but not on memory traffic: the
duplicate pass doubled the working set, and past the point where that stopped
fitting, the cost of it compounded.

**So M36's superlinearity finding is substantially retracted.** Scan cost from
100k to 800k rows was 52× for 8× the data; it is now 11×. Not linear, but
nothing like a wall. The remaining curve is worth one more look; the cliff that
made it look architectural was this.

Two of my own claims died here, and both died the same way. The buffer-pool
hypothesis was refuted by measuring it. The allocation hypothesis that replaced
it was refuted by measuring it — 23 ns, in the profile above, before a line of
it was written. The thing that found the real cause was not a better guess. It
was building the instrument first and letting it point.

### Still standing

The point-lookup finding is unaffected and still holds: 6.3 µs at 100k rows,
12.4 µs at 800k. An indexed lookup is not flat in collection size, and every
cost estimate in `adabt-opt` assumes it is.
