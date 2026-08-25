# Where the four tracks actually stand

Scored against the finish lines the plan itself defines, not against milestone
count. A milestone can be "done" and leave its track well short.

Baseline for comparison is `docs/diagnosis.md`, which scored the same four axes
after M15. The middle column is where this file left them at its last revision;
`docs/roadmap-notes.md` records what closed in between.

| track | baseline | last revision | **now** | finish line |
|---|---:|---:|---:|---|
| **A — usable** | 60% | 90% | **95%** | you could ship on it |
| **B — manually optimal** | 70% | 55% | **60%** | the engine's ceiling is the hardware's ceiling |
| **C — automatically optimal** | 40% | 65% | **75%** | the expert adds nothing |
| **D — good** | 30% | 35% | **55%** | there is a workload where it is demonstrably the right answer |

What moved, and why:

- **A 90 → 95.** The scale question is decided (`docs/scale-decision.md`):
  resident by design, ceiling documented, server states it at startup,
  revisit triggers named. What remains is 2PC, serializable and DDL's
  stated non-transactionality.
- **C 70 → 75.** Covering-index selection landed for both shapes with
  evidence-chosen backing kind — two more moves the expert used to hold
  alone — and settled decisions are re-examined under the calibrated model
  every cycle. The diagnosis's "keeps something that does not pay" standing
  measurement is retired: tuned aggregates now win by four orders of
  magnitude under outside witness.
- **D 45 → 55.** Three surface items landed: the SQL shell (`adabt-cli`),
  an examples directory, and bearer-token auth on the server — the first
  security pillar, tested over real sockets with per-connection state and
  constant-time comparison. TLS and the role model keep "exposed on a
  network" qualified; hardening keeps it unbenchmarked.
- **D's wins deepened** (sort now 2–3× over SQLite; both indexed shapes
  answering through self-proposed covering indexes) but no new evidence
  *class* arrived — still one witness — so the comparison component holds.
- **B held**: its closures this round were selection-side (which raises C);
  the physical library itself gained nothing new.

---

## A — Usable · 95%

**Shipped and load-bearing:** superblock and format gating, persisted catalog,
segmented WAL with truncation at checkpoint, streaming recovery and heap
reclamation, log archival with backup/restore/PITR reachable from the engine
API, sequences and `insert_returning`, batch writes, `Decimal`/`Timestamp`,
unique constraints, single-shard snapshot-isolated transactions with a
transactional differential runner, tree-shaped versioned wire IR with
depth-bounded fuzzing, streaming cursors, in-place schema evolution,
per-query RAM budgets and cooperative cancellation, slow-query log, metrics,
connection limits, idle timeouts, graceful shutdown, hash and indexed-nested-
loop joins with spill, a SQL `SELECT` front-end, and — as of Stage 3 —
**a decided scale contract**: datasets are held resident (~470 B/row,
`docs/scale-decision.md`), stated at server startup, with named revisit
triggers.

**What the last 5% is:**

1. **Cross-shard transactions.** `WalOp::Begin` logs `participants` and
   `coordinator` — the format has been 2PC-ready since M19 — but no coordinator
   exists. A transaction spanning shards is not expressible. With residency
   decided, sharding is the growth story, which makes this load-bearing.
2. **Serializable isolation** as a policy-selectable guarantee. The lever
   (`GuaranteeRequirements::max_consistency`) exists and is used; the level does
   not. The string appears nowhere in the codebase.
3. **DDL is non-transactional,** documented rather than fixed. That remains the
   right call; the finish line requires the documentation to be a stated
   property of the format, not a note in passing.

## B — Manually optimal · 60%

Still the weakest track, still gating C: every layout B lacks is a choice C can
never make.

**Closed since the last revision:** covering indexes (a query whose projection
the index holds reads zero pages — asserted in page reads), partial indexes
(syntactic-containment use rule, deliberately the weakest sound rule),
the executor-side read path (result-cache clone made unrepresentable via
closure, sort/column-scan copies removed, `Record` on interned `Arc<str>`
names, `decompress` returning `Cow`, allocation budgets asserting 2/row in
both dynamic and declared schemas).

**Missing, roughly in order of what it costs:**

| gap | why it matters |
|---|---|
| **Thread-per-core (M28)** | Everything is one `Mutex` deep. On any modern core count this is the dominant ceiling, and the largest single item on the track. |
| **Zero-copy fetch, the literal remainder** | Owned records still materialize on the fetch loop. Two allocations per row is the floor *of the owned form*; the borrowed view over decoded pages is the design M27 named and nothing has replaced. |
| **Clustered sort order** | The only thing that makes a range scan sequential rather than random. |
| **Cost-model honesty** | Every estimate assumes indexed point lookups are flat in collection size; measured, they grow (6.3 µs at 100k rows, 12.4 µs at 800k). The bitmap-over-hash preference is a reasoned ordering that measurement contradicted at 1M rows and left in place. Both must be decided by benchmark. |
| **Prefix/delta compression** | Dictionary encoding landed; these two did not. |
| **io_uring (M29)** | Needs an async storage path first. Real, but last. |

## C — Automatically optimal · 70%

**Closed since the last revision:** concurrent experiments — per-experiment
candidate masks, per-experiment attribution, `candidate_visible` scoped to the
experiment being served, scope overlap refused with an explanation, two
experiments driven to simultaneous verdicts with answers asserted unchanged on
every query; composite index selection, built on new telemetry
(`FieldsPinnedTogether`) because per-field counts cannot recover co-occurrence,
with reachability tests so a registered optimization can never again be absent
from every level preset; covering-index selection, both shapes, with backing
kind chosen by the evidence; and **settled decisions are now re-examined** —
every cycle re-scores each enabled optimization under the *calibrated* cost
model and disables what no longer clears `KEEP_SCORE` (admission bar minus a
hysteresis margin), beside the writes-per-use arithmetic for indexes. The
diagnosis's standing counter-easurement is hereby retired: it described a
driver that could not correct a settled prior, and the comparison now shows
tuned aggregates winning by four orders of magnitude — the structure pays,
visibly, under outside witness.

**Missing:**

1. **Retraction arithmetic is thinner than cost-benefit.** Writes-per-use is
   a floor for "obviously losing", not a ledger; there is no weighing of an
   index worth keeping part-time, and the retraction window is a decay
   guess. The KEEP_SCORE loop corrects priors; it does not price upkeep
   against benefit continuously.
2. **Shadow-copy for non-derived changes (M30's other half).** Only additions
   of derived representations can be proved. Compression, freezing and layout
   changes are applied on a prior and never verified.
3. **Compound reasoning (M32).** Composite and covering selection exist;
   join-order reasoning and data-driven partitioning do not.
4. **The ceiling problem.** C is scored against "the expert adds nothing," and
   an expert with clustered order and thread-per-core available beats this
   optimizer because it has fewer moves. **Closing B raises C without touching
   C.**

## D — Good · 55%

**Closed since the last revision:** the comparison for one witness (4 of 8
wins, losses published, `docs/comparison-notes.md`), the SQL shell
(`adabt-cli`), an examples directory, and bearer-token auth on the server —
the first security pillar, with the gate before dispatch so an
unauthenticated connection cannot even count rows.
`comparison/` ran against bundled SQLite and the tables are published with
losses beside wins (`docs/comparison-notes.md`): aDaBt now wins **4 of 8**
workloads — point lookups on the resident directory, post-tuning aggregate
shapes at four orders of magnitude through column store plus materialized
views, and **top-20 sort at 1.9× over SQLite**, flipped from a 35–48× loss by
answering limit-over-sort from raw columnar cells and fetching only the
winners. The remaining losses are bulk load (13.7×), indexed equality (3.9×),
indexed range (1.9×) and single-row inserts (1.9×); the middle two are
fetch-path cost that proposed covering indexes would delete. The comparison
also found the planner replacing winning index lookups with losing column
scans; the precedence half of that is fixed and re-published.

**Missing:**

1. **More witnesses.** RocksDB and PostgreSQL are still absent (no
   `cmake`/`libclang`, no server, no root locally); CI can install both. One
   honest witness is evidence; three is a case.
2. **Hardening (M35).** No loom coverage of lock-free structures, no
   crash/chaos matrix, no consistency checker. WAL-truncation tests are good
   and are not the same thing. One wall-clock assertion
   (`restoring_is_faster_than_rebuilding`) has already flaked on a noisy
   machine — timing assertions belong in the bench harness, not the suite.
3. **~~No SQL shell.~~ Landed** (`adabt-cli`): opens a data directory, runs
   any SELECT the M37 parser accepts, `.explain` for plans, tables and
   indexes listed, values rendered exactly (decimals never pass through
   floating point on their way to the screen). Refusals stay refusals —
   writes error by name, because a shell that approximates is worse than
   one that declines.
4. **Security (M38): auth landed, the rest open.** The server accepts
   `--auth-token` / `ADABT_TOKEN`; without a token, every request on a
   connection is refused `Unauthorized` until Auth succeeds — including
   Ping, so no unauthenticated oracle — with per-connection state,
   constant-time comparison, and denial that neither closes the connection
   nor echoes the secret. Still missing: TLS (the token is only as private
   as its transport), roles and per-collection permissions.
5. **Ecosystem (M39).** ~~No examples directory~~ — landed (`quickstart`,
   `watch_it_optimize`). Still missing: a C ABI, bindings, semver and a
   format-compatibility promise — and the record encoding has changed twice
   in its life with no migration path.

---

# The road to 100%

All four finish lines, ordered by what unblocks what. The two standing
principles decide the sequence: **B gates C**, so B's cheap high-value items
come before more C work; and **evidence before optimization** — every remaining
decision should be made against an external reference rather than against this
engine's own past. A third joins them now that the plan extends to the end:
**decisions that invalidate later work come before the work they invalidate.**

### Stage 1 — Publish the comparison *(finishes D's spine)* — **DONE for SQLite**

Run and published: `docs/comparison-notes.md`. aDaBt wins **4 of 8** against
bundled SQLite — point lookups, post-tune count/group-by at four orders of
magnitude, and top-20 sort at 1.9× after the columnar top-K work — and loses
4, worst 13.7× (bulk load). The first run found the planner replacing winning
index lookups with losing column scans; the precedence half of that defect is
fixed and the comparison re-run as proof. Remaining for the full finish test:
RocksDB and PostgreSQL via CI images, plus YCSB- and TPC-C-shaped workloads
in the same harness.

### Stage 2 — B's cheap half *(re-ordered by Stage 1's evidence)*

In order:

1. **Cost-model honesty** — precedence half done; selectivity still open.
   The comparison found the planner replacing winning hash-index lookups
   with losing full column scans on equality and range (43 ms → 210 ms);
   the dead guard behind it is landed-and-fixed, verified by re-run — tuned
   serves through the index, aggregate wins untouched. Landed alongside it:
   **columnar top-K** (`docs/comparison-notes.md`), which took the worst
   loss on the board to a 1.9× win over SQLite and added a move to C's set.
   Still open: selectivity in the access decision (a predicate matching
   everything still wins columnar for its index), the flat-point-lookup
   assumption every estimate carries, and the bitmap-over-hash preference
   measurement contradicted at 1M rows.
   *Finish tests:* predicted-vs-actual within noise at 100k and 1M rows in
   the level matrix; bitmap-versus-hash settled by benchmark, not argument.
2. **Borrowed-view fetch path** — **started, measured slice landed.** The
   columnar projection was paying two allocations per *cell* for field
   names (`to_string` plus the `Arc` conversion) on top of the record's own
   vector; `ColumnStore::arcs` interns each name once per store and hands
   out refcount bumps. A columnar scan now sits at its floor — **1
   allocation per row**, asserted beside the heap budgets in
   `allocations.rs`. What remains of this item is the literal borrowed view:
   read paths seeing references into decoded pages instead of owned
   records, which needs the executor's row API to grow lifetimes.
   *Finish test:* a scan of N rows allocates O(1) per row — now asserted
   for both the heap path (2/row) and the columnar path (1/row).
3. **Automatic covering-index proposals** — **landed, both shapes**
   (`auto_covering_index`, level 5+). New telemetry pairs each filtered
   field with the projection it travels with (`FieldsProjectedTogether`,
   equality and range kept apart), and a stable, frequent pair builds the
   M25 structure through the ordinary `CreateIndex` path — hash-backed for
   equality evidence, b-tree-backed for range evidence, with the planner's
   covering matcher checking the backing kind so a hash-backed index can
   never serve a range it cannot walk (that failure mode is a silent empty
   answer, which is why it is checked rather than assumed). Verified
   end-to-end with controls: rotating projections are not evidence.
   *Measured:* indexed equality went 3.9× slower than SQLite to **~1.7–2.8×**
   through `CoveringLookup` with zero fetches; indexed range went 1.8–3× to
   **~1.5×** through `CoveringRange`. The residue in both is per-row
   projection-record construction — item 2's territory.
4. **Clustered sort order** — a collection may declare physical order;
   range scans over it become sequential. *Finish test:* page reads for a range
   scan drop to the pages the range physically occupies.
5. **Prefix/delta compression** alongside dictionary encoding. *Finish test:*
   stored bytes on sorted-key collections fall measurably further, decode cost
   within a stated bound of today's.

*Why here:* small relative to Stage 4, directly exploits Stage 1's evidence,
and each closure widens C's move set — the cheapest way to raise Track C is to
give it something new to choose.

### Stage 3 — Decide the scale question *(gates everything after it)* — **DECIDED**

**RAM-bound, as a documented property** — the decision and its numbers live
in `docs/scale-decision.md`. The directory pages to disk only if a
revisit trigger fires: a comparison-expressible workload whose resident set
fits but dataset does not, or thread-per-core landing and making residency
the dominant remaining ceiling. Until then Track A stands at roughly 95%
with 2PC, serializable and DDL documentation between it and done; sharding
is the growth story, which makes Stage 5 load-bearing rather than
ceremonial.

### Stage 4 — Thread-per-core, then io_uring *(B's expensive half)*

Core pinning, run-to-completion scheduling, per-core memory, shard-affine
connections, built on M15's partitioning. Then the async storage path and
io_uring beneath it. Soak-gated throughout — running this concurrency change
ungated repeats M15's mistake at larger N.

*Finish tests:* throughput scales near-linearly to the core count on the bench
machine with `--shards 1` as the control; syscalls per query counted and
dropped by io_uring, not asserted anecdotally; scan throughput compared against
measured memory bandwidth for the machine, with the gap stated rather than
hidden.

*Why after Stage 3:* per-core memory assumes a residency answer. Why after
Stage 1: the claim "this reaches the hardware's ceiling" is B's finish line,
and it is measured against outside engines, not against aDaBt's own past.

### Stage 5 — Cross-shard 2PC and serializable *(finishes A)*

A coordinator over the format that has been waiting since M19, and
serializable as a selectable guarantee mapped onto conflict detection over the
version chains that already exist. If Stage 3 answered "RAM-bound," sharding
is also the growth story, which makes this stage load-bearing rather than
ceremonial.

*Finish tests:* the differential runner runs at serializable; crash points
injected between prepare and commit recover to a consistent side, tested the
way single-shard recovery is — truncation at arbitrary offsets; a cross-shard
transaction is expressible through the engine API and the wire protocol.

### Stage 6 — Finish C

1. **Re-examine settled decisions** — **landed**: the cycle loop re-scores
   every enabled optimization under the calibrated model and enforces
   `KEEP_SCORE` (admission bar less a hysteresis margin), so a corrected
   prior now has consequences; writes-per-use gives indexes their cost side.
   The aggregate-phase standing measurement that motivated this is retired —
   see Track C above.
2. **Cost-benefit retraction** — write-upkeep weighed against read benefit;
   part-time structures representable. *Finish test:* a workload that uses a
   field seasonally ends with the structure present in season and gone out of
   it, without human input.
3. **Shadow-copy for non-derived changes** — prove compression, freezing and
   layout changes on identical queries the way derived additions are proved
   now. *Finish test:* every action the optimizer can propose has a proof path
   or a written reason it cannot.
4. **Join-order and data-driven partitioning proposals** — the rest of M32.

Then re-run the expert-vs-auto matrix: *finish test for the track* is adaptive
matching expert-chosen configuration within noise on every phase of the
standard workloads, with the expert given everything Stage 2 built.

### Stage 7 — Hardening *(makes the numbers trustworthy)*

Loom on the lock-free structures, a crash/chaos matrix around checkpoints,
experiment promotion and 2PC, a consistency checker runnable against a live or
recovered data directory, and the sweep of wall-clock assertions out of the
test suite into deterministic counters (allocation counts, page reads) — one
flake is already on record.

*Finish tests:* nightly CI runs the loom subset and chaos matrix; the checker
detects every seeded divergence in both directions (engine wrong, checker
silent is the failure mode that matters); no test in the default suite asserts
on elapsed time.

### Stage 8 — Surface and ecosystem *(finishes D)*

**Landed:** the SQL shell (`adabt-cli`), the examples directory
(`quickstart`, `watch_it_optimize`), and bearer-token authentication on the
server — gate before dispatch, per-connection state, constant-time compare,
refusals that neither close the connection nor echo the secret.

Remaining: TLS (the token is only as private as its transport), roles and
per-collection permissions, a C ABI with one real binding on top of it, and
semantic versioning with an on-disk format version and a migration path, so
the encoding stops changing without one.

*Finish tests:* a hostile-client suite runs against the exposed server;
a fresh clone reaches a working example in a stated number of commands;
a format-breaking change fails CI unless the version gate and migration land
with it.

### What 100% does not include

Stating it plainly so the number stays honest: **replication and multi-process
distribution.** They appear in no track's finish line above — A's is "you could
ship on it," not "you could run it ha." They remain the largest things undone
after the tracks close, and if the scale question in Stage 3 answers "not
RAM-bound," replication deserves revisiting sooner than this list implies.

### Dependencies, in one paragraph

Stage 1 informs everything and gates nothing. Stage 2 feeds Stage 4's
justification and Stage 6's move set. Stage 3 gates Stage 4's memory model and
shapes Stage 5's importance. Stage 4 gates Stage 6's ceiling claim — the expert
cannot be said to add nothing while holding cards the optimizer is not dealt.
Stage 7 makes every earlier finish test believable. Stage 8 is last because it
is labour, not risk — and because shipping a wider surface multiplies everything
that must then be hardened.
