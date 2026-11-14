# Expert versus adaptive

The "automatically optimal" goal says an expert tuning by hand adds nothing
over the adaptive engine. This is the measurement of that claim.

**Result: the claim does not hold yet.** At 5,000 rows adaptive matches the
expert on one of five soak phases. At 20,000 rows it matches on none. Where
it is behind, the gap runs from 1.2× to more than 1,000×. The large gaps come
from one cause: the adaptive driver turns the result cache on late or not at
all. It never does at 5,000 rows, and at 20,000 rows only in the third phase.

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
