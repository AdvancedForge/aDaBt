# Architecture

Single-node database, embedded or served over TCP. Source in `crates/`; the
README's layout table has a one-line role for each crate.

## Crates, bottom to top

- `adabt-core`: logical vocabulary — `Value`, `Record`, `Schema`, ids, `Policy`, and `LogicalStore`, the only stable interface (`docs/api.md`)
- `adabt-ir`: logical plans, expressions, `QueryShape`/`QueryKey`; pure
- `adabt-telemetry`: per-shape stats, histograms, temperature sketch — the evidence the optimizer reads
- `adabt-storage`: slotted pages, buffer pool, WAL, recovery, codecs, compression, version chains, heap, catalog (v4)
- `adabt-index`: hash, B-tree and bitmap secondary indexes; composite, covering and partial variants
- `adabt-exec`: batched operators, planner, calibrated cost model; projection-aware filtering (`filter_by_peek_fields`, 1–4 fields), `ExecBudget`
- `adabt-opt`: optimizations, registry, level ladder, controller, decision log, adaptive driver, experiments; depends on nothing physical
- `adabt-engine`: `Database` (heap + indexes + caches + column store + direct arrays + compiled paths + materialized views + experiment runner) and `ShardedDatabase`
- `adabt-server`: TCP listener, binary protocol, bearer tokens, roles, per-collection floors, TLS
- `adabt-cli`: SQL `SELECT` shell with `.explain`
- `adabt-ffi`: C header and ABI
- `adabt-testkit`, `adabt-bench`: reference model and differential runner; workloads and harnesses

## The level ladder

15 registered optimizations, every one reachable from some level
(`crates/adabt-opt/src/levels.rs`, `MAX_LEVEL = 11`). Levels are cumulative
presets, not states — nothing in the engine branches on a level:

| level | meaning |
|---|---|
| 0 | conventional |
| 1 | basic: plan and result caching, statistics |
| 2 | automatic physical: index selection, compression |
| 3 | aggressive execution: prefetching, larger caches |
| 4 | physical layout: column stores, materialized views, delta encoding |
| 5 | workload-aware: workload-specific, covering indexes, clustered sort |
| 6–9 | increasing specialisation (8: schema freeze; 9: thread-per-core) |
| 10 | direct addressing |
| 11 | a workload-specific data appliance |

`docs/level-matrix.md` has measurements per level.

Manual (`Policy::manual(level)`) and adaptive (`Mode::Adaptive`) selection are
two drivers feeding one controller; every decision is logged. New derived
structures go through a shadow trial and staged rollout before serving traffic.

## Key design choices
- Derived representations are rebuildable from the primary; optimizations never change answers
- Zero-copy multi-field projection (`peek_fields`)
- Catalog v4 (`FORMAT_VERSION`): delta encoding and thread-per-core flags persisted
- Cross-shard writes serialized via `Arc<Mutex<()>>` (`commit_coordinated`), coordinator-decides, not 2PC
- `Strict` consistency validates point-read sets (not full serializable; predicate phantoms possible)
- Memory-resident: no disk overflow planned (`docs/benchmarks/scale-decision.md`)

Gaps are listed in `docs/limitations.md`.
