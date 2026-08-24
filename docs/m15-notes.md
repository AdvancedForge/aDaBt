# M15 — running it, and the four bugs that only running it found

Five pieces. The first was a soak: a long adaptive run against a workload that
changes underneath it, checked against a level-0 reference the whole way. It was
supposed to validate the machinery. It found four defects instead, three of which
would have made the self-optimizing claim false in ordinary use, and one of which
caused the decision log to record a success for a structure that had been deleted.

That ratio is the finding. Every component involved passed its own tests. What
none of them could test was each other.

---

## 1. The soak

`adabt-bench soak` runs five phases — identity lookups, point filters, range
filters, aggregates, identity lookups again — against a database in adaptive
mode, and measures each phase's latency at its start and at its end. The
difference between those two numbers is the database noticing.

Alongside it runs a second database pinned at level 0, taking the same queries.
Any difference in results stops the run. Verification is sampled — one query in
sixty-four — but the sampling follows the *changes*: every query is checked for
three hundred after the configuration moves, because that is when divergence is
plausible and a stretch where nothing has moved is not.

The final phase returns to the first phase's traffic on purpose. By then the
database is carrying structures built for three workloads it is no longer
running, and what happens to them is the part of the design that is easiest to
get wrong and hardest to see.

## 2. The experiment loop jammed after one experiment

The first run started exactly one experiment. It reached `Canary(1)`, sat at
`Inconclusive` with 96 of the 200 samples it wanted, and stayed there. Because
only one experiment runs at a time, every later change — two indexes, a column
store, materialized views, prefetch — bypassed the loop entirely and was applied
outright. The loop had not failed; it had stopped being used, and nothing said so.

**At one percent of traffic, N samples cost 100·N queries.** The default of a
thousand needs a hundred thousand queries for the *first* step of the ramp. So
the cheapest and safest step was the slowest to clear, which is backwards.

Each side's requirement is now proportional to the traffic it carries: at
`Canary(1)` the candidate needs 30 and the baseline 990; at `Canary(90)` it is
the other way round. Every step then costs about the same number of queries,
which is the property that lets a ramp finish at all.

An experiment can also now give up. One that receives no new evidence for 250
attempts aborts and says so, because an experiment waiting forever does not
merely fail to finish — it silently stops every later change from being proved.

## 3. The ramp then rejected two good candidates

With the ramp unjammed, two experiments reverted for "p99 regressed 11%" and
"p99 regressed 14%". Their shadow measurements — paired, same query, same state —
had them **49% and 53% faster at p99**.

The evidence line said it: `270 baseline / 30 candidate samples`. A p99 estimated
from thirty samples is the largest of thirty samples wearing a percentile's name.
Compared against a p99 drawn from three thousand, it looks like a regression
whatever the candidate is doing. The fix to §2 had created the asymmetry.

So the statistic is now chosen by how much evidence there is. A tail is compared
only when both sides can support one; below that the median is compared instead,
against a much looser bound. That is not a weakening — it is what the early ramp
is *for*. `Canary(1)` exists to catch a candidate that is plainly broken on a
blast radius of one percent, not to discriminate between two nearly equal
options.

## 4. The retraction reaper ate every candidate mid-trial

The next run promoted all three experiments — and the promoted indexes were not
in the final configuration, and the point-filter phase was twenty times slower
than the run before it. The log said why:

```
#4 enable  auto_index [users.country]  — trialled by experiment #2
#5 disable auto_index [users.country]  — the planner never chose users.country
                                          over 1668 filtered queries
#7 enable  auto_index [users.country]  — experiment #2 promoted
```

An experiment builds its candidate where the planner cannot see it; that is the
whole mechanism. The adaptive driver drops an index the planner never chooses;
that is the reason the optimizer is not a ratchet. Both are correct. Together
they annihilate: the candidate is hidden, so it is never chosen, so it reads as
dead weight, so it is dropped — and the experiment then promotes something that
was deleted several hundred queries earlier and writes a success into the log.

The driver is now told what is under experiment and leaves it alone. Usage
figures for a masked structure describe the experiment, not the workload.

The regression test for this is worth a note of its own: the first version of it
passed without the fix. Shadow trials are deliberately *not* counted in telemetry
— one logical query answered twice would double every statistic — so the reaper
cannot see them at all. The collision only happens in canary, where the baseline
queries are real counted traffic. A test that never reached a canary was testing
nothing.

## 5. Nothing was ever retracted

With all three fixed, the loop worked: shadow, ramp, promote, no divergence in
five thousand checked queries. And across 123,801 queries spanning five
workloads, **the database retracted nothing**. It ended a run of pure identity
lookups still carrying two indexes built for filter workloads that had ended,
paying write maintenance and memory for questions nobody was asking.

The criterion was `index_uses / filtered_queries`, and both counters were
cumulative. An index that served its workload well keeps a high lifetime ratio
forever. **The optimizer could retract something that was never useful; it could
not retract something that had stopped being useful** — which is the entire
difference between following a workload and accumulating structures for
workloads that are over.

Telemetry now forgets. Counters decay by three quarters each optimization cycle,
so a structure that stops being used has its record fall to nothing in about
thirty cycles, and the retraction test becomes "nothing has chosen this lately"
rather than a lifetime ratio. Decay happens on the optimizer's thread once per
cycle, not on the recording path, which keeps the per-shard counters uncontended.

This changed what an existing constant meant without changing the constant.
`MIN_OBSERVATIONS = 500` was "500 operations ever" and is now "roughly 125
operations per cycle sustained", because a decaying counter settles at four times
its per-cycle rate. That reading is the intended one — an optimizer should act on
a database that is busy *now* — and it is written down where the constant is
declared, because a constant whose meaning shifts silently is worse than one that
changes.

The soak now ends like this:

```
after point-filter   auto_index[users.country], column_store, direct_lookup, ...
after range-filter   auto_index[users.age], column_store, direct_lookup, ...
after aggregate      column_store, direct_lookup, materialized_view, prefetch
after identity-again column_store, direct_lookup, materialized_view, prefetch

#9  disable auto_index [users.country] — the planner has not chosen
                                          users.country in the last 1997 operations
#10 disable auto_index [users.age]     — the planner has not chosen
                                          users.age in the last 1998 operations
```

The database sheds what it stopped using, and says why.

One more call-site bug lived here: the decay was wired into `optimize` and the
soak drives `optimize_verified`. A database actually using the experiment loop
therefore never forgot anything. Two entry points, one cycle, and only one of
them was complete.

---

## 6. Recovery no longer reads every page

`directory.adabt` caches the page directory as of the last checkpoint. Unlike the
derived cache it must be validated *before* recovery, since it replaces a step of
it — so its stamp uses only what is knowable before a page is read: the database
identity, the checkpoint's log position, and the heap file's length. That is
sufficient because a checkpoint flushes every dirty page and *then* records where
it did so; the cache describes that heap exactly, and replay carries it forward.

It is keyed by **collection id, not name**, and the reason is a regression it
caused. Freezing a schema hands one collection's records to another: the new
encoding is built beside the old one and adopted in a single log entry, after
which the name refers to a collection with a different id. A record's slot prefix
carries the id, so that is what a page scan recovers. Keyed by name, the cache put
every record back under the pre-migration id; recovery then completed the
adoption by freeing the old collection's pages — by then the pages holding all the
data — and the collection came back empty with no error anywhere.

Measured on 200,000 records with three indexes, on a quiet machine:

| | |
|---|---|
| Open, rebuilding everything | 1,854 ms |
| Open, index contents cached | 871 ms |
| Open, both caches | 768 ms |

**2.4× overall, and the remaining 768ms is not the page scan.** The directory
cache removes about 100ms; the rest is the write-ahead log, which is read in full
on every open because a checkpoint records that pages were flushed but never
truncates the history behind it. That is now the measured next bottleneck, and it
was not visible before these two caches removed what was in front of it.

## 7. `SUM` is materialized after all

The previous milestone excluded `SUM` from materialized views because a
maintained floating-point sum and a scanned one disagree in the low bits. That
was correct in general, and "in general" was doing the work.

There is a condition under which floating-point addition is exact and order stops
mattering: every value an integer, and every partial sum below 2^53. Counts,
quantities and money in minor units all satisfy it. So each accumulator now
carries a budget — it admits a value only if the value is an integer, and tracks
the running total of absolute values ever added *or subtracted*. While that stays
under 2^53 every partial is exactly representable and the view is trustworthy.
The moment either condition fails, the accumulator marks itself inexact and the
view stops answering; the query falls back to the scan.

The budget is a high-water mark and is never reduced on a delete. That is
deliberately conservative: a scan only ever adds the values still present, so a
view that gives up early is safe and one that gives up late is not.

`MIN` and `MAX` remain excluded, for a reason no condition rescues: removing the
current minimum tells you nothing about the new one without re-reading every
remaining value.

## 8. Shared-nothing partitioning

`ShardedDatabase` is *N* complete databases — own directory, own log, own buffer
pool, own indexes, own optimizer, own lock. Records go to `id % shards`, so every
operation on one record touches one shard and the routing costs a remainder. The
server holds an `Arc` and no mutex of its own; two requests for different
partitions contend for nothing.

**It is not thread-per-core.** No core pinning, no run-to-completion scheduler, no
`io_uring`, no NUMA awareness. Those are what makes partitioning worth its last
factor of two and they are a different piece of work built on this one. Calling
this per-core would be claiming that work is done. `--shards 1` is the
unpartitioned behaviour exactly, which is the honest way to measure the rest.

The split is drawn where it is provably safe. Shards run the scan and any filters
— per-row work, independent of every other row. Their results merge **by record
id**, reproducing exactly the order an unpartitioned scan returns. Sorting,
limiting and aggregation then run once, centrally, over rows in that order.

No aggregate is ever computed per shard and combined. Combining partial sums adds
floating-point numbers in an order that depends on the shard count, so the answer
would depend on the partitioning — and `an_aggregate_over_a_partitioned_sum_is_
bit_identical` checks across 1, 2, 5 and 8 shards precisely because two shardings
that were both wrong in the same way would still agree with each other.

A `Limit` is never pushed down either: each shard would apply it locally and the
merged result would be missing whatever the others held.

---

## What the soak still shows, unfixed

**The aggregate phase does not improve.** Across every run it lands between −19%
and +7% while the other four phases improve by 60–97%. Two things are true of it
at once: the mix filters before aggregating, so materialized views cannot serve
it, and the column store is enabled on a 40%-confidence prior that the phase then
fails to justify. The experiment loop would have caught the second — but
`column_store` is proposed and applied before the phase begins, and by the time
the evidence arrives it is not being re-examined. That is the next thing worth
running down, and it is a *measurement* rather than a suspicion, which is more
than could be said before there was a soak.
