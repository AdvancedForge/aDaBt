# Roadmap

aDaBt is measured against four goals. Each has a finish line: a test that says
when the goal is met, as opposed to a list of features.

| goal | finish line | status |
|---|---|---|
| **Usable** | you could ship on it | Met, for a single node with sharding as the growth story |
| **Manually optimal** | the engine's ceiling is the hardware's ceiling | Met against SQLite; not yet measured against memory bandwidth or core count |
| **Automatically optimal** | an expert tuning by hand adds nothing | Not met: on the same operations for both sides (since 2026-11-15), adaptive matches on 3 of 5 soak phases at 5k rows (identity, aggregate, identity again; point filter 1.02×, range filter 1.31×). At 20k, identity matches; point filter is 1.14–1.16×, range filter 1.47–1.55× and aggregate 1.15–1.64× across two loaded runs. The plan cache is not a cause (it saves under 1%). Range is the direct array, which the default priorities price below the bar ([`benchmarks/expert-vs-adaptive.md`](benchmarks/expert-vs-adaptive.md)) |
| **Good** | there is a workload where it is demonstrably the right answer | Met: 4 of 8 workloads won against SQLite, losses published alongside |

"Met" means the finish line as defined here. It does not mean production-ready;
[`limitations.md`](limitations.md) is the authoritative list of gaps.

The story of how each goal was reached, stage by stage with the evidence for
each step, is in [`history/roadmap-stages.md`](history/roadmap-stages.md).

## What exists

- **Usable:** crash-safe storage (segmented WAL, torn-tail-safe recovery,
  checkpoints with log truncation), backup, restore and point-in-time recovery;
  snapshot-isolated, crash-atomic transactions, serializable under `Strict`
  (point reads and scans validated at commit); crash-convergent cross-shard commits; a format version gate with enumerated
  migrations; a TCP server with TLS, bearer tokens, roles and per-collection
  access floors; joins with spill; a `SELECT` front-end; per-query memory
  budgets and cancellation.
- **Manually optimal:** hash, B-tree, bitmap, composite, covering and partial
  indexes; column stores with dictionary and delta encoding and columnar top-K;
  materialized views; record compression; direct addressing for fixed schemas;
  compiled hot paths; clustered placement; zero-copy projection; a cost model
  calibrated by measurement.
- **Automatically optimal:** 15 registered optimizations, each reachable from
  the level ladder; an adaptive driver that proposes from telemetry, proves each
  change in a shadow trial before ramping it into traffic, re-scores settled
  decisions every cycle and retracts what stops paying.
- **Good:** a published comparison against SQLite, a soak test against a level-0
  reference, a crash matrix, a hostile-client suite, an invariant checker
  (`Database::verify`), a C ABI, a shell and runnable examples.

# What's next

Everything below is open, ordered by the same rule the project has used
throughout: evidence first, then what it unblocks.

### 1. Close the unmeasured finish tests (evidence, no new features)
- **Core scaling and memory bandwidth:** throughput against core count with
  `--shards 1` as the control, and scan throughput against measured memory
  bandwidth, with the gap stated.
- **Cost-model accuracy:** predicted against actual, within noise, at 100k and
  1M rows in the level matrix.
- **Bitmap at high cardinality:** the missing `index-scale` measurement before
  the hash/bitmap preference can be called settled.
- **Record compression on realistic records:** stored bytes and insert/scan
  throughput with it on and off.

### 2. More witnesses
- Vendor the PostgreSQL and RocksDB drivers so `--witness postgres|rocksdb`
  stops exiting with status 2; add YCSB- and TPC-C-shaped workloads.

### 3. Attack the published losses
- **Adaptive behind the expert** ([`benchmarks/expert-vs-adaptive.md`](benchmarks/expert-vs-adaptive.md)):
  - Fixed in the 2026-11-14 re-run: the result cache is admitted on its
    measured hit rate (point filters matched at both sizes; the second
    re-run measured 1.12× at 20k on a noisy cell), direct lookup
    is chosen at 20k rows, and structures from ended phases are retracted.
  - Fixed in the second 2026-11-14 re-run: record compression's estimate
    is sized from the stored records' uncompressed bytes and measured
    compression ratio, so it is no longer enabled where records do not
    compress. That did not close the 20k identity gaps.
  - Fixed in the third re-run: each optimization cycle read every live
    record twice; it now reads a bounded sample. At 20k, range filter went
    from 1.74× to 1.23× and aggregate from 1.99× to 1.12×.
  - Fixed in the fourth re-run: the 20k identity gaps (1.33–1.52×) were
    not a per-lookup cost. Each cycle scanned every record to find the
    highest id, taking ~4 ms against ~100 µs of lookups between cycles, and
    the lookups after it ran on flushed caches. The id now comes off the
    page directory. Identity went to 1.03× (matches) and identity again to
    1.12×.
  - **Identity again** (1.12× at 20k, 1 ns outside the expert's range at
    5k). A cycle still costs ~0.33 ms, mostly walks of the page directory
    for live counts and samples. Next step: O(1) live counts.
  - **Range filter** (1.62× at 5k, 1.65× at 20k) and **aggregate** (1.26×
    and 1.71×). Adaptive's own figures held steady while the expert's
    improved on a quieter machine. The expert wins with the level 11
    preset on range and with column store plus views on aggregate, and
    adaptive chooses neither. The plan cache is still a workload-blind 5%
    prior.

  - 2026-11-15 pinned 20k re-run after O(1) live counts: identity 1.03×,
    identity again 1.09×, point filter 1.11×, range filter 1.48×,
    aggregate 1.72×. The plan cache is in the expert's configuration for
    all three of the last gaps and adaptive never enables it: estimate it
    from measured planning time and plan-shape repetition next.

  - 2026-11-15 sixth re-run. The plan cache is estimated from measured
    planning time per shape and a ghost cache. It saves under 1%
    (planning 0.4–2 µs, misses 0.4–8 ms) and stays off, correctly. The
    expert now replays the soak's own operations: its windows had had more
    result-cache hits than adaptive's (87% against 75% on aggregate). At
    5k, aggregate went from 1.26× to 0.99× (matches). What is left:
    - **Range filter** (1.31× at 5k, 1.47–1.55× at 20k): the direct array
      is all of level 11's advantage (525 → 405 µs). Each miss fetches
      ~1,435 records by id to return 50, because `Limit` is not pushed into
      the index range. The direct array's estimate now counts index-served
      fetches. At the default priorities it scores ~0.2 against the 0.5 bar
      (freedom −2.0, resources −0.9), so adaptive declines it. Next: push
      the limit into the index range, which would help every configuration.
    - **Aggregate** (1.15–1.64× at 20k): the medians are result-cache hits
      whose cost varied by more between the two runs than between
      configurations. The column store would make misses 2.4× faster
      (p99), but the planner reads `age >= ?` through the b-tree when both
      exist. Next: the planner's choice between a wide index range and a
      column scan, and running the soak's level-0 reference outside the
      measured window.
    - **Point filter** (1.02× at 5k, 1.14–1.16× at 20k): adaptive's
      configuration measured fresh is ~200 ns faster than in the soak,
      so this is soak state or harness, not a choice.

  Re-run `adabt-bench expert-vs-adaptive` after any change to the driver's
  scoring. Matching on every phase is the finish line.
- **Writes:** bulk load is 1.5–2.0× behind SQLite; single-row inserts are now
  2.3–2.5× ahead. Both were lost mostly to placement: the free-space map
  scanned every full page on every insert. With that fixed, and the per-row
  copies, allocations and page-path costs around the log removed, a loaded
  row costs ~600 ns in the engine (details in
  `benchmarks/comparison-notes.md`). Per-record MVCC and the WAL stay. What
  is left, largest first: per-row derived-structure lookups in the engine
  (~140 ns), the log frame's FNV-1a checksum (~100 ns; changing it is a log
  format change), and the free-space and directory updates (~185 ns).
- **Indexed equality** (~1.4–1.5× behind; indexed range is now at parity). Profiling showed the cost was cache misses and allocator
  traffic, not record construction as such; shared field names, a hashed
  projection map, an in-place `Project`, and filtering and projecting the
  covering index's stored rows before copying them cut it (details in
  `benchmarks/comparison-notes.md`). What is left: the compiled
  predicate's per-row stack allocation was measured and is only ~5% of its
  cost; the rest is resolving each field by name in every record, so a
  per-schema field-position cache is the lever there. Routing covering lookups
  through `fetch_projected_batches` would not help: there is no fetch left.

### 4. Correctness gaps a user will hit
- Point-in-time recovery timelines: a restored database continues its log at
  the target, so it needs an archive directory of its own. Superseded
  history is kept in `superseded-log/` but there is no tooling for it.
- `loom` model checks for `adabt-engine`, which does not yet compile with the
  feature (loom's `Arc` meets std's at crate boundaries).
- Writes and DDL through SQL.
- A C ABI wider than i64 put/get.

### 5. Join ordering
- Two optimizations, `join_order` and `data_partitioning`, were registered
  while doing nothing (a flag no planner read; a duplicate of
  `clustered_sort`) and have been removed. Real cardinality-driven join
  ordering is open, with one constraint: the hash join builds on the right
  side so that output order never depends on the algorithm, so reordering
  must preserve that.

### 6. Toward beta
- Hosted CI running `scripts/check.sh`, an MSRV check and the soak test. The
  workflow is written (local branch `ci-workflow`); publishing it needs a token
  with the `workflow` scope.
- Freeze `LogicalStore` and the wire protocol, and extend
  [`semver.md`](semver.md) from the disk format to the API.

## Out of scope

Unless a revisit trigger fires: replication and HA
([`replication-decision.md`](replication-decision.md)), out-of-core datasets
([`benchmarks/scale-decision.md`](benchmarks/scale-decision.md)), and hiding
the cross-shard visibility window (which needs distributed locking).
