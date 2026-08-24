# aDaBt — where the project actually stands

Written after the M15 soak, against measurements rather than intentions. The
question is not what the design says; it is what the code does, what it is good
for, and how far it is from each of the four things it could be.

---

## 1. What it actually is

**An embedded database in Rust that changes its own physical representation
while running, and can prove a change is safe before trusting it.**

Concretely, today:

- Records in collections, addressed by a 64-bit id, under a schema that ranges
  from fully dynamic to fixed-width.
- Durable: write-ahead log, checkpoints, recovery from a truncated log at any
  byte offset.
- Queried through a small logical IR: get, scan, filter, project, sort, limit,
  grouped aggregate. **No joins, no transactions spanning statements, no SQL.**
- Ten optimizations that switch on and off at runtime: plan cache, result cache,
  buffer pool sizing, automatic indexing, record compression, column store,
  schema freezing, direct addressing, read-ahead, materialized views.
- A control plane that chooses among them — by a level a human sets, or by
  watching the workload — and logs every decision with its reasoning.
- An experiment loop that builds a candidate where the planner cannot see it,
  compares both paths on identical queries, ramps traffic 1→10→50→90%, and
  promotes or reverts on the evidence.
- Shared-nothing partitioning: *N* complete engines, one lock each.
- A TCP server with a binary protocol and a client.

### The one property everything is arranged around

**Optimization does not change answers.** Not approximately. The differential
runner replays random operation sequences against a reference model at every
level; separate suites run every query shape at every level before and after
every specialisation; the soak checks a live adaptive engine against a level-0
one across 123,801 queries while the physical layer is rebuilt underneath it.

This is why several features are *smaller* than they could be. Sums are
materialized only while integer arithmetic stays exact. Aggregates are never
combined across shards. `MIN`/`MAX` are not maintained at all. Each of those is a
place where a faster implementation was available and was rejected because it
would have moved an answer in the last decimal place.

### What it is not

Not a SQL database. Not a networked database anyone should run in production —
the server is a few hundred lines with no auth, no TLS, no backpressure, no
connection limits. Not multi-statement transactional: there are snapshots and
version chains, but no `BEGIN`. Not distributed; partitions are threads in one
process. Not thread-per-core, despite having partitioning — no core pinning, no
`io_uring`, no zero-copy path.

---

## 2. What it is measurably good at

Every number reproducible from `adabt-bench`; caveats in `docs/`.

| | |
|---|---|
| Strict vs relaxed durability, writes | **5,900×** |
| Automatic index on an equality filter | **10.5×** (soak shadow: 91% faster) |
| Automatic index on a range filter | soak shadow: **96% faster** |
| Column store on unfiltered aggregates | **9.6×** |
| Materialized view vs recomputing | **>10×** |
| Compiled path vs general query path | **3.0×** (1280 → 432 ns) |
| Single field from a computed address | **14×** (1280 → 92 ns) |
| Sequential scan with read-ahead | **>8×** fewer reads |
| Open, with directory + index caches | **2.4×** (1,854 → 768 ms at 200k) |
| Record compression, stored bytes | **>2× smaller** |

The soak's own numbers, adaptive mode, five workloads, 123,801 queries:

| phase | p50 at start | p50 at end | change |
|---|---|---|---|
| identity | 1,099 ns | 436 ns | **−60%** |
| point-filter | 7,079 µs | 1,069 µs | **−85%** |
| range-filter | 12,373 µs | 490 µs | **−96%** |
| aggregate | 1,824 µs | 1,850 µs | **−1%** |
| identity-again | 630 ns | 722 ns | **+15%** |

Four of five phases improve substantially without anyone configuring anything.
The two that do not are discussed in §5.

### Where it would genuinely be useful

- **An embedded store for a workload nobody has time to tune.** This is the real
  fit. Ship it, let it watch, and it builds the indexes the traffic asks for and
  drops them when the traffic stops.
- **A research vehicle for physical self-optimization.** The optimization library
  is separated from the optimizer by a crate boundary that has held for fifteen
  milestones; adding an optimization is a file, not a redesign.
- **A teaching artefact.** The decision log explains, in prose generated from
  structured records, why the database is shaped the way it is.
- **Reproducing the specialization spectrum.** One API from tag-length-value
  dynamic records to `BASE + id × STRIDE`, with the differential runner proving
  the answers never move.

### Where it would not

Anything needing joins, SQL, multi-statement transactions, replication, backup,
authentication, or more than one process. Anything where a wrong answer is
cheaper than a slow one — the whole design trades the other way.

---

## 3. Distance to a normal database

**Close on the storage engine. Far on the query surface.**

Present and genuinely done: slotted pages, buffer pool with pluggable eviction,
write-ahead logging, checkpoints, recovery tested by truncating the log at
arbitrary offsets, MVCC version chains with snapshot reads, secondary indexes,
compression, a page-directory cache.

Missing, roughly in order of how much work each is:

| Gap | Size |
|---|---|
| **Log truncation at checkpoint** — the log is read in full on every open, and is now the dominant cost of opening (768 ms of 768 ms at 200k records) | small |
| **Multi-statement transactions** — snapshots and version chains exist; `BEGIN`/`COMMIT`/rollback and lock management do not | medium |
| **Joins** — the IR has one collection per plan and says so | medium |
| **A query language** — the wire protocol carries a filtered-scan subset, deliberately, because the IR is still moving | medium |
| **Operational surface** — backup, restore, online schema change beyond freezing, metrics export, auth | medium |
| **Replication** | large |

**Verdict: perhaps 60% of an ordinary single-node embedded database.** The half
that is hard to get right — durability and recovery — is the half that is done.
The half that is mostly labour — transactions, joins, a language — is the half
that is not.

## 4. Distance to a *good-choice* database

Meaning: a thing someone would reasonably pick for a real workload.

Done: the correctness discipline is genuinely above average. 708 tests, a
reference-model differential runner, crash tests that truncate the log at twenty
offsets, tests that bit-flip every byte of every cache file, a soak that checks a
live adaptive engine against a naive one.

Missing:

- **An operational story.** No backup, no restore, no way to observe it except by
  asking it. A database you cannot back up is not a database anyone chooses.
- **A stable format.** The record encoding has changed twice this month with no
  migration path. There is no versioning discipline across releases.
- **Concurrency limits.** The server has no connection cap, no backpressure, no
  timeouts. One slow client can hold a shard's lock indefinitely.
- **Scale evidence.** Every measurement here is 5,000–200,000 records on one
  machine. Nothing is known about 100 million.
- **Fuzzing.** The plan called for fuzzing the decoder and the IR. It exists as an
  intention.

**Verdict: 30%.** The engineering underneath is sound; the surrounding apparatus
that makes something choosable barely exists.

## 5. Distance to a *manually optimal* database

Meaning: an expert sets a level or overrides and gets what an expert would build
by hand.

This is the closest to done. Levels resolve to a granular configuration; nothing
in the engine branches on level; every optimization is independently switchable
and reversible; guarantees filter rather than penalise, so `durability: strict`
makes async techniques invisible rather than expensive; the level × workload
matrix shows the axes genuinely trading against each other.

What is missing is **coverage of the specialization spectrum's upper half.**
Levels 8–11 in the original design mean per-core ownership, lock elimination,
kernel bypass and query compilation to machine code. What exists is: schema
freezing (8), direct addressing (10), a specialised compiled path for hot
identity lookups, and shared-nothing partitioning. What does not: run-to-
completion scheduling, `io_uring`, zero-copy network paths, real code generation.

Also missing: **a way for an expert to say something more specific than a level.**
Overrides toggle an optimization; there is no way to say "index this field with
this kind" or "partition on this predicate" through the policy.

**Verdict: 70%.** The mechanism is right and proven. The library of things it can
choose is about half the length the design imagines.

## 6. Distance to an *automatically optimal* database

Meaning: it reaches, unaided, roughly what the expert would have chosen.

This milestone moved it more than any other, and the honest position is now
**"the loop is complete and the judgement is shallow."**

Complete, and demonstrated in the soak: observe → propose → gate → build hidden →
compare on identical queries → ramp traffic → promote or revert → **retract when
the workload moves on**. All of it logged with reasons. Every step of that
sentence was broken at the start of M15 and is measured working at the end.

The judgement is where it falls short, and the soak says exactly where:

1. **The aggregate phase never improves** — between −19% and +7% across every run,
   while the others improve 60–96%. The column store is applied on a
   40%-confidence prior, the phase fails to justify it, and nothing re-examines
   the decision. The cost model corrects a prior *after* a change is applied, but
   only for changes still being observed; a change that settled in earlier is not
   reconsidered.
2. **Only one experiment runs at a time**, so proving a change costs thousands of
   queries and most changes are still applied outright. The loop is used for the
   first eligible proposal per cycle and no others.
3. **Only derived-representation additions can be proved at all.** Compression,
   schema freezing and every tuning knob are applied on estimate alone, because
   after them there is no old path left to compare against. That boundary is
   principled and it is also most of the optimization library.
4. **Retraction is binary and slow.** "Nothing has chosen this lately" over a
   ~30-cycle decay window. There is no notion of an index that is *sometimes*
   worth its upkeep, and no cost-benefit arithmetic weighing write overhead
   against read benefit — only a use/don't-use test.
5. **Everything is single-collection and single-field.** No composite indexes, no
   covering indexes, no partitioning chosen from the data, no denormalization.
   The optimizer cannot propose what it cannot name.
6. **No workload fingerprinting.** The design called for recognising a workload as
   one seen before and jumping to its known-good configuration. Nothing does.

**Verdict: 40%.** It genuinely self-optimizes — that is no longer an aspiration,
it is a measurement — but it optimizes a short list of things with shallow
reasoning, and it takes tens of thousands of queries to converge on what a
competent DBA would write down in a minute.

---

## 7. The four bugs, and what they say about the project

M15's soak found four defects that every component's own tests had passed:

- the experiment ramp demanded evidence in inverse proportion to how cheaply it
  could be gathered, so the loop ran once and silently stopped being used;
- it then rejected candidates on a p99 estimated from thirty samples;
- the retraction reaper ate every candidate mid-trial, because "hidden from the
  planner" and "never chosen by the planner" are indistinguishable from
  telemetry — and the promotion then logged a success for a deleted structure;
- nothing was ever retracted, because cumulative counters answer "was this ever
  useful" and the optimizer needs "is this useful now".

Every one is an *interaction*. Each component was individually correct. This is
the characteristic failure mode of a system whose parts each observe and modify
the others, and the practical lesson is that the soak is not a benchmark — it is
the only test that can see this class of bug at all. It should run on every
significant change.

A fifth, from the previous milestone and the same family: a schema freeze
interrupted by a crash decoded old bytes with a new codec and produced records
that were wrong rather than absent, silently.

**The project's real asset is not any optimization. It is the machinery that
catches optimizations being wrong** — a reference model, a differential runner,
crash tests that truncate at arbitrary offsets, cache tests that flip every byte,
and now a soak that runs the whole loop against a naive control.

## 8. What to do next, in order

1. **Truncate the log at checkpoint.** Small, and it is now the entire remaining
   cost of opening a database.
2. **Run the soak in CI.** It found four defects in one sitting; leaving it as
   something run by hand wastes the most effective test here.
3. **Re-examine settled decisions.** The aggregate phase is a standing,
   reproducible measurement of the optimizer keeping something that is not
   paying. Feed the cost model's correction back into changes already applied.
4. **Transactions.** The single largest gap between this and an ordinary
   database, and everything for it — snapshots, version chains, a log — exists.
5. **Backup and restore.** Currently the reason nobody could choose it.

Per-core ownership, `io_uring` and code generation are further out than any of
these, and until the log is truncated and the optimizer stops keeping things that
do not pay, they would be optimizing the wrong end.
