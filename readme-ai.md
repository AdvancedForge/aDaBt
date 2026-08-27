# aDaBt

**Status:** `0.1.0-alpha.1` — experimental public alpha, not production-ready. See `docs/limitations.md`.

A database whose **logical interface stays fixed while its physical
implementation ranges from completely conventional to radically specialized** —
and where the choice of specialization can be made by a human or by the database
itself, through the same mechanism.

Roadmap: all four tracks reach their defined 100% finish lines (`docs/roadmap.md`); replication is permanently out of scope. See `docs/getting-started.md` for the quickstart and `docs/limitations.md` for what is not promised.

Start with **`docs/getting-started.md`** — embedded quickstart, server quickstart with correct flags (`--data`, `--listen`, `--tls-cert`, `--tls-key`), and the adaptive optimization demonstration. `docs/architecture.md` explains the design. `docs/limitations.md` is the honest accounting of what is not promised. Milestone measurements are in `docs/benchmarks/` and `docs/history/`.

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
orders of magnitude (`docs/benchmarks/comparison-notes.md`).

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

Artefacts go to the workspace `target/` directory (default Cargo behavior).

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

`comparison/` (a **separate workspace**, excluded from the root) runs aDaBt against
SQLite on workloads chosen to include the ones aDaBt should lose. Invoke as
`cargo run --manifest-path comparison/Cargo.toml --release` (not `cargo run -p adabt-comparison` from root, which does not resolve). The numbers
are published in `docs/benchmarks/comparison-notes.md`: at its best configuration aDaBt
wins **4 of 8** — point lookups, post-tuning count/group-by at four orders of
magnitude through its column store and materialized views, and top-20 sort at
2–3× over SQLite by selecting k winners from raw columnar cells instead of
sorting the collection. The indexed shapes answer through self-proposed
covering indexes (`auto_covering_index`: hash-backed for equality evidence,
b-tree-backed for ranges) and sit at 1.5–2.4×; what remains is projection
fetch cost, not structure choice. `docs/benchmarks/` also notes that `--witness postgres|rocksdb` is planned — `comparison/src/main.rs:8` fail-fasts (exit 2) when `DATABASE_URL`/`rocksdb` driver unavailable rather than pretending SQLite numbers are those DBs. It loses bulk load and single-row inserts —
the price of per-record MVCC and WAL, which it will not trade away.

## Watching it work

```sh
$B soak --data-dir /var/tmp/soak --size 5000 --ops-per-phase 25000 --log
```

Runs the adaptive engine against a workload that changes underneath it, with a
second database pinned at level 0 taking the same queries — any difference in
results stops the run. It found four defects in its first sitting, all of them
interactions between components that each passed their own tests, and it is the
only test here that can see that class of bug. `docs/history/m15-notes.md` has them.

## Proving a change before trusting it

An optimization that adds a derived representation is not switched on; it is put
on trial. The structure is built where the planner cannot see it, both paths
answer the same queries against the same state until the results are known to
agree, and only then does traffic move — 1%, 10%, 50%, 90% — with any divergence
or guardrail breach reverting it. `optimize_verified` is the entry point;
`docs/history/m14-notes.md` explains what shadow proves that canary cannot, and why.

Changes that rewrite the primary are refused for the trial with a reason, because
after one there is no old path left to compare against.

## What is not built

No replication — explicitly out of scope (`docs/history/replication-decision.md`). Everything else on the roadmap is landed: cross-shard coordinator-decides durability (`ShardedDatabase::commit_coordinated`, `XSH1` journal, torn-tail safe, windowed visibility), strict read-set validation (`Consistency::Strict`, predicate phantoms remain documented in `docs/limitations.md`), TLS (`--tls-cert`/`--tls-key`), roles + per-collection `Forbidden` floors, best-effort core-pinned shard execution (`core_affinity` burst pinning in broadcast, per-shard `BufferPool` = per-core memory, persistent workers rejected by `connection_scale` gate), compound reasoning (`join_order` + `data_partitioning` level 6). The page directory and every index remain fully resident by design (~470 B/row, `docs/benchmarks/scale-decision.md`), so the practical ceiling is a few million rows per shard — sharding is the growth story.

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
