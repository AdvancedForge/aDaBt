# Where the four tracks actually stand

Scored against the finish lines the plan itself defines, not against milestone
count. A milestone can be "done" and leave its track well short, and two of them
do.

Baseline for comparison is `docs/diagnosis.md`, which scored the same four axes
before any of this work.

| track | then | now | finish line |
|---|---:|---:|---|
| **A — usable** | 60% | **90%** | you could ship on it |
| **B — manually optimal** | 70% | **55%** | the engine's ceiling is the hardware's ceiling |
| **C — automatically optimal** | 40% | **65%** | the expert adds nothing |
| **D — good** | 30% | **35%** | there is a workload where it is demonstrably the right answer |

Track B went *down*. That is not a regression in the code — it is a correction
in the scoring. The 70% was measured against "the mechanism works," and the
mechanism did work. Measured against the finish line actually written down, an
engine with no thread-per-core execution, no io_uring, an owned-`Record` read
path and four of M25's seven layout features missing is not near the hardware's
ceiling. The old number was scoring the wrong thing.

---

## A — Usable · 90%

**Shipped and load-bearing:** superblock and format gating, persisted catalog,
segmented WAL with streaming recovery and heap reclamation, sequences and
`insert_returning`, batch writes, `Decimal`/`Timestamp`, unique constraints,
single-shard snapshot isolation with a transactional differential runner,
tree-shaped versioned wire IR with depth-bounded fuzzing, streaming cursors,
in-place schema evolution, backup/restore/PITR reachable from the engine API,
per-query RAM budgets and cooperative cancellation, slow-query log, metrics,
connection limits, graceful shutdown, hash and indexed-nested-loop joins with
spill, and a SQL `SELECT` front-end.

**What the last 10% is:**

1. **The scale ceiling is ~3M rows.** `HeapStore` holds a
   `BTreeMap<RecordId, VersionChain>` entry per record and every index lives
   entirely in RAM — measured at 400–850 bytes of resident memory per row. This
   is the single largest thing standing between "works" and "you could ship on
   it," and it is an architectural property, not a tuning problem.
2. **Cross-shard transactions.** `WalOp::Begin` logs `participants` and
   `coordinator` — the format was deliberately made 2PC-ready in M19 — but no
   coordinator exists. A transaction spanning shards is not expressible.
3. **Serializable isolation** as a policy-selectable guarantee. The lever
   (`GuaranteeRequirements::max_consistency`) exists and is used; the level does
   not.
4. **DDL is non-transactional.** Documented rather than fixed, which was the
   right call, but it is a real limit.

## B — Manually optimal · 55%

The weakest track, and the one whose gap is most consequential — because
**Track C cannot exceed it.** The optimizer chooses among structures Track B
builds; every layout B lacks is a choice C can never make.

**Shipped:** expressive manual policy (name any action, any scope), bitmap
indexes, composite indexes, per-column dictionary encoding, a bytecode VM for
predicates with 4,000-case differential coverage against the tree evaluator,
zero-copy *filter* evaluation, record compression.

**Missing, roughly in order of what it costs:**

| gap | why it matters |
|---|---|
| **Thread-per-core (M28)** | Everything is one `Mutex` deep. On any modern core count this is the dominant ceiling, and it is the single biggest item on this track. |
| **Zero-copy read path (M27, half done)** | `Record` is a `BTreeMap<String, Value>`: one `String` allocation per field per row, decoded fresh every read. M27 removed this from the filter loop and never touched the fetch loop. The profile puts decode at 251 ns/row. |
| **Covering and partial indexes** | A covering index answers from the index alone — no fetch at all. Given that the fetch path is now the measured cost of a scan, this is worth more here than it would be in most engines. |
| **Clustered sort order** | The only thing that makes a range scan sequential rather than random. |
| **Prefix/delta compression** | Dictionary encoding landed; these two did not. |
| **io_uring (M29)** | Needs an async storage path first. Real, but last. |

## C — Automatically optimal · 65%

**Shipped:** shadow→canary→promote/revert experiments with per-experiment
candidate masking, cost-benefit retraction with a real maintenance-cost counter,
workload fingerprinting and configuration recall, joint search over interacting
optimizations with a greedy fallback past 12 candidates.

**Missing:**

1. **Concurrent experiments (M30).** The runner is still
   `Option<Box<LiveExperiment>>` — one experiment at a time, globally. Both
   safety blockers that justified that are now fixed (per-collection masking,
   per-experiment attribution), so this is unblocked work rather than deferred
   work.
2. **Shadow-copy for non-derived changes (M30).** Only *additions of derived
   representations* can be proved. Compression, freezing and layout changes
   cannot be A/B tested at all — they are applied on a prior and never verified.
3. **Compound reasoning (M32).** Composite index *selection* (the structure
   exists; nothing chooses it), join-order reasoning, data-driven partitioning.
4. **The ceiling problem.** C is scored against "the expert adds nothing." An
   expert with covering indexes and clustered order available beats this
   optimizer easily — not because the optimizer is worse, but because it has
   fewer moves. **Closing B raises C's score without touching C.**

## D — Good · 35%

The least-served track, and the one whose finish line is hardest to fake.

**Shipped:** honest scale evidence including two published refutations of the
project's own claims, IR and wire fuzzing, a SQL parser.

**Missing:**

1. **The comparison that is the actual finish line.** "There is a workload where
   it is the right answer, demonstrably" means numbers against SQLite, RocksDB
   and Postgres — *including the workloads where aDaBt should lose*. None of
   those are installed here and none of that has been run. This is the single
   biggest gap on the track and no amount of internal benchmarking substitutes
   for it.
2. **Hardening (M35).** No loom coverage of the lock-free structures, no
   crash/chaos matrix, no consistency checker. Crash recovery is tested by
   truncating the WAL at arbitrary offsets, which is good and is not the same
   thing.
3. **No SQL shell.** M37 delivered the parser; the CLI beside it was not built.
4. **Security (M38): nothing.** No auth, no TLS, no roles, no per-collection
   permissions. The posture statement says trusted-network-only, which is honest,
   but it means the server cannot be exposed.
5. **Ecosystem (M39).** A README and no examples directory, no C ABI, no
   bindings, no semver or format-compatibility promise.

---

# The plan, in order

Ordered by *what unblocks what*, not by track. Two principles decide the
sequence: **B gates C**, so B's cheap high-value items come before more C work;
and **evidence before optimization**, which is what the last two milestones
demonstrated the hard way.

### 1. Zero-copy fetch path — finish M27

The half of M27 that was never done. `Record` becomes a borrowed view over the
decoded page for read paths, keeping the owned form for writes. Decode is 251
ns/row and every field costs a `String` allocation; the scan path is now the
measured bottleneck and this is the largest single item in it.

*Finish test: a scan of N rows allocates O(1) per row, asserted, not timed.*

**Why first:** it is on the critical path for every read in the system, it is
bounded, and the instrument to prove it already exists.

### 2. Covering and partial indexes — finish M25

Directly exploits (1): a covering index skips the fetch entirely, which is
exactly the cost just measured. Partial indexes are cheap once covering exists.

**Why second:** small, and it widens C's move set — the cheapest way to raise
Track C's score is to give it something new to choose.

### 3. Composite index selection — M32, first half

The structure shipped and *nothing chooses it*. This is the shipped-but-
unreachable pattern one more time, and it is the smallest possible fix for it:
the planner already prefers a composite index when one exists, so this is
optimizer-side only.

### 4. Concurrent experiments — finish M30

Now genuinely unblocked. `Option<Box<LiveExperiment>>` → `Vec` with
per-collection routing. Soak-gated, as the plan requires — running N experiments
concurrently without that gate repeats M15's mistake at larger N.

### 5. The comparison benchmark — finish M36

Install SQLite, RocksDB and Postgres; run YCSB- and TPC-C-shaped workloads
against all four. Publish the losses.

**Why here and not later:** every remaining optimization decision should be made
against an external reference rather than against this engine's own past. It is
also the finish line for Track D, and it is the only item on this list that can
tell us the other four were worth doing.

### 6. Thread-per-core — M28

The biggest remaining performance item and the largest single piece of work on
the list. Deliberately after (5) so its value is measured against something real.
Needs a different concurrency architecture: core pinning, run-to-completion
scheduling, per-core memory, shard-affine connections, built on M15's
partitioning.

### 7. Hardening — M35

Loom for the lock-free structures, a crash/chaos matrix, a consistency checker.
This is what makes the numbers from (5) trustworthy rather than just fast.

### Then, and only then

Cross-shard 2PC and serializable (finishes A), shadow-copy experiments and the
rest of M32 (finishes C), prefix/delta compression and clustered order (finishes
B), security and ecosystem (finishes D).

### Deliberately not on this list

**The scale ceiling.** Paging the directory to disk would lift the ~3M row limit
and it is the correct fix, but it is a storage-engine rewrite and it invalidates
the assumptions behind items 1, 2 and 6. It should be decided as a project
question — *is this engine for datasets that fit in RAM?* — rather than absorbed
as a milestone. If the answer is yes, the ceiling is a documented property and
not a gap, and Track A is at 95% rather than 90%.
