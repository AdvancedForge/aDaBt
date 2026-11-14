# Roadmap

aDaBt is measured against four goals. Each has a finish line: a test that says
when the goal is met, as opposed to a list of features.

| goal | finish line | status |
|---|---|---|
| **Usable** | you could ship on it | Met, for a single node with sharding as the growth story |
| **Manually optimal** | the engine's ceiling is the hardware's ceiling | Met against SQLite; not yet measured against memory bandwidth or core count |
| **Automatically optimal** | an expert tuning by hand adds nothing | Not met: adaptive matches the expert on 1 of 5 soak phases at 5k rows and 0 of 5 at 20k, mostly because it enables the result cache late or never ([`benchmarks/expert-vs-adaptive.md`](benchmarks/expert-vs-adaptive.md)) |
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
  - The driver admits the result cache late or never. Its fixed 1 MiB
    reservation costs the whole resource budget on a small database, and its
    speed estimate ignores how often queries repeat. That leaves point filters
    about 1,000× behind and, at 5k rows, aggregates too.
  - At 20k rows the identity phase ends without direct lookup (2.65× behind).
  - Structures from ended phases are not retracted when they cost no
    maintenance (identity-again, 1.2–1.8× behind).

  Re-run `adabt-bench expert-vs-adaptive` after any change to the driver's
  scoring. Matching on every phase is the finish line.
- **Writes:** bulk load is ~11× and single-row inserts are behind SQLite.
  Per-record MVCC and WAL stay; batching, group commit and WAL-frame cost are
  the levers.
- **Indexed equality** (~1.6× behind; indexed range is now at parity). Profiling showed the cost was cache misses and allocator
  traffic, not record construction as such; shared field names, a hashed
  projection map and an in-place `Project` cut it (details in
  `benchmarks/comparison-notes.md`). What is left: the covering source clones
  every needed field — including the filtered one the plan then drops — where
  it could filter and project the borrowed stored row. The compiled
  predicate's per-row stack allocation was measured and is only ~5% of its
  cost; the rest is resolving each field by name in every record, so a
  per-schema field-position cache is the lever there. Routing covering lookups
  through `fetch_projected_batches` would not help: there is no fetch left.

### 4. Correctness gaps a user will hit
- Point-in-time restore from a base image: `open_at` over a heap file that
  already holds pages written after the target can show those writes.
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
