# M1 baseline measurements

Recorded after the heap store landed. Machine: WSL2, 8 cores, ext4 on NVMe.

## The durability cliff

Identical workload, identical engine, one policy setting changed:

| workload | durability | ops/sec | p50 | p99 | cpu/Mop |
|---|---|---:|---:|---:|---:|
| write_heavy | strict | **139** | 7.6 ms | 10.0 ms | 170.5 |
| write_heavy | relaxed | **824,756** | 864 ns | 4.4 µs | 3.3 |
| point_lookup | strict | 84,072 | 640 ns | 1.8 µs | 3.3 |
| point_lookup | relaxed | 1,795,486 | 352 ns | 640 ns | ~0 |

**Writes are ~5,900x slower with fsync-per-commit than without.**

This is the single most consequential number in the project so far, and it
settles an architectural question rather than merely informing one.

Guarantees are a *hard eligibility filter*, not a term in the multi-objective
score — see `adabt_core::policy`. That decision was made on principle before any
measurement existed. These numbers show it was also the only workable choice: at
a 5,900x ratio, no weighting of speed against safety would ever keep strict
durability if durability were tradeable. A scoring approach would quietly
"optimize" every database into relaxed durability and call it a win.

## Measurement validity

The first version of these numbers was wrong, and wrong in the direction that
flatters the system: it showed strict durability costing only ~2x.

The cause was the benchmark's scratch directory defaulting to
`std::env::temp_dir()`, which is `/tmp`, which is **tmpfs** on this and most
Linux systems. fsync on tmpfs never reaches a disk. The benchmark was measuring
memory and reporting it as durability.

Nothing in the output looked wrong — durability simply appeared cheap.

The harness now defaults to `~/.cache/adabt-bench` and refuses to stay quiet
about it: `adabt-bench` warns when its data directory is memory-backed, via
`resources::is_memory_backed`. Any future durability figure produced without
that warning being absent should be treated as unverified.
