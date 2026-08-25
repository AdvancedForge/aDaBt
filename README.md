# aDaBt

A database whose **logical interface stays fixed while its physical
implementation ranges from completely conventional to radically specialized** —
and where the choice of specialization can be made by a human or by the database
itself, through the same mechanism.

Status: **M0–M37 complete**, plus the roadmap stages: the comparison is
published (4 of 8 wins vs SQLite, losses included), covering-index selection
proposes both shapes from traffic, the scale contract is decided, settled
decisions are re-examined every cycle, and `adabt-cli` gives the SQL surface
a shell. 1,100 tests, 56,500+ lines. Thread-per-core and cross-shard
transactions are the largest things still unbuilt.

Start with **`docs/roadmap.md`** — where the four tracks stand now and the
ordered plan to their finish lines. `docs/diagnosis.md` is the original
post-M15 accounting that the roadmap scores against; milestone notes and
measurements are in `docs/`.

## Layout

| Crate | Role |
|---|---|
| `adabt-core` | Logical vocabulary: `Value`, `Record`, `Schema`, ids, `Policy`, `LogicalStore`. No physical code. |
| `adabt-ir` | Logical plans, expressions, `QueryShape` / `QueryKey`. Pure. |
| `adabt-telemetry` | Sharded probes, per-shape stats, log-linear histograms, decaying temperature sketch. |
| `adabt-storage` | Slotted pages, buffer pool, WAL, recovery, codecs, compression, version chains, heap. |
| `adabt-index` | Hash and B-tree secondary indexes. |
| `adabt-exec` | Batched operators, planner, shape-invariant access decisions. |
| `adabt-opt` | `Optimization`, registry, levels, controller, decision log, scoring, calibrated cost model, adaptive driver, experiments. **Depends on nothing physical.** |
| `adabt-engine` | `Database`: heap + indexes + caches + column store + direct arrays + compiled paths + schema inference + materialized views + the live experiment runner. |
| `adabt-testkit` | Reference model, deterministic generator, differential runner. |
| `adabt-bench` | Workloads and measurement harnesses. |
| `adabt-server` | TCP listener, binary protocol, blocking client. No lock around the engine — the shards hold their own. |
| `adabt-cli` | The SQL shell: open a directory, SELECT through the M37 parser, `.explain` plans. Thin on purpose — one evaluation path. |

## What the measurements say

Every number below is reproducible from `adabt-bench`; the caveats are in `docs/`.

| | |
|---|---|
| Strict vs relaxed durability, writes | **5,900×** — which is why guarantees filter rather than score |
| `auto_index` on an equality filter | **10.5×** |
| Column store on aggregates | **9.6×** |
| Record compression, stored bytes | **>2× smaller** |
| Compiled path vs general query path | **3.0×** (p50 1280ns → 432ns) |
| Single field from a computed address | **14×** (p50 1280ns → 92ns) |
| Materialized view vs recomputing the aggregate | **>10×** — groups instead of rows |
| Sequential scan with read-ahead | **>8×** fewer reads (160 pages, 160 → under 20) |
| Opening a database, both caches vs neither | **2.4×** (1,854 → 768 ms at 200k records) |

And what it does unaided, from `adabt-bench soak` — five workloads in sequence,
123,801 queries, adaptive mode, nothing configured:

| phase | p50 at start | p50 at end | |
|---|---|---|---|
| identity lookups | 1,099 ns | 436 ns | **−60%** |
| point filters | 7,079 µs | 1,069 µs | **−85%** |
| range filters | 12,373 µs | 490 µs | **−96%** |
| grouped aggregates | 1,824 µs | 1,850 µs | −1% |

The last row is a historical measurement, and it earned its keep: it stood,
reproducible, until it forced the driver to re-examine settled decisions —
every cycle now re-scores what is enabled under the calibrated cost model
(`docs/roadmap.md`, Track C). Against the outside witness the aggregate
phase now tells the opposite story: tuned group-by beats SQLite by four
orders of magnitude (`docs/comparison-notes.md`).

## Six ideas that carry the design

**The schema-mode spectrum.** `Dynamic → Declared → Strict → Fixed` is a
declared, per-collection dial, and the database can *move along it* from
evidence. A collection that started schemaless and settled into a shape becomes
directly addressable without its API changing.

**Derived representations are rebuildable.** Indexes, caches, column stores and
direct arrays are all reconstructible from the primary. Adding one cannot lose
data, rollback is a drop, and divergence is always a bug — never reconcilable.

**Guarantees filter; priorities score.** `durability: strict` makes
async-durability techniques *invisible*, not merely expensive.

**The resource axis points both ways.** Most optimizations spend memory to buy
latency; compression trades the other way, so a `resources`-priority policy has
something to select. A test enforces that at least one such optimization exists.

**One control path.** Manual and adaptive selection are two implementations of
`OptimizationDriver` feeding one `OptimizationController`. Every decision is
logged, including manual ones.

**Evidence outranks estimate.** A prior claiming a 70% win that measurement
refutes stops arguing for itself. Irreversible changes are never made
automatically, because every safety mechanism assumes a decision can be undone.

## Optimization never changes answers

Enforced, not asserted. The differential runner replays random operation
sequences against a reference model at every optimization level; separate tests
run every query at every level, before and after every specialisation, and
demand identical results.

## Build

Requires a C linker and Rust 1.82+.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Artefacts go to `~/.cargo-target/adabt` — the source lives on a 9p-mounted
Windows drive where Cargo's I/O is 5–15× slower.

## Benchmarks

```sh
cargo build --profile bench-fast -p adabt-bench
B=~/.cargo-target/adabt/bench-fast/adabt-bench

$B matrix       --engine engine --levels 0,1,2,3,10
$B query-matrix --levels 0,1,2,4,10 --disable result_cache
$B compiled     --size 50000 --ops 20000
```

`--disable` forces an optimization off whatever the level says, so a benchmark
can isolate one from another that would mask it. The harness warns when its data
directory is memory-backed: `fsync` on tmpfs never reaches a disk, and an early
version of these numbers was wrong because of it.

`comparison/` (a separate crate, excluded from the workspace) runs aDaBt against
SQLite on workloads chosen to include the ones aDaBt should lose. The numbers
are published in `docs/comparison-notes.md`: at its best configuration aDaBt
wins **4 of 8** — point lookups, post-tuning count/group-by at four orders of
magnitude through its column store and materialized views, and top-20 sort at
2–3× over SQLite by selecting k winners from raw columnar cells instead of
sorting the collection. The indexed shapes answer through self-proposed
covering indexes (`auto_covering_index`: hash-backed for equality evidence,
b-tree-backed for ranges) and sit at 1.5–2.4×; what remains is projection
fetch cost, not structure choice. It loses bulk load and single-row inserts —
the price of per-record MVCC and WAL, which it will not trade away.

## Watching it work

```sh
$B soak --data-dir /var/tmp/soak --size 5000 --ops-per-phase 25000 --log
```

Runs the adaptive engine against a workload that changes underneath it, with a
second database pinned at level 0 taking the same queries — any difference in
results stops the run. It found four defects in its first sitting, all of them
interactions between components that each passed their own tests, and it is the
only test here that can see that class of bug. `docs/m15-notes.md` has them.

## Proving a change before trusting it

An optimization that adds a derived representation is not switched on; it is put
on trial. The structure is built where the planner cannot see it, both paths
answer the same queries against the same state until the results are known to
agree, and only then does traffic move — 1%, 10%, 50%, 90% — with any divergence
or guardrail breach reverting it. `optimize_verified` is the entry point;
`docs/m14-notes.md` explains what shadow proves that canary cannot, and why.

Changes that rewrite the primary are refused for the trial with a reason, because
after one there is no old path left to compare against.

## What is not built

No replication. No cross-shard transactions — the log format records
participants and coordinator, but no coordinator exists. Serializable isolation
is not yet a selectable level. No authentication, TLS or roles: the server is
trusted-network-only. Shared-nothing partitioning exists; **thread-per-core
does not** — no core pinning, no `io_uring`, no async storage path. Every index
and the page directory are fully resident, measured at roughly 470–570 bytes of
resident memory per row, which puts the practical ceiling near a few million
rows on ordinary hardware. `--shards 1` is the unpartitioned behaviour exactly,
which is the honest way to measure what partitioning is worth.

Joins (hash and indexed nested loop, with spill), multi-statement transactions
with single-shard snapshot isolation, a SQL `SELECT` front-end, segmented WAL
with log truncation at checkpoint, and backup/restore/PITR have all landed
since this file was last revised. The milestone notes in `docs/` are the
record.

`SUM` is materialized only while integer arithmetic stays exact, aggregates are
never combined across shards, and `MIN`/`MAX` are not maintained at all. Each is
a place where a faster implementation was available and was rejected because it
would have moved an answer in the last decimal place.

`docs/roadmap.md` is the full accounting, including what remains for each track.
