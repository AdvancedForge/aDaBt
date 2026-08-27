# M14 — closing the loop, and four things that were missing

Five pieces, done in the order their risk demanded rather than the order they
were listed: a crash-safety check that turned out to be a bug hunt, the
experiment loop, a cache for derived representations, the network listener, and
the last two unimplemented optimizations.

Three bugs were found along the way, two of them capable of returning wrong
answers without raising an error. That ratio is the argument for doing the
prerequisite check first.

---

## 1. A crash during an optimization could silently corrupt records

The prerequisite. Every existing "crash" test dropped a store without
checkpointing, which is not a crash — `Drop` still runs and the log is still
flushed. `crates/adabt-storage/tests/crash_during_optimization.rs` truncates the
write-ahead log at twenty points across an operation, which is what a process
actually killed mid-write leaves behind.

The freeze failed on the first run:

```
record 9 came back wrong after a crash at log byte 15975
  left:  Record { fields: {"balance": I64(649754648416459267), "id": U64(7305804385234280967)} }
  right: Record { fields: {"balance": I64(333), "id": U64(9), "name": Str("customer-9")} }
```

That is tag-length-value bytes being read as a fixed layout. No error, no
checksum failure — just a record that came back as plausible nonsense.

**Why it happened.** Recovery has to apply a schema change *before* replaying the
writes that assume it, so `AlterSchema` was applied over the whole log while the
per-record re-encodes replayed only from the last checkpoint. A crash between
those two points left the new codec pointed at bytes the old one wrote.

**The fix is out-of-place migration.** `alter_schema` now builds the new encoding
beside the old one under a private collection name and adopts it in a single log
entry. Truncate the log anywhere before that entry and the original collection is
untouched; anywhere after it and the migration is complete. There is no
in-between state to recover from, so there is nothing for recovery to guess. The
cost is honest: the collection is stored twice until the flip.

The test now also asserts that its cut points *straddle* the change — one flip,
never a gradual slide — because a version of this test that only checked "nothing
is damaged" would pass just as happily against an implementation that never froze
anything.

Recompression and index creation survived unchanged: the compression encoding is
a per-slot byte, and an index is derived.

## 2. The experiment loop

`adabt-opt::experiment` had the state machine — phases, guardrails, the rule that
one divergence is fatal — and nothing drove it. `adabt-engine::experiment` is the
half that makes it real.

**The candidate is built where the planner cannot see it.** An index that exists
is an index the planner will use, so building one and leaving it visible would
end the experiment before it started: every query would take the new path, there
would be nothing to compare against, and the first wrong answer would be served
rather than caught. The structure is built and a single flag decides whether a
given query may know about it.

The mask lives on the `Database`, not on the experiment, and that placement is
load-bearing. A shadow trial moves the experiment aside to record into it; if the
mask went with it, both halves of the pair would take the candidate path and the
comparison would be of a thing against itself. `shadow_compares_two_genuinely_
different_paths` asserts a p50 ratio below 0.5 for exactly this reason — if
hiding ever breaks, the ratio sits at 1.0 and the test says so.

**Shadow and canary prove different things.** Shadow answers the same query both
ways against the same state, so a difference in *results* is attributable to the
change and nothing else; it is the only phase in which correctness can be
established, which is why no traffic moves until it passes. Canary sends a
fraction of real queries down the new path and returns what it finds — only one
path runs per query, so it cannot compare results at all. Its evidence is latency
under the cache state a real workload produces, which shadow cannot generate
because running both paths back to back perturbs the state each is being measured
on.

**Each phase earns its own promotion.** Measurements reset on entering a new
measuring phase. Carrying them forward would let shadow evidence satisfy the
minimum for every canary step in turn, and the ramp — the entire mechanism for
discovering that a change which looked good in a paired trial behaves differently
under real traffic — would run to completion without ever measuring real traffic.

**Routing is deterministic**, not random: the *n*th query goes to the candidate
iff `floor(n·p/100)` steps. An experiment that reached a different verdict on a
rerun would be untestable, and an even stride avoids the clumping that would let
a candidate measure a warmed cache.

**Trials bypass the plan cache**, and for a subtler reason than they bypass the
result cache. The plan cache is keyed by shape and holds an access *decision* —
but the decision depends on which structures are visible, and the two sides of an
experiment differ in exactly that while sharing a shape. A cached decision would
hand the baseline's plan to the candidate.

**Everything still goes through the controller.** A change built for a trial is
gated exactly as one applied outright — same guarantee filter, same constraints,
same log. An experiment that skipped the gates would turn "nothing bypasses the
controller" into "nothing except the interesting case".

**What cannot be experimented on, and why.** `Action::is_shadowable` admits only
changes that *add* a derived representation. Compression and schema freezing
rewrite the primary, after which the old path no longer exists to compare
against; cache and buffer-pool sizes are single global numbers with no second
value to hold at once. Asked to experiment with one, `begin_experiment` refuses
with the reason rather than accepting and measuring the database against itself.

`optimize_verified` is the driver-facing entry point: the optimizer's own
proposals go through the loop when they can, and are applied normally when they
cannot.

**Boundary.** A genuine result divergence cannot be planted inside a single
engine without fault injection that does not exist — every index the engine
builds is correct. That the verdict machinery reacts to a divergence is tested at
the unit level; that a *wrong* candidate is caught in shadow is tested in
`experiments.rs` with a deliberately broken pair across two databases.

## 3. Derived representations survive a restart

Indexes were rebuilt on every open by decoding every record in the heap.
`derived.adabt` is a cache of their contents, validated against a stamp
describing the primary, and used only on an exact match.

**The stamp is over-specified on purpose.** Reading a stale index produces wrong
answers, which is the one failure mode a cache of derived state can have that is
not merely slow. It carries the log position, the heap file's length, every
collection's live record count — and a database identity.

The identity was not in the first version, and the test that caught the omission
is still there. Two databases built by the same sequence of operations over
different values stamped identically: same log position, same heap length, same
row counts, completely different keys. Copying one directory's cache into the
other got it adopted, and every indexed query then returned the wrong rows in
silence.

Every failure to read is reported as "no cache" rather than as an error. A
damaged cache is not a damaged database, and the correct response is to rebuild
without comment.

Measured on 200,000 records with three indexes:

| | |
|---|---|
| Open with no indexes at all | 801 ms — the floor, which is recovery itself |
| Open, rebuilding three indexes | 1,617 ms |
| Open, restoring them from cache | 764 ms |

So the cache does not make opening faster than recovery allows; it removes the
index rebuild essentially entirely — 817 ms of it — and the 2.1× overall figure
is bounded by the page scan underneath. That page scan is now the thing worth
attacking, which was not obvious before measuring.

## 4. The server listens

Length-prefixed frames over TCP, a thread per connection, one engine behind one
mutex. A client is included, deliberately: a protocol with one implementation is
a protocol nobody has checked.

**What it is not.** There is no `io_uring`, no zero-copy, no per-core accept, and
no concurrency inside the engine at all — every request takes the same lock, so a
hundred connections are a hundred threads taking turns. The mutex makes that
correct, not fast. A benchmark run through this server measures the lock, which
is worth knowing before quoting one.

**The query body is a deliberate subset**: a collection, an optional single-field
comparison, a limit — not the logical IR. A wire format is a compatibility
promise and the IR is still changing; freezing the second inside the first would
make every future change to the query language a protocol break. The subset still
covers the shape that matters most, a filtered scan, which is what routes through
an index, a column store or a full scan depending on what the optimizer decided.

An error is carried home as `Error::Remote { status, message }` rather than
reconstructed into the original error. A status code narrows what went wrong but
does not determine it, and rebuilding the server's error from one would produce
something naming a cause nobody established.

## 5. The last two optimizations

`NOT_YET_IMPLEMENTED` is now empty.

### Prefetch

Sequential read-ahead in the buffer pool: two consecutive misses at adjacent
addresses is the shortest evidence of a scan that is evidence at all, after which
sixteen pages arrive in one read instead of sixteen. On a 160-page scan that is
under twenty reads instead of a hundred and sixty.

Two bugs, both caught by the tests written alongside it. The first let the pool
momentarily hold one page more than its capacity. The second was a policy
mistake: read-ahead was initially forbidden from evicting anything at all, which
sounds safe and means no read-ahead ever happens once the pool is warm — exactly
when a scan needs it. It now may evict, capped at a quarter of the pool per
batch. A scan can steadily push out its own trailing pages, which costs nothing
because they have already been read, but one speculative act can never displace
more than a quarter of what the pool holds.

Random access never triggers it, and a batch never overwrites a resident dirty
page with the stale copy on disk — nothing else in the pool would notice if it
did, because the write would simply be gone and the page would still checksum.

### Materialized views, and why only `COUNT`

A view holds grouped totals and updates them on write, turning an aggregate that
costs O(rows) into one that costs O(groups).

Only `COUNT` is materialized, and the reason is the interesting part.

A maintained aggregate is computed by different arithmetic than a scanned one.
The scan adds every value in record order; the view adds each value as it arrives
and subtracts it again on delete. For integers that makes no difference. For
`f64` it makes every difference: floating-point addition is not associative, so
`SUM` maintained incrementally and `SUM` recomputed by a scan disagree in the low
bits, and they disagree *more* the longer the view lives. A discrepancy in the
low bits of a sum is exactly the divergence this project refuses to tolerate —
optimization does not change answers, not by much, not in the last decimal place.

`MIN` and `MAX` are excluded for an unrelated reason: they cannot be maintained
under deletion at all. Removing the current minimum tells you nothing about the
new one without re-reading every remaining value, so a "maintained" min is a scan
in disguise.

A view is never invalidated and never recomputed, so an error in maintenance does
not wash out on the next read — it accumulates, and the number it returns stays
plausible the whole time. `materialized_views.rs` therefore runs 1,500 random
mutations against a second database with views off and compares after every
hundred, rather than checking a total once and trusting it.

**And a third bug.** Writing those tests exposed something older: `delete` told
the derived structures about a removal only when the *old record* had been read,
and the old record was read only when an index needed it. A collection with a
column store and no index went on aggregating rows that had been deleted. A
column store needs only the id to tombstone a row, so it is now told
unconditionally. The regression test is
`a_delete_reaches_the_column_store_even_with_no_index_to_maintain`.

---

## What is still not built

Unchanged from `m12-m13-notes.md`, and still deliberately unscaffolded:
per-core ownership, shared-nothing execution, kernel-bypass networking. The
mutex in `adabt-server` is where the first of those would show up, and the
module says so in its own documentation rather than in a roadmap.

Two smaller things this milestone chose rather than deferred by accident:

- **`SUM` in a materialized view** needs an accumulator that reproduces the
  scan's summation order. That is a different piece of work from this one, and
  approximating it would be worse than not having it.
- **A serialized query IR on the wire.** The subset is a promise the project can
  keep; the full IR is not one it should make yet.
