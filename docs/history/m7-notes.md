# M7: widening the optimization library

Built because the M6 matrix showed the adaptive driver would have had almost
nothing to choose between: five optimizations, of which one dominated every
workload tested and another was redundant on top of it.

## The hole this closed

Before M7, every registered optimization had a **negative** resources effect:

```
plan_cache     (+2 speed, -1 resources)
result_cache   (+5 speed, -4 resources)
buffer_pool    (+4 speed, -6 resources)
auto_index     (+7 speed, -3 resources)
direct_lookup  (+9 speed, -7 resources, -4 freedom)
```

All of them spend memory to buy latency. A policy of `resources: 10, speed: 3`
had literally nothing to select, so a third of the three-axis premise was
structurally untestable rather than merely untested.

`record_compression` (+6 resources, -1 speed) is the first optimization trading
the other way. `optimizations::resource_axis_tests` now asserts that at least
one exists, so the hole cannot silently reopen.

## What was added

| Optimization | Level | Effect |
|---|---|---|
| `record_compression` | 2 | LZ4 per record. Halves stored bytes on padded fixed-layout schemas, and uses fewer pages for the same data. |
| `column_store` | 4 | Columnar derived copy with dictionary-encoded text; aggregates push down into it. |

Also: a decaying count-min sketch for data temperature, per-`QueryShape`
telemetry, and a sharded probe replacing the single mutex.

## Compression is per record, not per page

Slots are already variable-length, so a shorter record simply takes less of one.
Compressing whole pages would need an indirection table, because pages are
addressed arithmetically.

Per-record also makes the choice per record: the encoding is a byte in the slot
prefix, so a block that does not shrink is stored raw and compression can never
make a record bigger. Compressed and raw records coexist, which means enabling
compression needs no migration and disabling it needs no rewrite.

## The column store measurement, and the mistake in it

First implementation: `ColumnStore::project` rebuilt a `Record` per row and
handed rows to the executor. Measured against a heap scan on the `aggregate`
workload, with the result cache disabled to isolate it:

```
aggregate  level 1:  30 q/s
aggregate  level 4:  37 q/s     (+23%)
```

23% is a bad return for a second copy of the data. The cause was the
implementation, not the idea: building N records dominated reading two columns,
so the layout was columnar but the *use* was rowwise.

After pushing the whole aggregate into the column store — grouping key and
aggregated value read straight from their columns, one allocation per group
instead of one per row:

```
aggregate  level 1:  31 q/s
aggregate  level 4: 292 q/s     (9.4x, p50 31.5ms -> 3.4ms)
```

The 23% version would have passed every correctness test and looked like a
working column store.

## Two harness bugs found while measuring

**Derived memory omitted the column store.** `derived_memory_bytes` summed
indexes, the result cache and direct arrays, but not columns, so levels 2 and 4
reported identical memory. A resource axis that under-reports is worse than none.

**The aggregate workload issued one identical query.** The result cache answered
100% of it, so the column store never ran and level 4 looked identical to level
1. The mix now varies its filter, and `--disable result_cache` exists so a
benchmark can isolate one optimization from another that would mask it.

## A rule I got wrong twice

`applicability` answers "is this possible", never "is this a good idea". Both
`result_cache` and `column_store` were first written checking
`telemetry.write_fraction` there — and bulk-loading a database makes the
workload look 100% writes, so a freshly loaded database refused its own
optimizations forever on the strength of its loading phase.

Worth judgments belong in `estimate`, where the adaptive driver will weigh them
against a policy. The rule is now documented on the trait method itself rather
than in a design note, because that is where it gets read.
