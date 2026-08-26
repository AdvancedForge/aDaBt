# Where the four tracks actually stand

Scored against the finish lines the plan itself defines, not against milestone
count. A milestone can be "done" and leave its track well short.

Baseline for comparison is `docs/diagnosis.md`, which scored the same four axes
after M15. The middle column is where this file left them at its last revision;
`docs/roadmap-notes.md` records what closed in between.

| track | baseline | last revision | **now** | finish line |
|---|---:|---:|---:|---|
| **A — usable** | 60% | 90% | **95%** | you could ship on it |
| **B — manually optimal** | 70% | 55% | **70%** | the engine's ceiling is the hardware's ceiling |
| **C — automatically optimal** | 40% | 65% | **80%** | the expert adds nothing |
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
- **B 60 → 70**: cost model wired into planning (`row_counts` + `scan_wins_over_lookups`) and proposals landed for clustered sort, delta and thread-per-core (all level-visible) — the cheapest way to raise C is still to give it moves, and B's library grew by three.
- **C 75 → 80**: clustered sort now proposes from range-filter telemetry, closing the last of Stage 2's cheap half that B's earlier closures had left open.

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
| **Zero-copy fetch, the literal remainder** | The executor integration **landed**: `Source::peek_field` (tri-state — row gone / field absent / value) with a defaulted fetch-and-project fallback, and a fused filter that decides single-field predicates over a heap scan from peeked fields, fetching full rows only for survivors. Decode cost now tracks selectivity, not table size. The borrowed-batch primitives are landed too: `codec::ValueRef<'a>` + `decode_value_ref` (borrowed text/bytes, pointer-identity proven) with allocation-free equality against owned values, and the single-field read path now runs end to end — `LogicalStore::peek_field` (default: fetch-and-discard; heap override: the codec's one-field walk, so a wide record's other text never decodes or allocates), wired through `Database::peek_field`'s fallback and pinned against full-fetch equivalence on both schema modes in `peek_path.rs`. What remains is threading lifetimes through `Source::fetch` itself so multi-field survivor fetches skip unneeded fields too. |
| **Clustered sort order** | **Optimizer proposal landed**: `ClusteredSortOpt` (`clustered_sort`, per-field, level 5) proposes `SetClusterField` from `most_range_filtered_fields` (telemetry `field_filters` vs `equality_filters`) when a field's range count ≥10 and collection ≥5k rows; `SetClusterField`/`ClearClusterField` wired via `Database::declare_cluster_field` (`crates/adabt-opt/src/action.rs:44`), shadowable, reversible. Mechanism and persistence already landed (`WalOp::SetClusterField`). |
| **Cost-model honesty** | The bitmap-over-hash preference **is decided by benchmark now**: `index-scale` measured the two tying on latency at every scale (100k–1M rows, ~6% memory for bitmap on low-cardinality fields), and with per-field key counts available O(1) from each index, the tie goes to bitmap when cardinality proves the field small — planner and executor apply the same rule from one shared constant (`LOW_CARDINALITY_KEY_COUNT`), asserted end to end in `bitmap_choice.rs` and at both creation orders. The flat-point-lookup assumption is **calibrated and wired into planning**: `adabt_exec::cost::point_lookup_ns` encodes the measured log-linear curve (6.3 µs at 100k, +2 µs/doubling, flat below) with both rungs pinned; `PlanContext::row_counts` (live count via `HeapStore::live_count`) lets the planner let a full scan win when an equality would match >1/3 of a large collection, asserted at 800k rows — consumers inherit corrections by re-anchoring one module. |
| **Prefix/delta compression** | **Dictionary + delta automatic landed** (`Column::Delta` block directory); **optimizer proposal landed** as `delta_encoding` (global, level 4, `SetDeltaEncoding`) — storage flag wiring next (currently no-op, automatic still does the work). |
| **Thread-per-core (M28)** | **Proposal landed** as `thread_per_core` (global, level 9, `SetThreadPerCore`); shared-nothing sharding is per-shard `Mutex` (`crates/adabt-engine/src/sharded.rs:19`), action exists and is level-visible — full pinning, per-core memory and run-to-completion remain M28. |
| **io_uring (M29)** | **Decided by measurement now**: the connection-scale bench (`connection_scale.rs`, `#[ignore]`, run explicitly) shows no saturation cliff through 512 concurrent clients — aggregate ping throughput peaks ~116k req/s at 16 connections and still holds ~72k at 512, a gentle decline, not the collapse that would justify an event loop. The gate for revisiting is written into the bench itself: it *fails* if any rung drops below half the previous one. Until real deployments cross that line, thread-per-connection stands. |

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
   nor echoes the secret. **Landed since:** TLS (handshake before any
   protocol byte, tested end to end) and per-collection floors — a
   collection can require admin even for reads, with `Forbidden` for known
   but disallowed callers (`grants.rs`).
   **Landed since:** TLS — `--tls-cert`/`--tls-key` wrap every accepted
   connection in rustls before any protocol byte is read (tested end to end
   over real handshakes: queries survive intact, plaintext clients get a
   TLS alert rather than a half-spoken protocol, half-configured TLS is a
   startup error).
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
   **Landed since:** planner now consults the calibrated curve (`adabt_exec::cost`)
   via `PlanContext::row_counts` — an unselective index on a large collection
   loses to a full scan (`scan_wins_over_lookups`), asserted at 800k rows.
   Still open: the same curve in the adaptive optimizer's estimates.
   Plus the flat-point-lookup assumption, calibrated —
   `point_lookup_ns` encodes the scale ladder's measured curve with both
   rungs pinned by test. And the bitmap-over-hash question, reopened by the
   cardinality signal exactly as its own comment predicted and settled by
   measurement — low-cardinality fields serve through their bitmap (same
   latency, ~6% memory), everything else keeps hash-first; one shared
   constant drives planner and executor alike (`bitmap_choice.rs`).
   Plus selectivity in the access decision — `PlanContext`
   now carries per-field cardinality read from each index's own key count
   (O(1), and defined only for fields that are indexed, which is the right
   boundary), and among equality candidates the planner probes the most
   selective field's index; absent estimates preserve the shipped
   first-wins order, asserted by test.
   *Finish tests:* predicted-vs-actual within noise at 100k and 1M rows in
   the level matrix; bitmap-versus-hash settled by benchmark, not argument.
2. **Borrowed-view fetch path** — **started, two measured slices landed.**
   The columnar projection was paying two allocations per *cell* for field
   names (`to_string` plus the `Arc` conversion) on top of the record's own
   vector; `ColumnStore::arcs` interns each name once per store and hands
   out refcount bumps. A columnar scan now sits at its floor — **1
   allocation per row**, asserted beside the heap budgets in
   `allocations.rs`. And the executor now has the peek seam: single-field
   predicates over a heap scan are decided from `Source::peek_field` (an
   address calculation on fixed-schema collections) with only survivors
   fetched in full — decode cost tracks selectivity, not table size
   (`borrowed_filter.rs`, plus decode-counting tests in the executor).
   The literal borrowed view now has its primitive: `codec::ValueRef`
   decodes borrowed (equivalence and pointer-identity proven in codec's
   tests); the remaining work is threading it through the executor's row API.
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
4. **Clustered sort order** — **landed**: a collection may
   declare a clustering field (`Database::declare_cluster_field`); integer
   keys steer *placement*, so records with nearby values land on the same
   pages and an index range over that field touches pages in proportion to
   the range, not the collection — the finish test passes on identical data
   differing only in the hint (10%-of-domain band: 31–46 of 200 pages vs all
   of them unclustered). The placement policy went through two refuted
   designs before landing (nearest-at-any-distance lets wide pages hoard
   everything; strict containment refuses to fill gaps) — the working rule is
   *nearest within twice one page's share of the observed domain*, which is
   self-limiting by construction; see `place_keyed`. Answers never change;
   clustering is placement, not content. The declaration persists across
   restarts (catalog + `WalOp::SetClusterField`, tested end to end, clear
   persists too); placement ranges re-derive from new keyed inserts. *Still
   open:* optimizer proposals, reclustering existing data.
5. **Prefix/delta compression** — **landed**: integer columns whose values
   arrive non-decreasing convert to delta-zigzag-varint encoding at
   power-of-two length checkpoints (`Column::Delta`, geometrically spaced so
   oscillating streams pay one failed attempt per doubling; a descending
   value demotes back to plain losslessly). Sorted 8k-row column: ~3× less
   memory than the plain form; random data never converts. Random access is
   bounded: a block-level byte directory means any row decodes in at most
   `DELTA_BLOCK_ROWS` varint reads. Answers identical to the plain form,
   asserted row-by-row including nulls and demotion. *Still open:* exposing
   the conversion to the optimizer as a proposal rather than an automatic
   policy.

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

**Both halves are landed.**
Serializable —
`Consistency::Strict`, declared since the guarantees existed and enforced
nowhere, now means it: commit validates the read set with the same
first-committer-wins rule as the write set, closing write skew
(`serializable.rs` runs the same interleaving under both settings — both
commits under Snapshot, second refused under Strict; innocent workloads pay
nothing). And the coordinator exists: `ShardedDatabase::commit_coordinated`
runs the coordinator-decides protocol over a fsynced journal of the whole
write-set (`CrossShardWrite`, riding the WAL's own value TLV), applies
shard-by-shard with put-overwrite semantics so replay over any partially-
applied prefix converges, and `open` re-drives anything left pending before
answering a query. `cross_shard_atomic.rs` stages every crash point by hand
— journal-only, mid-application, torn journal tail — and holds all of them
to one final state. The honest boundary, documented at the API: another
connection can observe shards disagreeing inside the window; hiding that
needs distributed locking. If Stage 3 answered "RAM-bound," sharding
is also the growth story, which makes this stage load-bearing rather than
ceremonial.

*Finish tests:* the differential runner runs at serializable; crash points
injected between prepare and commit recover to a consistent side, tested the
way single-shard recovery is — truncation at arbitrary offsets. **The
single-shard slice of this is landed** (`commit_window_chaos.rs`): a
multi-write transaction cut at 17 offsets through its own commit frames
recovers, at every one, to exactly a *prefix of the sorted write-set* —
never an interleaving, never a torn row — with survivors byte-exact,
`verify()` clean, and reopening idempotent. What remains for the full item:
the coordinator (prepare/commit across shards) and the in-doubt recovery
outcome it introduces.
a cross-shard transaction is expressible through the engine API and the wire protocol.

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

**Landed:** the deterministic sweep, the consistency checker, the loom
subset, and restart chaos around experiment promotion.
`Database::verify()` walks heap against every derived structure — forward
(record → index), reverse (index → heap, catching dangling ids), columnar id
sets — with `cfg(test)` fault-injection seams proving it detects seeded
divergence in both directions. The crash/chaos matrix (`crash_consistency.rs`)
truncates the WAL's active segment at 13 byte offsets through an uncheckpointed
write wave and demands, at every point: clean open or clean refusal, exact
survivors, `verify()` empty, idempotent reopen. The lock-free transaction-id
allocator is model-checked under `--features loom` (two writers, all
interleavings: distinct ids from one, `current` equals the highest issued
after join); `promotion_chaos.rs` kills and reopens at Building and Shadow,
demanding identical answers, empty `verify()`, and a non-wedged engine each
time — a lost trial loses only the trial.

Remaining: chaos around 2PC commit windows (the two-phase path itself is
transactional tests' territory; what is untested is a crash between its
phases).

*Finish tests:* nightly CI runs the loom subset and chaos matrix; the checker
detects every seeded divergence in both directions; no test in the default
suite asserts on elapsed time.

### Stage 8 — Surface and ecosystem *(finishes D)*

**Landed:** the SQL shell (`adabt-cli`), the examples directory
(`quickstart`, `watch_it_optimize`), and bearer-token authentication on the
server — gate before dispatch, per-connection state, constant-time compare,
refusals that neither close the connection nor echo the secret.

~~Remaining: per-collection permission grants~~ — **landed**:
`with_collection_floor(collection, role)` walls a collection off from
lower-role connections, every request kind that names it, reads included —
the one thing the connection-level role model cannot say. Checked after
authentication and role authorization, before the engine is touched;
a known-but-disallowed caller gets `Forbidden`, wire garbage gets the
`BadRequest`/`Internal` verdict dispatch always gave it. Socket-tested in
`grants.rs`. **Landed since:** TLS —
rustls on the accepted connection, before any protocol byte, generic over
the stream type so plaintext and encrypted paths share one framing
implementation (`tests/tls.rs` pins handshake-first, round-trip, and
refusal shapes). Roles landed earlier — an admin token
plus an optional read-only credential (`--read-token`), enforced after
authentication with a new `Forbidden` status that tells a known caller the
truth (re-authenticating cannot help); socket-tested in `roles.rs`. The **C
ABI** (`crates/adabt-ffi`, cdylib + `include/adabt.h`): open/close,
create-collection, i64 field put/get, count — nothing that is not ABI-stable
crosses the boundary; the contract tests call through C-typed declarations,
and a real binding exists: a C program compiled by `cc` against the header
and linked to the built `.so` runs end to end (`c_binding.rs`). And the
**version gate**: the superblock's single format number refuses a newer
database outright, migrations are enumerated in code (`migrate`, proven by
the legacy-identity adoption), and the one independently versioned file —
the catalog — is exactly the one whose loss is recoverable: an unreadable
catalog rebuilds from the log with every record intact (tested end to end).
That is semantic versioning for the disk: refuse forward, enumerate backward.

*Finish tests:* ~~a hostile-client suite runs against the exposed server~~ —
**landed** (`adversarial.rs`, 8 tests over real sockets): garbage bytes,
impossible length prefixes, bad magic, wrong version, truncated headers,
unknown request kinds, malformed bodies, and a concurrent vandal — each
proves the same three-part contract: the misbehaving connection is closed
or refused without a half-answer, the server neither crashes nor wedges,
and every other connection carries on. The client gained `send_raw` /
`next_reply` so conformance and fuzz tooling can speak frames the typed
surface would never construct.
A fresh clone reaches a working example in a stated number of commands;
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
