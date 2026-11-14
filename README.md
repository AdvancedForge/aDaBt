# aDaBt

![msrv](https://img.shields.io/badge/rust-1.88%2B-blue)
![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)

**A database that rebuilds its own internals around the workload it is given.**

The interface stays fixed: collections, records, logical queries. Underneath,
the engine watches what the traffic actually does and specialises its physical
layout to match: indexes, column stores, materialized views, compression,
direct addressing, compiled hot paths. Every change is proved on live traffic
before it serves a query, and none of them may change an answer. The same
machinery can be driven by hand, one level or one optimization at a time.

> **Status:** `0.1.0-alpha.1`, an experimental alpha. Single node, RAM-resident,
> not production-ready. [`docs/limitations.md`](docs/limitations.md) lists
> exactly what it does not do.

## See it

```sh
cargo run --release -p adabt-engine --example watch_it_optimize
```

```text
before optimize(): 3 groups in 63.648328ms
Aggregate(by [region], count(*))
  HeapScan(events)
rationale: no applicable index; full scan

optimize() — propose, prove, promote…
applied: ["column_store[global]", "delta_encoding[global]", "materialized_view[global]", ...]

after optimize():  3 groups in 6.736µs
...
faster by 9449.0x — with identical rows.
```

Same query, same 50,000 records, no configuration. The engine saw a repeated
grouped count, proposed a column store and a maintained view, proved them, and
promoted them.

## How it works

```
 telemetry ──► propose ──► build hidden ──► shadow ──► ramp 1→10→50→90% ──► keep
 (per query      (15 opt-     (planner       (both paths    (any divergence     (re-scored
  shape)         imizations)   can't see it)  answer, must   or guardrail        every cycle;
                                              agree)         breach reverts)     retracted when
                                                                                 it stops paying)
```

- **One control path.** Manual and adaptive selection are two implementations
  of `OptimizationDriver` feeding one controller. Every decision, by a person
  or by the engine, is logged with its reason.
- **Derived representations are rebuildable.** Indexes, caches, column stores
  and direct arrays can all be reconstructed from the primary heap, so any of
  them can be dropped at any moment. Changes that would rewrite the primary are
  refused for automatic trial, since after one there is no old path to compare
  against.
- **Guarantees filter; priorities score.** A policy says what must hold
  (`durability: strict`) and what to prefer (speed, resources, freedom).
  Guarantees remove options outright rather than making them expensive.
- **The resource axis points both ways.** Most optimizations spend memory to
  buy latency; compression and delta encoding trade the other way, so a
  resource-first policy has real choices.
- **Evidence outranks estimate.** Each optimization starts from a prior, which
  measurement replaces. A claimed win that shadow execution refutes stops
  arguing for itself. Irreversible changes are never made automatically.
- **Schemas can tighten from evidence.** `Dynamic → Declared → Strict → Fixed`
  is a per-collection dial. A collection that settles into a shape can become
  directly addressable without its API changing.

[`docs/architecture.md`](docs/architecture.md) covers the crates and the
optimization level ladder.

## Does it work?

**It never changes an answer.** A differential runner replays random operation
sequences against a reference model at every optimization level, and every
query is checked at every level before and after each specialisation. The soak
test drives the adaptive engine through a workload that changes underneath it
while a level-0 database answers the same queries; any divergence fails it.
Crash tests truncate the write-ahead log at a matrix of byte offsets
through a workload and require exact recovery, and `Database::verify()` checks
every derived structure against the heap.

**It gets faster on its own.** From `adabt-bench soak`, with five workloads back
to back, 123,801 queries, adaptive mode and nothing configured:

| phase | p50 at start | p50 at end | |
|---|---:|---:|---:|
| identity lookups | 1,099 ns | 436 ns | **−60%** |
| point filters | 7,079 µs | 1,069 µs | **−85%** |
| range filters | 12,373 µs | 490 µs | **−96%** |
| grouped aggregates | 1,824 µs | 1,850 µs | flat |

The flat last row is a historical measurement. It is what led the driver to
re-score settled decisions every cycle; tuned group-by now beats SQLite by four
orders of magnitude.

**Individual optimizations,** each isolated with `--disable`:

| | |
|---|---|
| `auto_index` on an equality filter | **10.5×** |
| Column store on aggregates | **9.6×** |
| Materialized view vs recomputing the aggregate | **>10×** |
| Compiled path vs general query path | **3.0×** (p50 1,280 ns → 432 ns) |
| Single field from a computed address | **14×** (p50 1,280 ns → 92 ns) |
| Sequential scan with read-ahead | **>8×** fewer page reads |
| Record compression, stored bytes | **>2×** smaller |
| Strict vs relaxed durability, writes | **5,900×**, which is why guarantees filter rather than score |

**Against SQLite** ([`comparison/`](comparison), workloads chosen to include
ones aDaBt should lose), aDaBt wins 5 of 8:

| aDaBt wins | aDaBt loses |
|---|---|
| point lookup by id: **1.9×** | bulk load: **~11× slower** (per-record MVCC + WAL) |
| full-scan count, tuned: **~10⁴×** | single-row inserts |
| group-by count, tuned: **~10⁴×** | indexed equality: **~1.6× slower** |
| top-20 sort, tuned: **2.8×** | |
| indexed range, tuned: **1.3–2.9×** (noisy) | |

Full numbers and caveats are in
[`docs/benchmarks/comparison-notes.md`](docs/benchmarks/comparison-notes.md).

## Use it

```rust
let mut db = Database::open(&dir, Policy::manual(0))?;
db.create_collection("users", Schema::dynamic())?;
db.insert_batch("users", records)?;

let adults_in_norway = LogicalPlan::new(
    LogicalOp::scan("users")
        .filter(Expr::And(vec![
            Expr::eq("country", "NO"),
            Expr::cmp("age", CmpOp::Ge, 18i64),
        ]))
        .project(vec!["age".into()]),
);
let rows = db.query(&adults_in_norway)?;
println!("{}", db.explain(&adults_in_norway)); // the access path, and why
```

There is also a server (`adabt-server`, with TLS and token auth), a `SELECT`
shell (`adabt-cli`) and a C ABI (`adabt-ffi`). See
[`docs/getting-started.md`](docs/getting-started.md).

## Workspace

| Crate | Role |
|---|---|
| `adabt-core` | Logical vocabulary: `Value`, `Record`, `Schema`, ids, `Policy`, the `LogicalStore` trait |
| `adabt-ir` | Logical plans, expressions, query shapes; pure |
| `adabt-telemetry` | Per-shape statistics, log-linear histograms, decaying temperature sketch |
| `adabt-storage` | Slotted pages, buffer pool, segmented WAL, recovery, codecs, version chains, heap |
| `adabt-index` | Hash, B-tree and bitmap secondary indexes |
| `adabt-exec` | Planner, calibrated cost model, batched executor |
| `adabt-opt` | Optimization registry, levels, controller, adaptive driver, experiments; depends on nothing physical |
| `adabt-engine` | `Database` and `ShardedDatabase`: the facade over everything above |
| `adabt-server` | TCP server, binary protocol, client |
| `adabt-cli` | SQL `SELECT` shell with `.explain` |
| `adabt-ffi` | C ABI and header |
| `adabt-testkit` | Reference model, deterministic generators, differential runner |
| `adabt-bench` | Workloads and measurement harnesses |

Each crate depends only on crates above it in this table (`adabt-testkit` is a
dev-dependency only). `adabt-core` depends on nothing in the workspace.

## Build and test

Requires a C linker and Rust 1.88+.

```sh
bash scripts/check.sh   # the gate: fmt, clippy, tests, loom, docs
```

Benchmarks:

```sh
cargo build --profile bench-fast -p adabt-bench
B=target/bench-fast/adabt-bench
$B soak         --data-dir /var/tmp/soak --size 5000 --ops-per-phase 25000 --log
$B matrix       --engine engine --levels 0,1,2,3,10
$B query-matrix --levels 0,1,2,4,10 --disable result_cache
cargo run --manifest-path comparison/Cargo.toml --release   # vs SQLite
```

Use a real disk for data directories. On tmpfs `fsync` never reaches storage and
durability numbers mean nothing; the harness warns about this.

## Documentation

| | |
|---|---|
| [`docs/getting-started.md`](docs/getting-started.md) | Runnable examples, server quickstart |
| [`docs/architecture.md`](docs/architecture.md) | Crates, the level ladder, key design choices |
| [`docs/api.md`](docs/api.md) | Engine, sharded and wire surfaces |
| [`docs/operations.md`](docs/operations.md) | Scale, backup and PITR, tuning |
| [`docs/limitations.md`](docs/limitations.md) | What it does not do: **the authoritative list** |
| [`docs/roadmap.md`](docs/roadmap.md) | What was finished, and what comes next |
| [`docs/semver.md`](docs/semver.md) | The on-disk compatibility promise |
| [`docs/benchmarks/`](docs/benchmarks) | Measurements and methodology |
| [`docs/history/`](docs/history) | Milestone notes, kept as written at the time |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
