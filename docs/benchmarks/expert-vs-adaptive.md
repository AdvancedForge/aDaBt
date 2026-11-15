# Expert versus adaptive

The "automatically optimal" goal says an expert tuning by hand adds nothing
over the adaptive engine. This is the measurement of that claim.

**Latest (2026-11-15, sixth re-run, both sizes pinned): the plan cache was
not a cause, and both sides now run the same operations.** Planning takes
0.4–2 µs against misses of 0.4–8 ms, so the plan cache saves under 1%. Its
estimate now rests on measured planning time and a ghost cache, and it
stays off. Point filter and aggregate medians are result-cache hits, and the
two sides had been measured on different operations, with different hit
rates in their windows (87% against 75% on aggregate). The expert now
replays the soak's own operations. At 5,000 rows 3 of 5 phases match
(aggregate went from 1.26× to 0.99×). Point filter is 1.02×, 10 ns outside
the expert's range. Range filter is 1.31×. At 20,000 rows, two loaded runs
put range filter at 1.47–1.55× and point filter at 1.14–1.16×. In range
filter the gap is the direct array: it is all of level 11's advantage there
(525 to 405 µs). Adaptive can now claim it on evidence, but the default
priorities price it below the bar. In point filter, the same configuration
measured fresh is ~200 ns faster. Aggregate is 1.15–1.64×, on hits whose
cost varies run to run. See
[the sixth re-run](#re-run-2026-11-15-evidence-for-the-plan-cache-and-the-same-traffic-for-both).

**Previous (2026-11-15, fifth re-run, 20,000 rows pinned to one core): live
counts are O(1) and identity again is 1.09×.** Identity matches (1.03×);
point filter 1.11×, range filter 1.48× and aggregate 1.72× are behind on
configuration, and in all three the expert runs the plan cache, which
adaptive never enables. See
[the fifth re-run](#re-run-2026-11-15-o1-live-counts-pinned).

**Previous (2026-11-14, fourth re-run, both sizes): the identity gap was the
optimizer's cycle.** Every 500 operations a cycle scanned every record to
find each collection's highest id. That took ~4 ms against the ~100 µs of
lookups between cycles, and the lookups after it ran on flushed caches. The
id now comes off the page directory. At 20,000 rows, identity went from
1.33× to 1.03× (matches) and identity again from 1.52× to 1.12×. At 5,000
rows identity matches, point filter is ahead, and identity again is 1 ns
outside the expert's range. Range filter (1.6× at both sizes) and
aggregate (1.26× and 1.71×) are behind on configuration. The expert was
faster on these this run, and adaptive's own figures did not move. See
[the fourth re-run](#re-run-2026-11-14-the-highest-id-off-the-directory).

**Previous (2026-11-14, third re-run, 20,000 rows only): the optimizer's
inputs are now sampled.** Range filter moved from 1.74× to 1.23× behind and
aggregate from 1.99× to 1.12×. The identity phases did not move (1.33× and
1.52×) on the expert's own configuration, so per-cycle scans were not their
cause. At 5,000 rows the second re-run stands: 3 of 5 phases match. See
[the third re-run](#re-run-2026-11-14-a-bounded-sample-each-cycle).

**Result (2026-11-14 re-run): the claim does not hold yet, but the
gaps are now small.** After the scoring fixes described in
[the re-run section](#re-run-2026-11-14-scoring-on-evidence), adaptive
matches the expert on three of five phases at 5,000 rows and one of five at
20,000. The 1,000×-class gaps are gone; what is left is 1.35–1.8×. At 20,000
rows the one configuration difference on the identity phases, and the one
structure adaptive carries through every phase, is record compression: it
trades latency for memory, the expert (optimizing latency alone) never picks
it, and adaptive picks it on an estimate that claims to save more bytes than
the database holds.

The original measurement follows unchanged. At the time adaptive matched one
of five phases at 5,000 rows and none at 20,000, behind by 1.2× to more than
1,000×, because the adaptive driver turned the result cache on late or not
at all.

## Method

```
cargo build --release -p adabt-bench
adabt-bench expert-vs-adaptive --size 5000  --ops-per-phase 20000 --repeats 3 \
    --data-dir ~/.cache/adabt-bench/evsa-5000
adabt-bench expert-vs-adaptive --size 20000 --ops-per-phase 20000 --repeats 3 \
    --data-dir ~/.cache/adabt-bench/evsa-20000
```

**Machine:** Intel Core i3-1315U (8 threads), 15 GB RAM, ext4 on NVMe,
rustc 1.96.0, release build. The machine was shared with other builds while
this ran (load average about 4–5), so the spreads below are wider than a
quiet machine would give. That is why the noise is measured instead of
assumed.

**Traffic:** the five soak phases (`adabt-bench soak`): identity lookups,
point filters (`country = ?`, sort by balance, limit 20), range filters
(`age` in a 5-wide window, limit 50), grouped aggregates (`age >= ?`, count
and mean balance by country), then identity lookups again. Point and range
phases carry 2% writes, the aggregate phase 0.5%. Durability is the default
(strict) on every side. Writes are not timed.

**Contestants**, per phase:

- **adaptive:** the engine in adaptive mode, driven through the whole soak
  exactly as `adabt-bench soak` drives it (default priorities speed 9,
  resources 3, freedom 5). It is measured over the last tenth of each phase
  (2,000 operations), which is the soak's own definition of "settled". It
  carries whatever it built for earlier phases, as a real deployment would.
  It is not told the phase.
- **expert:** for each phase separately, the fastest of every candidate
  manual configuration, measured on that phase's traffic alone in a fresh
  database. The candidates are:
  - every level preset 0–11, and
  - two hand-written configurations per phase:

    | phase | hand-written candidates |
    |---|---|
    | identity | direct lookup; plan cache + direct lookup |
    | point filter | plan + result cache + hash index on `country`; same with a bitmap index |
    | range filter | plan + result cache + b-tree on `age`; same plus clustered by `age` |
    | aggregate | plan + result cache + column store + materialized views; same plus b-tree on `age` |

  Each manual run warms up for 1,000 operations, applies its configuration
  (several optimizations pick their scope from telemetry, so they need
  traffic first), warms up for another 1,000, then measures 2,000
  operations, the same window the adaptive side is measured on. The expert
  knows the phase in advance and gets a fresh database for it. Both are
  advantages over adaptive, and both are deliberate.

  Since the 2026-11-15 sixth re-run the expert's traffic is the soak's own
  operations: the same queries and writes, ending on the same last 2,000
  operations of the phase adaptive is measured on, over the same rows.
  Before that it was a stream seeded by phase alone, the same kind of
  traffic but not the same operations; see
  [the sixth re-run](#re-run-2026-11-15-evidence-for-the-plan-cache-and-the-same-traffic-for-both).
- **level 0:** the floor. It also serves as the reference: every manual
  configuration's sampled answers (every 64th query) are compared with level
  0's on the identical query stream. Adaptive answers are checked by the
  soak's own level-0 reference. **No divergences in any run.**

**Noise:** every cell runs 3 times. A cell's figure is the median of its
three p50s, and its range is the lowest to the highest p50. Adaptive
**matches** when its range overlaps the expert's, is **behind** when its
fastest repeat is slower than the expert's slowest, and **ahead** in the
mirror case. Choosing the expert as the minimum over about 14 candidates
favours a lucky draw. That bias helps the expert, so "matches" is the
conservative verdict to earn.

## Results

### 5,000 rows

| phase | level 0 p50 | expert p50 (range) | adaptive p50 (range) | adaptive / expert | verdict | expert's pick |
|---|---|---|---|---|---|---|
| identity | 366 ns | 232 ns (232–467) | 228 ns (197–247) | 0.98× | **matches** | hand: direct lookup |
| point filter | 1.75 ms | 1.8 µs (1.8–2.2) | 328 µs (264–375) | 178× | behind | level 2 |
| range filter | 1.63 ms | 139 µs (128–152) | 205 µs (157–228) | 1.47× | behind | level 10 |
| aggregate | 4.98 ms | 1.1 µs (1.0–1.9) | 1.66 ms (1.62–1.71) | 1,569× | behind | hand: caches + column store + views + b-tree |
| identity again | 965 ns | 176 ns (175–177) | 315 ns (272–357) | 1.79× | behind | hand: plan cache + direct lookup |

p99 (median of repeats), expert / adaptive: identity 436 / 451 ns; point
filter 468 / 486 µs; range filter 250 / 349 µs; aggregate 2.17 / 3.11 ms;
identity again 224 / 506 ns.

### 20,000 rows

| phase | level 0 p50 | expert p50 (range) | adaptive p50 (range) | adaptive / expert | verdict | expert's pick |
|---|---|---|---|---|---|---|
| identity | 368 ns | 198 ns (183–260) | 524 ns (521–814) | 2.65× | behind | hand: direct lookup |
| point filter | 6.77 ms | 1.7 µs (1.7–1.9) | 1.77 ms (1.54–1.82) | 1,019× | behind | level 1 |
| range filter | 6.56 ms | 475 µs (404–495) | 650 µs (635–697) | 1.37× | behind | level 11 |
| aggregate | 9.48 ms | 1.1 µs (1.1–1.1) | 1.8 µs (1.7–2.2) | 1.60× | behind | level 7 |
| identity again | 488 ns | 226 ns (186–239) | 277 ns (275–308) | 1.23× | behind | hand: plan cache + direct lookup |

p99 (median of repeats), expert / adaptive: identity 382 / 1,322 ns; point
filter 5.89 / 3.00 ms; range filter 852 / 1,224 µs; aggregate 8.85 / 8.11 ms;
identity again 418 / 480 ns.

Wall time: 35 minutes at 5,000 rows and 92 minutes at 20,000. Most of it is
loading a fresh database with strict durability for each of the 225 manual
cells.


The full per-candidate tables, with what each side had switched on, are in
the command's output. The excerpts that matter are in the reading below.

## Reading

**Identity: matches at 5,000 rows, behind at 20,000.** At 5,000 rows both
sides end up on direct lookup, and the adaptive engine finds it on its own
within the phase. At 20,000 rows, in every repeat, the adaptive engine ended
the phase without it, on delta encoding, record compression and
thread-per-core. Its p50 (524 ns) was then slower than level 0 (368 ns). It
did enable direct lookup later in the run, which is why identity-again is
closer. Which optimizations get proposed first, and whether the
direct-lookup experiment completes, decides this phase. At 5,000 rows the
soak log shows that experiment reverted once ("built nothing that can be
held beside the current representation") and was applied outright on a
later cycle.

**Point filter and aggregate: the result cache.** Every expert pick on these
phases has the **result cache**. The point-filter phase issues only 8
distinct queries and the aggregate phase 60. Under a 2% (0.5%) write trickle
most queries are still cache hits, answered in about 2 µs instead of being
executed. The expert's p99 is close to adaptive's, and at 20,000 rows it is
worse: the tail is the cache misses after a write, where both sides execute
much the same plan. The gap is in the median, and the median is the cache.

- At 5,000 rows adaptive never enables the cache. It is 178× behind on point
  filters and 1,569× behind on aggregates.
- At 20,000 rows adaptive enables it during the range phase, which is too late
  for point filters (1,019× behind). By the aggregate phase it has the cache,
  and the gap closes to 1.6×.

The likely reason is in the scoring, not the optimization itself:

- `result_cache`'s estimate reserves a fixed 256 entries × 4 KiB = 1 MiB.
  `score_against` judges memory as a fraction of the database's current
  footprint, floored at 1 MiB. On a small database the reservation costs the
  whole resource budget (the term is clamped at −1, times `resources` = 3).
  On a larger database the same 1 MiB is a smaller fraction, and the score
  can clear the bar. That fits the cache appearing only at 20,000 rows, and
  only after the earlier phases had grown the footprint further with derived
  structures.
- The speed side is estimated at a 49% p50 gain with 35% confidence. The
  estimate does not look at how often queries repeat, which is the one fact
  that decides whether a result cache pays. The measured gain here is above
  99%.
- Until the score clears `MIN_SCORE` (0.5), the cache is never proposed and
  never gets the shadow trial that would have measured it. The soak's
  `--log` at 5,000 rows shows no cache decision at all.

The plan cache is held back the same way: its estimate claims only a 5% gain
against a 2 MiB reservation.

Without the cache, the aggregate phase is about 1.6 ms for both sides at
5,000 rows. Materialized views do not help, because they serve only
unfiltered groupings and every query in this phase filters on `age >= ?`.

**Range filter: behind by 1.4–1.5×.** Adaptive builds the right structure, a
b-tree on `age` clustered by `age`, the same core as the winning presets. At
5,000 rows the presets that beat it also carry the plan and result caches.
At 20,000 rows adaptive has the result cache too, and level 11 still wins by
1.37× with disjoint ranges. The differences between level 11's configuration
and adaptive's are the plan cache, the buffer pool, record compression and
the remaining level-11 structures. This run does not separate which of them
matters. The hand-written "b-tree + clustered" candidate shows a spread above
100% at both sizes, so on this phase single cells are noisy.

**Identity again: behind by 1.2–1.8×.** By the last phase adaptive is
carrying everything it built for the three middle phases: the `age` index,
clustering, column store, delta encoding and, at 20,000 rows, the result cache
and thread-per-core. None of these help an identity-only workload, and the
pure "direct lookup" experts are faster. The retraction logic is meant to
drop structures that stop paying, but at 0 writes an index costs no
maintenance, so none of the retraction triggers fire. This is the "return to
the first phase" check the soak script was designed for. It shows that
retraction does not clear out structures from ended phases when they cost
nothing to keep.

## What this means for the goal

"An expert tuning by hand adds nothing" is **not met**. The roadmap now
records the result cache's admission, the identity phase's missed direct
lookup, and retraction after a phase ends as the work between the engine and
the claim. The comparison is the test for
both: re-run it after any change to the adaptive driver's scoring.

## Re-run 2026-11-14: scoring on evidence

The first measurement traced most of the gap to the adaptive driver's
scoring: estimates that ignored evidence the engine either had or could
cheaply collect. Commit `b23d802` added that evidence (a ghost result cache
that counts the hits a disabled cache would have had and the bytes it would
hold; compiled identity lookups reported to telemetry again). This re-run
measures the scoring changes that use it:

| commit | change |
|---|---|
| `16c0739` | The result cache is judged on its measured (or ghost) hit rate, with confidence growing with probes, and on its resident bytes instead of its 1 MiB capacity. |
| `707e6a9` | Calibration measures a change on the samples of its query recorded since it was made (latency histograms never decay), only when before and after are the same query with at least 100 samples, and never across a running experiment. |
| `e6ca365` | Memory is judged as the change in footprint relative to the larger of before and after, so doubling and halving the database weigh the same. Spending used to count twice as much as saving. |
| `5957b6e` | Column store, delta encoding, read-ahead, per-core workers, materialized views, direct lookup and clustering claim their gain only on the share of traffic they can help (scans, unfiltered groupings, identity lookups, range reads), inferred from telemetry. |
| `5fe1509` | The driver decides nothing while an experiment runs, since queries then skip the caches and are planned around a hidden candidate. |
| `990be61` | A measurement is read as a correction to what the estimate predicted, so a gain measured on one workload is not claimed on another. |

Same command, data directory layout, contestants and noise rules as above;
the binary was built from `990be61`. **Load:** the machine was shared again.
One-minute load average during the 5,000-row run was 1.9–6.0 (median 3.7),
and during the 20,000-row run 1.1–2.2 (median 1.8). Wall time was 29 minutes
and 76 minutes. Every manual cell's sampled answers matched level 0 (6,426
checks per size), and the soaks behind the adaptive cells checked theirs
against their own level-0 reference: **no divergences**.

### 5,000 rows

| phase | expert p50 (range) | adaptive p50 before → after | adaptive / expert before → after | verdict before → after | adaptive ended with |
|---|---|---|---|---|---|
| identity | 204 ns (191–289) | 228 ns → 197 ns (192–202) | 0.98× → 0.97× | matches → **matches** | direct_lookup |
| point filter | 1.75 µs (1.75–1.76) | 328 µs → 1.74 µs (1.72–2.33) | 178× → 0.99× | behind → **matches** | auto_index[country], result_cache |
| range filter | 118 µs (116–119) | 205 µs → 182 µs (178–230) | 1.47× → 1.55× | behind → behind | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 929 ns (927–932) | 1.66 ms → 1.25 µs (1.21–1.52) | 1,569× → 1.35× | behind → behind | auto_index[age], clustered_sort[age], result_cache |
| identity again | 190 ns (187–200) | 315 ns → 200 ns (197–209) | 1.79× → 1.05× | behind → **matches** | direct_lookup |

p99 (median of repeats), expert / adaptive: identity 354 / 282 ns; point
filter 229 / 354 µs; range filter 201 / 318 µs; aggregate 0.69 / 2.56 ms;
identity again 263 / 326 ns.

### 20,000 rows

| phase | expert p50 (range) | adaptive p50 before → after | adaptive / expert before → after | verdict before → after | adaptive ended with |
|---|---|---|---|---|---|
| identity | 194 ns (193–200) | 524 ns → 263 ns (262–266) | 2.65× → 1.36× | behind → behind | direct_lookup, record_compression |
| point filter | 1.72 µs (1.72–1.76) | 1.77 ms → 1.75 µs (1.74–1.76) | 1,019× → 1.01× | behind → **matches** | auto_index[country], record_compression, result_cache |
| range filter | 408 µs (403–411) | 650 µs → 721 µs (713–726) | 1.37× → 1.77× | behind → behind | auto_index[age], clustered_sort[age], record_compression, result_cache |
| aggregate | 953 ns (946–982) | 1.8 µs → 1.61 µs (1.57–1.62) | 1.60× → 1.68× | behind → behind | auto_index[age], clustered_sort[age], record_compression, result_cache |
| identity again | 198 ns (194–200) | 277 ns → 279 ns (277–283) | 1.23× → 1.41× | behind → behind | direct_lookup, record_compression |

p99 (median of repeats), expert / adaptive: identity 325 / 438 ns; point
filter 5.71 / 1.56 ms; range filter 484 / 948 µs; aggregate 2.74 / 9.83 ms;
identity again 335 / 455 ns.

(The "before" columns are the first measurement's adaptive figures; the
experts were re-measured in this run and moved by a few percent, within
their published ranges, except the 20,000-row identity expert, now
`plan cache + direct lookup`.)

### Reading the re-run

**The result cache now arrives in the phase that needs it.** On the point
filters the ghost measures an 85% would-be hit rate on about 40 KB, the
cache scores 1.82 in the phase's first decision cycle and stays on. The
point-filter phase now matches at both sizes, from 178× and 1,019× behind,
and the aggregate phase closes from 1,569× to 1.35× at 5,000 rows. In the
range phase the cache is admitted once the hit rate and resident bytes are
measured (about 45% of queries repeat, on up to ~600 KB), and it is kept
through the aggregate phase.

**Each phase now ends with that phase's structures.** At 5,000 rows the
adaptive configuration at the end of every phase is the expert's core: direct
lookup alone for identity (the first measurement also carried delta
encoding, read-ahead and, transiently, a column store and views), index plus
cache for point filters, b-tree and clustering on `age` plus cache for
ranges. The decision log of a 5,000-row soak at `990be61` has 16 decisions
and no add-then-drop of the same structure within a phase. Each structure is
dropped once, after its phase ends (an index "not chosen in the last 2,007
operations"; clustering, cache and direct lookup when they fall below the
keep score), and direct lookup is re-added in the last phase. That is the
retraction the first measurement found missing.

**What is still behind, and why:**

- **Record compression at 20,000 rows.** It is the only configuration
  difference on both identity phases, and the only structure adaptive
  carries through every phase. Its estimate saves a fixed 64 bytes a row;
  these records occupy about 56, so it claims to save more than the database
  holds, which scores as freeing all of it. Against a 5% latency cost and
  `resources: 3` that clears the bar. A cap at a fraction of the footprint
  was tried and withdrawn: once compression is on, the footprint it is
  judged against is the compressed one, the claim halves, and the change is
  retracted and re-admitted. A sound estimate needs the uncompressed size
  of what is stored, which `OptContext` does not carry. This is the next
  fix, and it most likely accounts for the 1.36–1.41× on the identity
  phases and part of the range and aggregate gaps.
- **Aggregate, 1.35× (5,000) and 1.68× (20,000).** Both sides answer most
  queries from the result cache. The expert's hits take ~0.95 µs, adaptive's
  ~1.25–1.6 µs. The expert carries the plan cache, a column store and views
  and no `age` index or clustering. This run does not separate which
  difference costs the 0.3–0.6 µs; the plan cache's estimate is still a
  workload-blind 5% prior that never clears the bar.
- **Range filter, 1.55× (5,000) and 1.77× (20,000).** Adaptive builds the
  winning presets' core (b-tree and clustering on `age`, result cache). The
  winning presets, level 10 and 11, add the plan cache, buffer pool, column
  store and the rest of the high levels; as in the first run, this was not
  separated further. At 20,000 rows record compression is a plausible part
  of it.

## What the re-run means for the goal

Still **not met**: 3 of 5 phases at 5,000 rows, 1 of 5 at 20,000. The
1,000×-class failures were scoring, and they are fixed. The remaining
1.35–1.8× gaps have a named first suspect (record compression's estimate)
and two unexplained ones (the plan cache and the aggregate hit path).

## Re-run 2026-11-14: record compression sized from the records

The re-run above named record compression's estimate as the first suspect
at 20,000 rows. It claimed a fixed 64 bytes saved a row. These records
occupy about 56 bytes stored, 43 of them record, which is under the 64-byte
minimum below which the store keeps a record raw. Compression could save
nothing here, and the estimate claimed it would free the whole database.

| commit | change |
|---|---|
| `4f193b8` | `HeapStore::record_footprint` reports each collection's uncompressed record bytes (read from slot framing) and the ratio compression would store them at (measured on up to 256 evenly spaced records). Both read the same whether compression is on or off. |
| `b59edeb` | `WorkloadMix` counts the reads that reach stored records: every read the result cache did not answer. |
| `0133cca` | `OptContext::record_bytes` carries those figures, filled in by `opt_inputs`. |
| `9426486` | The estimate claims uncompressed bytes × (1 − measured ratio), split 3:1 between storage and residency so the saving is counted once. The claim does not depend on the current setting, so it cannot retract and re-add itself. Decode CPU and latency scale with the share of record reads. |

Same command, contestants and noise rules as above. The binary was built
from `9426486`. **Load:** the machine was shared. The one-minute load
average, sampled every 30 s, was 2.0–6.9 (median 3.1) during the 5,000-row
run and 1.3–7.6 (median 1.9) during the 20,000-row run. Wall time was 23 and
78 minutes. Every manual cell's sampled answers matched level 0 (6,426
checks per size), and the adaptive soaks checked theirs: **no
divergences**.

### 5,000 rows

| phase | expert p50 (range) | adaptive p50 before → after | adaptive / expert before → after | verdict before → after | adaptive ended with |
|---|---|---|---|---|---|
| identity | 196 ns (189–202) | 197 ns → 195 ns (193–212) | 0.97× → 0.99× | matches → **matches** | direct_lookup |
| point filter | 1.79 µs (1.71–1.87) | 1.74 µs → 1.73 µs (1.73–1.74) | 0.99× → 0.97× | matches → **matches** | auto_index[country], result_cache |
| range filter | 115 µs (105–137) | 182 µs → 166 µs (166–168) | 1.55× → 1.44× | behind → behind | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 930 ns (920–936) | 1.25 µs → 1.30 µs (1.26–1.32) | 1.35× → 1.40× | behind → behind | auto_index[age], clustered_sort[age], result_cache |
| identity again | 192 ns (189–295) | 200 ns → 209 ns (200–217) | 1.05× → 1.09× | matches → **matches** | direct_lookup |

p99 (median of repeats), expert / adaptive: identity 260 / 303 ns; point
filter 454 / 333 µs; range filter 230 / 299 µs; aggregate 0.69 / 2.52 ms;
identity again 376 / 354 ns.

### 20,000 rows

| phase | expert p50 (range) | adaptive p50 before → after | adaptive / expert before → after | verdict before → after | adaptive ended with |
|---|---|---|---|---|---|
| identity | 202 ns (195–419) | 263 ns → 271 ns (270–284) | 1.36× → 1.34× | behind → matches* | direct_lookup |
| point filter | 1.73 µs (1.71–1.74) | 1.75 µs → 1.94 µs (1.89–2.20) | 1.01× → 1.12× | matches → behind | auto_index[country], result_cache |
| range filter | 393 µs (391–405) | 721 µs → 686 µs (685–728) | 1.77× → 1.74× | behind → behind | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 960 ns (950–966) | 1.61 µs → 1.91 µs (1.69–2.65) | 1.68× → 1.99× | behind → behind | auto_index[age], clustered_sort[age], result_cache |
| identity again | 196 ns (195–200) | 279 ns → 294 ns (293–359) | 1.41× → 1.50× | behind → behind | direct_lookup |

\* Only because one expert repeat took 419 ns (spread 111%). The other two
were about 200 ns, so this is not a real match.

p99 (median of repeats), expert / adaptive: identity 337 / 435 ns; point
filter 1.27 / 1.72 ms; range filter 466 / 1,020 µs; aggregate 2.75 / 9.69 ms;
identity again 336 / 604 ns.

("Before" is the previous re-run. The expert is re-measured each run. The
20,000-row identity expert is now `direct lookup` alone, the same
configuration adaptive ends the phase with.)

### Reading the second re-run

**Compression is gone, and the gaps did not close.** Adaptive no longer ends
any phase with record compression at either size. A separate 20,000-row soak
with the decision log on made 16 decisions and never proposed compression:
with a measured ratio of 1.0 the estimate claims no saving, which never pays
for the CPU. On the identity phases at 20,000 rows, adaptive and the expert
now have **the same configuration** (direct lookup, nothing else), and
adaptive is still 1.34× and 1.50× behind. Compression was a real
over-claim, but it was not what cost those phases. The previous section's
"most likely accounts for the 1.36–1.41×" was wrong.

**What is still behind, and why (as far as this run shows):**

- **Identity at 20,000 rows, 1.34–1.50×, same configuration.** The only
  difference left is history. The adaptive database built direct lookup
  while it was serving traffic. It runs a decision cycle every 500
  operations and a telemetry decay with it, including inside the measured
  window. In the last phase, it also carries pages and versions written by
  three earlier phases. The expert gets a fresh database with its
  configuration applied after 1,000 warm-up operations. At 5,000 rows the
  same comparison matches, so whatever it is grows with size. This run does
  not separate these causes. The next measurement is adaptive's final
  configuration applied manually to a database that has been through the
  same soak. That would separate configuration from history.
- **Point filter at 20,000 rows, 1.01× → 1.12×.** The configuration is the
  previous run's minus compression. Adaptive's range (1.89–2.20 µs, 16%
  spread) is wider than in any other point-filter cell, and load peaked at
  7.6 during this run. This reads as noise rather than a regression from
  the change, but the verdict is "behind" and is reported as such.
- **Aggregate, 1.40× and 1.99×**, and **range filter, 1.44× and 1.74×.** The
  configurations are unchanged from the previous run apart from
  compression, which is now absent. The expert's aggregate configuration is
  plan cache, result cache, column store and views. Adaptive's is an `age`
  index with clustering plus the result cache. The range winners are the
  level 10/11 presets. The 20,000-row aggregate cell's 50% spread
  (1.69–2.65 µs) makes its ratio unreliable in either direction.

## What the second re-run means for the goal

Still **not met**: 3 of 5 phases at 5,000 rows and 1 of 5 at 20,000 (the
20,000-row identity match is an artifact of one slow expert repeat).
Record compression's estimate is now sound and no longer chosen where it
cannot save anything. The 20,000-row identity gaps survive with identical
configurations, so the next suspect there is the adaptive database's state
and its decision cycles, not its choices. The aggregate and range gaps
still have the plan cache and the aggregate hit path as their unseparated
suspects.

## Re-run 2026-11-14: a bounded sample each cycle

**Change.** Every optimization cycle (every 500 operations, including inside
measured windows) summed every live record's stored size and read every
record's framing for the compression footprint: a page read per record,
twice, at a cost that grows with the database. Both now come from an evenly
spaced sample of at most 256 records per collection
(`HeapStore::record_footprint`, `HeapStore::sampled_stored_bytes`), commit
`50fe324`.

**Run.** `adabt-bench expert-vs-adaptive --size 20000`, 3 repeats, 5,459 s.
One-minute load average was 3.8 at the start and 2.3 at the end; the machine
was shared. 6,426 manual answers checked against level 0; no divergence.

| phase | level 0 p50 | expert p50 | adaptive p50 | ratio | previous | verdict | adaptive config |
|---|---|---|---|---|---|---|---|
| identity | 402 ns | 209 ns | 278 ns | 1.33× | 1.34× | behind | direct_lookup |
| point filter | 5,717 µs | 1,691 ns | 1,879 ns | 1.11× | 1.12× | behind | auto_index[country], result_cache |
| range filter | 4,272 µs | 539.8 µs | 666.5 µs | 1.23× | 1.74× | behind | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 13.06 ms | 1,550 ns | 1,729 ns | 1.12× | 1.99× | behind | auto_index[age], clustered_sort[age], result_cache |
| identity again | 452 ns | 195 ns | 297 ns | 1.52× | 1.50× | behind | direct_lookup |

### Reading the third re-run

- **Range filter and aggregate closed most of their gap.** Their
  configurations are unchanged, so the full-record passes each cycle were a
  real part of those losses. The aggregate cell's adaptive range is still
  wide (1.70–5.09 µs), so its ratio is not settled.
- **The identity phases did not move.** Adaptive runs the expert's exact
  configuration and is about 70–100 ns slower at the median. A per-cycle cost
  every 500 operations cannot move a median by that much, so the difference
  is paid on every lookup: what an adaptive database does per operation
  (telemetry recording, the compiled path's tally, the result-cache ghost)
  or the state its history left behind. Measuring a manual database with
  telemetry at the adaptive setting separates the two.
- **Point filter** is 1.11×, within the spread of the adaptive cell
  (1.86–2.35 µs).

## Re-run 2026-11-14: the highest id off the directory

**Where the identity gap came from.** A scratch harness drove one
database through identity lookups at 20,000 rows and took the p50 of the
last 2,000 (the soak's settled window). Each figure is from one run; the
spreads across runs were a few nanoseconds unless noted.

| database | identity p50 |
|---|---|
| manual, direct lookup, fresh | 186–190 ns |
| the same database, also driven through the soak's cycles (`optimize_verified` every 500, `advance_experiment` every 100) | 259–262 ns |
| adaptive, the soak's identity phase, last 2,000 | 257–273 ns |
| the same adaptive database, 20,000 more lookups without cycles | 185–191 ns |
| manual, direct lookup, after the soak's first four phases (no cycles while measured) | 220 ns |
| adaptive after the first four phases, cycles off | 206 ns |

So the cost was not per lookup and not state left behind. A quiet
adaptive database is as fast as a manual one, since telemetry is set the
same way in both modes and the compiled path records nothing per call. A
manual database driven through the same cycles loses the same 70 ns. With
the clock read around each call, `advance_experiment` took 13 ns and
`optimize_verified` took 4.2 ms, of which 3.7 ms was `max_ids`. To get each
collection's highest id it took the last row of a full scan, which reads
and decodes every record. At ~200 ns a lookup, the 500 operations between
cycles last about 100 µs. A 4 ms pass through the whole heap between every
one of them left most lookups running on caches it had flushed. A median
can move after all, because the cycles take far longer than the lookups
between them. At 5,000 rows the same pass took about 1 ms and left the
direct array in cache, which is why that size matched. `perf` was not
available (`perf_event_paranoid` is 4), so the breakdown came from timing
each part of `opt_inputs`.

Core placement also matters on this machine. Pinned to an efficiency core
(CPU 6), the quiet manual figure is ~290 ns against ~190 ns on a
performance core (CPU 2). An unpinned long run can land on either. The
before/after figures below were all taken pinned to CPU 2.

**Change.** `HeapStore::max_live_id` reads the highest id whose newest
version is live off the in-memory directory, with no page reads. A
differential test checks it against the scan's last row through generated
inserts, updates and deletes. Commit `b74bcef`. A cycle at 20,000 rows went
from ~4.2 ms to ~0.33 ms.

| harness, pinned to CPU 2 | before | after |
|---|---|---|
| manual + cycles | 260–262 ns | 195–197 ns |
| adaptive identity phase, last 2,000 | 261–272 ns | 195–217 ns |

**The optimizer's evidence is unchanged.** The id is the same, so its
inputs are the same. A full `adabt-bench soak --size 20000 --log` before
and after made the same decisions in the same order with the same verdicts
and ended every phase with the same configuration. Only the measured
latencies and the scores computed from them differ in the last digits.

**Run.** `adabt-bench expert-vs-adaptive --size 20000` and `--size 5000`,
3 repeats each, 4,572 s and 1,222 s, back to back. The one-minute load
average was 3.9 at the start of the 20,000-row run, 2.0 between the runs
and 2.0 at the end. The machine was shared. 6,426 manual answers were
checked against level 0 at each size, with no divergence.

### 20,000 rows

| phase | level 0 p50 | expert p50 (range) | adaptive p50 (range) | ratio | previous | verdict | adaptive config |
|---|---|---|---|---|---|---|---|
| identity | 369 ns | 198 ns (194–210) | 203 ns (197–214) | 1.03× | 1.33× | **matches** | direct_lookup |
| point filter | 4,692 µs | 1,736 ns (1,724–1,756) | 1,911 ns (1,780–1,939) | 1.10× | 1.11× | behind | auto_index[country], result_cache |
| range filter | 4,057 µs | 400.8 µs (392.6–404.7) | 661.1 µs (655.9–669.5) | 1.65× | 1.23× | behind | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 7.89 ms | 974 ns (964–1,018) | 1,670 ns (1,647–1,725) | 1.71× | 1.12× | behind | auto_index[age], clustered_sort[age], result_cache |
| identity again | 373 ns | 198 ns (193–199) | 221 ns (216–221) | 1.12× | 1.52× | behind | direct_lookup |

### 5,000 rows

| phase | level 0 p50 | expert p50 (range) | adaptive p50 (range) | ratio | previous (second re-run) | verdict | adaptive config |
|---|---|---|---|---|---|---|---|
| identity | 452 ns | 189 ns (188–197) | 198 ns (195–199) | 1.05× | 0.99× | **matches** | direct_lookup |
| point filter | 1,116 µs | 1,781 ns (1,780–1,799) | 1,730 ns (1,720–1,746) | 0.97× | 0.97× | **ahead** | auto_index[country], result_cache |
| range filter | 1,012 µs | 105.2 µs (103.2–107.1) | 170.5 µs (166.7–170.5) | 1.62× | 1.44× | behind | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 1,783 µs | 958 ns (950–971) | 1,211 ns (1,190–1,301) | 1.26× | 1.40× | behind | auto_index[age], clustered_sort[age], result_cache |
| identity again | 294 ns | 190 ns (186–195) | 197 ns (196–198) | 1.04× | 1.09× | behind | direct_lookup |

### Reading the fourth re-run

- **The 20,000-row identity phases closed most of their gap.** The first
  phase went from 278 ns to 203 ns and now matches. Identity again went
  from 297 ns to 221 ns, 1.12× and narrowly behind. Both run the expert's
  configuration.
- **What is left on identity again** is about 20 ns at 20,000 rows and
  1 ns at 5,000, where the ranges miss each other by 1 ns. A cycle still
  costs ~0.33 ms. It walks the page directory several times (live counts
  in `collection_sizes` and twice in each 256-record sample) and reads the
  sampled records' pages. In the harness that left the soak's last 2,000
  lookups at 195–217 ns against 186–191 ns with cycles off. Making live
  counts O(1) is the next step there.
- **Range filter and aggregate look worse, but adaptive did not get
  slower.** Its cells are within a few percent of the third re-run (661
  against 667 µs, 1,670 against 1,729 ns). The expert's got faster on a
  quieter machine (401 against 540 µs, 974 against 1,550 ns). The third
  re-run's 1.23× and 1.12× flattered adaptive. The gaps there are
  configuration: the expert wins with the level 11 preset on range and
  with column store plus materialized views on aggregate, and adaptive
  picks neither.
- **Point filter** is 1.10× at 20,000 rows, with the adaptive range
  (1,780–1,939 ns) almost reaching the expert's. At 5,000 rows it is ahead.

## What the fourth re-run means for the goal

Still **not met**: 2 of 5 phases at 5,000 rows (one of them ahead), and 1
of 5 at 20,000. The identity gaps were the optimizer's own cycle cost, not
its choices. With most of that cost removed, identity again is within
~10% at 20,000 rows and within 1 ns at 5,000. Range filter (1.6×) and
aggregate (1.3–1.7×) are now the large losses, and both come from
configurations adaptive does not choose.

## Re-run 2026-11-15: O(1) live counts, pinned

**Change.** Live record counts were a walk of every id in the directory,
asked on every plan and every cycle. A `Directory` type now keeps the count
beside the id map, changed only by the four mutations that can affect it
(commit `9ef4d01`).

**Run.** `taskset -c 2 adabt-bench expert-vs-adaptive --size 20000`, 3
repeats, 4,617 s, pinned to one performance core: this machine's efficiency
cores run the identity lookups about 50% slower, and an unpinned run can land
on either. One-minute load average was 3.3 at the start and 1.9 at the end.
6,426 manual answers checked against level 0; no divergence.

| phase | expert p50 | adaptive p50 | ratio | fourth re-run | verdict | expert config | adaptive config |
|---|---|---|---|---|---|---|---|
| identity | 196 ns | 201 ns | 1.03× | 1.03× | matches | direct_lookup | direct_lookup |
| point filter | 1,727 ns | 1,918 ns | 1.11× | 1.10× | behind | plan_cache, result_cache | auto_index[country], result_cache |
| range filter | 400.0 µs | 592.6 µs | 1.48× | 1.65× | behind | level 11 | auto_index[age], clustered_sort[age], result_cache |
| aggregate | 957 ns | 1,650 ns | 1.72× | 1.71× | behind | column_store, materialized_view, plan_cache, result_cache | auto_index[age], clustered_sort[age], result_cache |
| identity again | 198 ns | 215 ns | 1.09× | 1.12× | behind | direct_lookup | direct_lookup |

### Reading the fifth re-run

- **Pinning tightened every adaptive range** (identity 200–202 ns, against
  272–417 ns in the third re-run), so these ratios are the most reliable
  yet.
- **Identity again** is 17 ns behind on the same configuration. What a cycle
  still costs (about 0.3 ms, mostly reading the sampled pages) is the
  remaining suspect.
- **The other three gaps are choices.** The expert runs the plan cache in
  all three; adaptive's estimate for it is still a workload-blind 5% prior
  that never clears the bar. Aggregate also wants the column store and a
  materialized view, and in that phase adaptive still carries the
  range-filter phase's index and clustering.

## Re-run 2026-11-15: evidence for the plan cache, and the same traffic for both

The fifth re-run left three gaps at 20,000 rows (point filter 1.11×, range
filter 1.48×, aggregate 1.72×), each with the plan cache in the expert's
configuration and not in adaptive's. Each was taken apart with a scratch
harness before anything changed: the expert's procedure (fresh database,
warm 1,000, configure, warm 1,000, measure 2,000) for any set of
overrides, splitting each query by whether the result cache answered it,
and timing `Database::plan` on every eighth query. All pinned to CPU 2 at
20,000 rows; figures are per repeat.

### What the measurements located

**The plan cache is not a cause anywhere.** Planning (`plan()`, which
builds the planner's context and decides) took 0.4–2 µs. The result cache
answers repeated queries before planning, so the queries that plan are the
misses, and those execute for 1.2–5 ms (point filter), 0.4–0.6 ms (range)
and 2–8.5 ms (aggregate): planning is under 0.5% of them. With and without
the plan cache, everything else equal:

| phase | without plan cache | with plan cache |
|---|---|---|
| point filter (result cache) | 2,242 / 2,310 ns | 2,505 / 2,330 ns |
| aggregate (result cache, column store, views) | 1,115 / 1,119 ns | 1,097 ns (one repeat of two) |

It is in the expert's configuration because level presets and the
hand-written candidates include it, not because it does anything there.

**Point filter and aggregate medians are result-cache hits, and which hit
depends on the window.** In every configuration tried, 92% of the point
filter window and 87% of the aggregate window were hits, and the median
was a hit (~1.1 µs on aggregate in every configuration, including
adaptive's). The adaptive soak's own windows had 88% and 75% hits. The two
sides were measured on different operations: the expert on a stream seeded
by phase, adaptive on the soak's. With fewer hits, adaptive's median sat in
the upper third of its hit distribution while the expert's sat in the
middle, whatever either was configured with. In the soak's aggregate
phase, consecutive 2,000-operation windows on one unchanged database had
67–82% hits.

**Materialized views answer nothing in the aggregate phase.** Its queries
filter (`age >= ?`) and a view answers only unfiltered groupings: no query
in any configuration with views was answered by one, and misses were as
slow with views as without (7.4 against 8.5 ms). The estimate's claim of
nothing on filtered traffic is correct.

**The column store would make aggregate misses 2.4× faster, but not beside
the age index.** Misses (the p99, not the p50):

| aggregate configuration | miss p50 |
|---|---|
| result cache only (scan) | 8.5 ms |
| + b-tree on age | 4.65 ms |
| + b-tree on age + clustered by age | 4.64 ms |
| + b-tree on age + column store | 4.63 ms |
| + column store, no index | 1.9 ms |

With the index present the planner reads `age >= ?` (on average 58% of the
rows) through the index and leaves the column store unused. That is also
why the range phase's index survives into the aggregate phase: the planner
chooses it, and it does beat a scan. The column store estimate claims
nothing because index-served reads are not scans. Making it claim gain
here would admit a structure the planner then ignores; the planner's choice
between a wide index range and a column scan is the lever, and it is not
changed here.

**Clustering by age does nothing measurable** on range (525 against 521 µs)
or aggregate misses (4.64 against 4.65 ms).

**In the range phase, the direct array is the whole of level 11's
advantage.** Medians, three repeats each, load average ~1.7:

| range configuration | p50 |
|---|---|
| result cache + b-tree on age | 520–529 µs |
| + clustered by age | 515–532 µs |
| + direct lookup | 402–406 µs |
| + clustered + direct lookup | 403–411 µs |
| level 11 | 404–407 µs |
| level 11 without direct lookup | 526–534 µs |

Every range miss reads ~1,435 records through the index to return 50 (the
`Limit` is not pushed into the index range), each fetched by id; the
direct array makes those fetches cheaper. Pushing the limit down would cut
the fetches 28-fold for every configuration; it is an executor change
outside this work.

### Changes

- **Planning time per query shape is recorded** (`Event::Planned`,
  `Snapshot::planning`): what each decision cost, measured when made and
  remembered by the cache on a hit, and whether the plan cache served it.
  While the cache is off a ghost of it (shapes only, under the real
  cache's admission, eviction and clear-on-index-change rules) says which
  queries it would have served. The plan cache's estimate is the saved
  planning time over total query time, its memory the bytes it holds or
  would hold, its confidence growing with the queries planned. On this
  benchmark that is well under 1%, so it is still not enabled, now on
  evidence.
- **The direct array claims its gain on index-served fetches**, not only on
  lookups by id (`WorkloadMix::index_fetches`, `id_fetch_share`). What the
  result cache answers among filtered reads is now its hit rate over the
  reads that reached it: during an experiment queries bypass the cache
  without probing it, and the raw count made the range phase look like 80%
  index fetches instead of ~55%. With the count, a soak admitted the direct
  array in the range phase and dropped it again in the same phase.
  In steady state its score there is ~0.2 against the 0.5 bar (speed
  +3.3, resources −0.9, freedom −2.0, at 50% confidence): the default
  priorities (speed 9, resources 3, freedom 5) value a second copy of the
  table and a fixed-width commitment above a 23% faster range median. The
  expert optimizes latency alone and does not pay those costs.
- **The expert now runs the soak's own operations** (`expert.rs`,
  `soak_traffic`): the generator is replayed through the earlier phases to
  the same last tenth of the phase, over the same rows.

Soak (`adabt-bench soak --size 20000 --log`, after all three): the same
decisions as before in the first four phases, nothing added and dropped
within a phase. The only retractions are of the previous phase's
structures.

### Run

`taskset -c 2 adabt-bench expert-vs-adaptive --size 20000`, then
`--size 5000`, 3 repeats each, 5,282 s and 1,409 s. One-minute load
average 3.3 at the start of the 20,000-row run (the machine was shared),
2.0 between the runs and 2.7 at the end. Because the first 20,000-row run
started loaded, it was repeated straight after (4,833 s, load 2.0 at the
start and 2.7 at the end). 6,426 manual answers checked against level 0 in
each run; no divergence.

#### 20,000 rows, first run

| phase | expert p50 (range) | adaptive p50 (range) | ratio | fifth re-run | verdict | expert config |
|---|---|---|---|---|---|---|
| identity | 191 ns (189–191) | 203 ns (194–234) | 1.06× | 1.03× | behind | plan cache + direct lookup |
| point filter | 1,789 ns (1,789–1,852) | 2,080 ns (2,018–2,465) | 1.16× | 1.11× | behind | level 11 |
| range filter | 412.9 µs (404.0–415.2) | 608.7 µs (603.8–611.9) | 1.47× | 1.48× | behind | level 11 |
| aggregate | 1,233 ns (1,174–1,305) | 2,026 ns (1,796–3,106) | 1.64× | 1.72× | behind | caches + column store + views |
| identity again | 191 ns (191–194) | 277 ns (212–344) | 1.45× | 1.09× | behind | plan cache + direct lookup |

#### 20,000 rows, second run

| phase | expert p50 (range) | adaptive p50 (range) | ratio | verdict | expert config |
|---|---|---|---|---|---|
| identity | 193 ns (191–197) | 195 ns (191–197) | 1.01× | **matches** | plan cache + direct lookup |
| point filter | 1,743 ns (1,734–1,819) | 1,979 ns (1,945–2,215) | 1.14× | behind | level 8 |
| range filter | 421.9 µs (421.2–434.5) | 655.1 µs (644.9–659.8) | 1.55× | behind | level 11 |
| aggregate | 1,759 ns (1,673–1,910) | 2,019 ns (1,911–3,182) | 1.15× | behind (by 1 ns) | level 7 |
| identity again | 198 ns (196–277) | 292 ns (222–297) | 1.47× | matches (on the expert's outlier) | plan cache + direct lookup |

#### 5,000 rows

| phase | expert p50 (range) | adaptive p50 (range) | ratio | fourth re-run | verdict | expert config |
|---|---|---|---|---|---|---|
| identity | 197 ns (197–198) | 199 ns (197–206) | 1.01× | 1.05× | **matches** | level 11 |
| point filter | 1,748 ns (1,744–1,778) | 1,790 ns (1,788–1,847) | 1.02× | 0.97× | behind | level 8 |
| range filter | 116.1 µs (115.9–117.5) | 152.6 µs (152.6–155.4) | 1.31× | 1.62× | behind | level 11 |
| aggregate | 1,217 ns (1,079–1,345) | 1,205 ns (1,203–1,211) | 0.99× | 1.26× | **matches** | caches + column store + views |
| identity again | 197 ns (196–198) | 199 ns (198–201) | 1.01× | 1.04× | **matches** | direct lookup |

Adaptive's configuration in every phase is unchanged from the fifth
re-run: direct lookup; index on country and result cache; index and
clustering on age and result cache (range and aggregate); direct lookup.

### Reading the sixth re-run

- **At 5,000 rows three of five phases match**, aggregate among them
  (1.26× to 0.99×) now that both sides answer the same operations. Point
  filter is 10 ns outside the expert's range.
- **At 20,000 rows the two runs disagree by more than the changes.** The
  first started at load 3.3 and its adaptive cells spread up to 65%
  (identity again 212–344 ns, against 215 ns in the fifth re-run). In the
  second, identity matches again, and identity again is 222–297 ns. The
  identity phases run the expert's configuration; their spread is the
  machine, not a choice.
- **Range filter is behind by the direct array** (1.47× and 1.55×), which
  adaptive can now claim on its evidence but which the default priorities
  price above its gain, and by the soak's database: adaptive's own
  configuration measured fresh is 520–529 µs, the soak's window 604–659 µs.
- **Aggregate at 20,000 rows is no longer a window artifact but is not
  settled either.** On the same operations the expert's best cell was
  1,233 ns in one run and 1,759 ns in the other, and its winner changed
  (column store with views, then level 7); adaptive was ~2,020 ns both
  times with one slow repeat (~3.1 µs) each. All of these medians are
  result-cache hits at the same position in the same window, so the
  differences are in what a hit costs on a loaded database, not in which
  structures exist. One asymmetry remains in the harness: adaptive's soak
  runs the level-0 reference's 8 ms full scans between its queries (every
  64th, and every query for 300 after a change), while manual candidates
  have their answers checked afterwards.
- **Point filter** is 1.14–1.16× at 20,000 rows and 1.02× at 5,000, on
  medians that are result-cache hits. Adaptive's configuration is the
  hand-written expert candidate's (index on country and both caches), which
  measured 1,858 and 1,768 ns against adaptive's 2,080 and 1,979 ns: the
  same configuration, ~200 ns apart, so this gap is the soak's database and
  harness, not a choice.
- **The plan cache is settled on evidence.** It saves under 1% here, and
  the expert's figures do not depend on it.
