# M25 — index and layout library

The plan names six things: "Composite, covering, partial and bitmap indexes;
clustered sort order; prefix and delta compression; per-column dictionary."
A survey of what each would actually cost, done before writing anything,
found they are not remotely the same size — and this milestone ships the
subset that could be built with the correctness rigor the rest of this
project holds itself to, rather than six shallow versions.

## What shipped: bitmap indexes

`IndexKind::Bitmap`, a third kind alongside `Hash` and `BTree`, implementing
the same `Index` trait with the same equality semantics — same `lookup`,
same rows back, same ascending-id order — but storing one bit per record id
per distinct value instead of a `Vec<RecordId>` per key.

**Why it is worth having, concretely:** on a low-cardinality field with many
rows per value, `HashIndex`'s per-entry cost is 8 bytes of `RecordId` plus
its share of `Vec` growth slack; a bitmap's is one bit.
`a_bitmap_index_is_cheaper_than_hash_for_many_rows_sharing_few_values` is
the measured evidence at 5,000 rows over 4 distinct values — not an
assertion in a comment.

**`Bitset` is hand-written and grows on demand**, sized to the highest id
actually set rather than pre-sized to an assumed collection size, so a
bitmap over a sparse or small id range costs only what it spans. `ids()`
iterates set bits with `trailing_zeros` + `bits &= bits - 1`, which yields
ascending id order for free — a bitset's natural iteration order already is
id order, unlike a `HashMap`'s, so `Index::lookup`'s ordering promise needs
no sort to keep.

**`BitmapIndex` is not a `declare_index!` instance**, deliberately.
`Core<M: KeyMap>` — the shared bookkeeping `HashIndex` and `BTreeIndex` both
use — is built around a `Vec<RecordId>` per key: push, binary-search,
remove-by-position. A bitmap's per-key value is a `Bitset` with genuinely
different operations. Forcing both through one abstraction would have meant
an abstraction that describes neither honestly.

**Ranges are declined, not approximated.** `range()` returns `None`, exactly
as `HashIndex` does, for the same reason: answering one needs the keys
visited in sorted order, and a `HashMap`-backed structure has none.
`IndexKind::Bitmap.supports_range()` is `false`, so the planner never
proposes it for a range predicate.

**Planner preference, stated rather than incidental:** for an equality
predicate, `Hash` still wins outright; `Bitmap` is preferred over `BTree`;
`BTree` is the fallback. A b-tree asked for equality is a range structure
being used off-label, so between two purpose-built equality structures the
b-tree loses — and between hash and bitmap, hash wins because materializing
a bitmap's matches costs a scan of its word range rather than just of the
matches.

**Never chosen automatically.** `adabt-opt`'s `auto_index` still only ever
proposes `Hash` or `BTree` — `index_kind_for` is unchanged. `Bitmap` is
reachable only by naming it: `Database::create_index`, or an M24 manual
policy override carrying `kind = IndexKind::Bitmap.as_ordinal()`. Teaching
the automatic heuristic the low-cardinality signal (which
`OptContext::density_of` already provides) is real, separable work and is
not claimed here.

**The invariant holds.** `indexes_never_change_query_results` in
`engine.rs` — this project's central property test, which runs a query set
against every index configuration and asserts identical results — gained two
`Bitmap` configurations. It passes.

## What was already done: per-column dictionary

`ColumnStore` (`crates/adabt-engine/src/column.rs`) already implements
dictionary encoding for low-cardinality text columns — `Column::Dict{dict,
codes}`, first-seen-string interning, with its own passing test
(`dictionary_encoding_collapses_a_low_cardinality_column`). This is one of
the six named features and it did not need building. It is confirmed end to
end through the public API in
`column_store_dictionary_encoding_is_already_the_per_column_dictionary_capability`
rather than reimplemented.

## What was deferred, and the honest reason for each

**Composite indexes.** `field: String` — singular — is baked into
`Index::field()` itself, `AccessDecision::IndexLookup`/`IndexRange`,
`PhysicalOp::IndexLookup`/`IndexRange`, `OptContext::existing_indexes`,
`OptScope::Field`, and the planner's own predicate matching
(`equality_constraints`, `range_fields`, `range_constraint`) which walks an
`Expr` looking for *one* field's constant. A composite index needs a
multi-field key type, plural variants of four public enums across three
crates, and — the actually hard part — planner logic that recognizes when
an `And` predicate covers every column of a composite key in order. That is
a multi-crate redesign, not an addition.

**Covering indexes.** Every `Index` impl stores only `Vec<RecordId>` per key,
and `exec.rs`'s `fetch_batches` unconditionally fetches from the heap for
every id an index returns. Covering needs both a structure that stores
column values alongside ids *and* an executor path that skips the fetch when
the projection is a subset of what the index carries — two new pieces, and
the second one interacts with the join work M23 just landed.

**Partial indexes.** No filter hook exists anywhere: `index_record` indexes
every row handed to it, and `create_index_from` takes no predicate. The
additive part (store a predicate, skip non-matching rows when building) is
small; the correctness-critical part is planner-side — an index over
`status = 'active'` may only serve a query whose own filter *provably
implies* that predicate, which is implication checking over `Expr`, and
getting it wrong silently drops rows. That is exactly the class of bug this
project's soak and differential rigs exist to catch, and it deserves its own
pass rather than being tacked on.

**Clustered sort order.** Confirmed absent, twice (the M24 survey and this
one): no representation anywhere stores records ordered by a field's value —
`ColumnStore` keeps heap-scan order, `DirectArray` addresses by id
arithmetic. Clustering is an entirely new physical representation, the same
size of undertaking `DirectArray` and `ColumnStore` each were: a new
structure, a new derived-representation lifecycle, a new `Optimization`
impl, and its own place in the experiment loop.

**Prefix and delta compression.** `compress.rs` is whole-record LZ4 —
generic and field-unaware. Prefix and delta encoding are per-column,
order-sensitive techniques that belong to a columnar or clustered layout;
prefix compression in particular pays on *sorted* data, which is the
clustered representation that does not exist yet. Building them before
there is an ordered layout to apply them to would be building them against
nothing.

**A note the codebase should carry:** the "one external dependency,
`thiserror`" framing that appears in older docs is out of date —
`compress.rs` uses `lz4_flex`. Recorded here rather than left to be
rediscovered.

---

# M26 — query compilation

## What shipped

**A bytecode VM for predicate evaluation** (`crates/adabt-ir/src/vm.rs`),
live on the executor's `Filter` path. `Expr::evaluate` walks a `Box`ed tree
once per row and pays three costs per row that do not depend on the row:
recursive descent, a `match` on node kind at every step, and — for `Like` —
re-parsing the pattern string from scratch on every single call, because
`like_matches` calls `parse_like_pattern` internally. `Program::compile`
moves all three to compile time: the tree is walked once, patterns are
parsed once, field names are interned, and evaluation becomes a loop over a
flat `Vec<Op>`.

**Two operand stacks, not one tagged stack.** Which stack an instruction
touches is fixed by the instruction, so the split is known at compile time
and costs no per-step discriminant check.

**Short-circuiting is preserved, via jumps.** `And` folds left with a
`JumpIfFalse` after each step and `Or` with `JumpIfTrue`. This is correct
precisely because `False` absorbs `And` and `True` absorbs `Or` in
three-valued logic, so once the accumulator reaches the absorbing element it
*is* the answer and the remaining operands cannot change it. Dropping
short-circuit would have been simpler and still correct, but this is a
performance milestone and `And(cheap_false, expensive)` is exactly the shape
that would have regressed.

## How the substitution is made safe

The plan was specific about this: *"Scope must include running every query
shape through both executors — the differential runner compares two stores,
not two executors."*

`mod differential` in `vm.rs` does exactly that, one layer down: a seeded
xorshift generator builds 4,000 random expression/record pairs — nested
`And`/`Or`, arithmetic that can overflow, `In` lists mixing present and
missing fields, `Like` patterns with escapes, explicit nulls, absent fields
— and asserts `Program::run` and `Expr::evaluate` return the *identical*
`Truth` on every one. Three-valued logic is where a reimplementation
quietly diverges, and a hand-written suite only covers the cases its author
already thought of.

A second test asserts the machine's **stack discipline** rather than only
its answers: after walking every instruction, exactly one truth and zero
values remain. A stack machine that under- or over-flows would still often
return a plausible answer, because `run` pops with a default — so checking
the answer alone would not catch it.

## Honest limits

**Not a JIT.** No machine code is generated. The honest description of the
win is that it removes per-row interpretive overhead that was never
row-dependent, not that it approaches native speed. `compiled.rs` in the
engine already makes the same distinction for the same reason.

**Predicates only.** Sort keys, aggregates and projections still run
interpreted. `Filter` was chosen because it is the operator that runs a
per-row expression over the largest number of rows; the others either run
once per group or do no expression evaluation at all.

**Cranelift was not attempted**, as the plan's own "optional ... after"
allows. It would add a large external dependency to a project that has two,
and the portable VM is the part the plan called for first.

---

# M27 — zero-copy read path

## What shipped

**The per-row filter loop no longer copies anything.** `vm::Program::run`'s
value stack became `Vec<Option<Cow<'a, Value>>>` with
`run<'a>(&'a self, rec: &'a Record)`: a literal borrows from the compiled
program, a field value borrows from the record, and the overwhelmingly
common shape — load a field, compare it against a literal, discard both —
now allocates nothing at all. Only `Arith`, which genuinely produces a value
that did not previously exist, yields `Cow::Owned`.

Before this, every `LoadField` did `rec.get(name).cloned()` and every
`PushVal` did `v.clone()`. For a `Value::Str` that is a heap allocation and
a memcpy **per row, per field reference** — on the innermost loop in the
engine.

**Why this refactor was safe to make quickly:** M26's differential test.
Swapping the representation underneath an evaluator is exactly the kind of
change that silently breaks an edge case, and the 4,000-pair generated
comparison against `Expr::evaluate` re-ran unchanged and still passes. The
test built in the previous milestone is what made this one cheap.

## What was deferred, and why

**The full API-level change — `LogicalStore` returning borrowed record
views instead of owned `Record`s — was not attempted.** The plan itself
flags it as "a large API change," and the specific obstacle is concrete:
`LogicalStore`'s reads take `&mut self` (documented in `store.rs` as
deliberate — the buffer pool faults pages in, counters move), so a returned
borrow would hold a mutable borrow of the store for the lifetime of the
row, making it impossible to read a second record while holding the first.
Resolving that means either a snapshot/guard type that owns the pinned
pages, or interior mutability in the pager — both real designs, neither a
mechanical refactor, and both touching every caller in the workspace.

Doing it badly would mean either a fake "zero-copy" that clones behind the
scenes, or destabilising every store implementation at once. The contained
win above is real and measurable; the API change is honestly still open.

---

# M28 and M29 — thread-per-core and io_uring: not attempted

These two are assessed rather than implemented, because a partial version
of either would be worse than none.

**M28 (thread-per-core)** needs core pinning, a run-to-completion
scheduler, per-core memory affinity, and shard-affine connections. The
foundation the plan names — shared-nothing partitioning — genuinely exists
(`ShardedDatabase`, M15). But the current execution model spawns *scoped
threads per query* in `broadcast`; thread-per-core means long-lived threads
pinned to cores with work handed to them, which is a different concurrency
architecture, not a setting. Pinning the existing per-query threads would
add syscalls without changing the model and would measure as noise.

**M29 (io_uring)** is Linux-specific and needs either a substantial
external dependency or hand-written submission/completion-queue handling
over raw syscalls. This project has two external dependencies total
(`thiserror`, `lz4_flex`); adding a third of that weight is a decision worth
making deliberately rather than in passing. It also only pays once the
storage path is asynchronous, which it is not.

**The honest status**, so nothing downstream reads these as done: the
partitioning M15 built is real and the `--shards` flag measures it; the
per-core and async-I/O work on top of it is not started. `server.rs`'s
module docs already say exactly this, and remain accurate.

---

# Track C — automatic optimality

## M31 — cost-benefit retraction

**The gap:** telemetry measured only the *benefit* of an index
(`Event::IndexUsed`, "the planner chose it"). Retraction was therefore a
boolean — retract at *zero* uses, keep otherwise — which cannot see an index
the planner still picks occasionally while the write path pays thousands of
maintenance writes for it.

**The prerequisite the plan named, built:** `Event::IndexMaintained`,
recorded in `reindex_insert`/`reindex_remove` — the exact point the cost is
actually paid, so it counts real work rather than estimating it. Collected
per `(collection, field)` and **decayed alongside `index_usage`**, which
matters: comparing a decayed benefit against an undecayed cost would make
every index look worse the longer it existed.

**Retraction is now arithmetic against the policy's own weights.** Writes
per planner use is compared against a tolerance derived from
`Priorities` — `speed`-heavy policies tolerate an expensive index,
`resources`-heavy ones give up sooner. That preference is exactly what
`Priorities` exists to express, so no new tunable was invented. The soak's
decision log now reads *"has not chosen users.country in the last 1997
operations, while paying 11 index writes to keep it"* — the cost half is
visible where before there was nothing.

## M33 — workload memory

`Fingerprint` describes a workload's *shape*: bucketed read/write mix,
bucketed shape concentration, and the set of hot filtered fields. Bucketed
deliberately — 71% reads and 73% reads are the same workload for every
decision the optimizer can make, and a fingerprint that distinguished them
would recall nothing, ever. `similarity` is a graded match rather than
equality, for the same reason.

`WorkloadMemory::recall` returns a configuration that worked for a
sufficiently similar shape, with the similarity that justified it so the
reason reaches the decision log. **Recall is a suggestion, never an
application**: nothing here bypasses the controller's gates. That is the
same rule the manual driver already follows — "nothing a human can ask for
bypasses the machinery the optimizer uses" — applied to memory. A workload
can look identical while the data has grown tenfold.

## M34 — joint search

**Why greedy is provably wrong here**, in three ways this codebase already
declares: `conflicts_with` (greedy takes the higher scorer and never asks
whether the other plus a third would beat it), `prerequisites` (a dependent
optimization looks worthless scored alone), and the shared `max_ram_bytes`
budget (two cheap wins can beat one expensive one that greedy takes first,
leaving no room).

`best_combination` enumerates combinations rather than ranking individuals,
checking coherence and the shared budget against the *combination*.
Exhaustive over a small candidate set rather than heuristic over a large one:
with ten built-in optimizations, exhaustive is exact and trivially cheap,
while a heuristic would be unverifiable guesswork layered on estimates that
are already rough. Past `MAX_EXHAUSTIVE` it declines rather than silently
taking exponential time on a cycle meant to be cheap.
`two_cheap_wins_beat_one_expensive_one_under_a_shared_budget` is the
concrete case greedy gets wrong — 5.0 available, greedy reaches 4.0.

Like recall, search returns a *proposal*; the controller's gates are
unchanged. Search changes which combination is proposed, never what is
allowed.

## M30 and M32 — not attempted

**M30 (concurrent and full-coverage experiments)** needs a shadow-copy
mechanism so non-derived changes — compression, freezing, layout — can be
proved, where today only derived-representation *additions* can be. The plan
itself makes it soak-gated, and for good reason: running N experiments
concurrently without that gate repeats M15's mistake at larger N. That
gating work is the milestone, and it is not started.

**M32 (compound and cross-object reasoning)** is largely blocked upstream.
Composite index selection needs composite indexes, which M25 deferred with
reasons; join-order reasoning needs multi-way joins, which M23 deferred
explicitly (one join per query, refused with a clear error). Building the
*reasoning* over capabilities that do not exist would be scoring options the
engine cannot execute.

Both are recorded here rather than quietly skipped, so nothing downstream
reads Track C as complete.

---

# Wiring M33/M34, and M30 — partial

## M33/M34 are now actually reachable

They were not. Both modules shipped fully unit-tested and **called from
nowhere** — the same shipped-but-unreachable shape an audit had just caught
for `set_log_archive`. A module with green tests and no call site is not a
feature, and writing them up as done was wrong.

Now, in `AdaptiveDriver::decide`:

- **Recall reorders, never admits.** Every candidate has already cleared
  `MIN_SCORE` on its own merits; a remembered configuration only decides
  which of those to try *first*. Lowering a bar on the strength of memory
  would let a stale configuration reapply itself to a workload that has
  moved on. The trigger records why — *"tried first — a workload 91% like
  this one used it"* — so a recalled decision is explainable rather than
  unexplained.
- **Joint search replaces greedy top-N**, so conflicts, prerequisites and the
  shared memory budget are judged against the combination being proposed. It
  falls back to greedy order when no combination is feasible, rather than
  treating "no combination" as "propose nothing" — which would silently
  disable the adaptive path entirely.

**A flaw the wiring tests found:** the first version treated "no change
proposed" as "settled" and remembered the configuration. But a driver is
routinely quiet because candidates are in *cooldown*, or because a change it
already made is still awaiting measurement — both mean mid-decision.
Remembering then teaches the memory a waypoint as a destination. Settled now
means quiet **and** with nothing pending.

## M30 — the masking defect fixed, concurrency still blocked

Attempting M30 surfaced a **live bug**, not merely a missing feature.

`Candidates` held `column_store: bool` and `direct: bool` — *global* flags —
while `Action::SetColumnStore`/`SetDirectLookup` are engine-wide actions
carrying no collection. So an experiment trialling a column store for
`users` masked the column store for **every** collection, including ones
whose column store had been promoted long before. Two consequences, both
real: unrelated queries silently lost an optimization for the experiment's
duration, and the baseline those queries were measured against moved while
an experiment elsewhere was running — contaminating exactly the measurement
the experiment machinery exists to keep clean.

Both are now per-collection lists scoped to the experiment's own collection,
with `a_candidate_on_one_collection_does_not_mask_another_collections_structures`
as the regression test. An experiment with no collection records nothing
rather than masking everything — the safe direction, since masking the wrong
structure takes the "baseline" reading through the candidate itself.

**Concurrency itself is still not done, and here is precisely what blocks
it.** `retire_experiment` does `self.hidden = Candidates::default()` —
clearing the *whole* mask. With two experiments live, retiring one would
unmask the other's candidate mid-flight, exposing an unproven structure to
real traffic. Fixing that means attributing every masked entry to the
experiment that built it, which is a change to the mechanism that guarantees
"a candidate is invisible until proven" — the core of this project's safety
story. The plan's own note applies exactly here: *"Soak-gated: running N
experiments concurrently without it repeats M15's mistake with a larger N."*

Per-collection masking was the necessary precondition and is done. The
attribution change, and the shadow-copy mechanism for non-derived changes
(the other half of M30), are not.
