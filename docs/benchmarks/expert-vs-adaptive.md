# Expert versus adaptive

The "automatically optimal" goal says an expert tuning by hand adds nothing
over the adaptive engine. This is the measurement of that claim.

**Latest (2026-11-14, second re-run): record compression is fixed and was
not the cause.** Its estimate now sizes the saving from the stored records,
adaptive no longer enables it at any size, and the 20,000-row gaps remain:
3 of 5 phases match at 5,000 rows, 1 of 5 at 20,000, with the identity
phases behind on the same configuration as the expert. See
[the second re-run](#re-run-2026-11-14-record-compression-sized-from-the-records).

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
