# M16 — foundations

Four small things that everything in Track A depends on, plus one finding.

## The superblock, and why the caches had version numbers and the data did not

`directory.adabt` and `derived.adabt` — both *discardable caches* — each carried a
magic number and a format version. The heap and the write-ahead log — the two
*authoritative* files — carried neither. A cache that cannot be read costs a
rebuild; a heap that is misread costs the data.

`superblock.adabt` now holds the format version, the database identity and the
page size, and is read before a single page. A newer version is refused outright
rather than read optimistically, because the failure mode of optimism here is not
a crash but a plausible record with the wrong bytes in it. It also absorbs the old
`identity.adabt`, adopting its value on first open — the smallest possible
migration, kept because it proves the mechanism works on something real.

## The catalog, and why the log could not be truncated without it

`register_collection` assigned `CollectionId(next_collection_id)` in **log replay
order**, and `scan_pages` reads that id back out of the first four bytes of every
heap slot. So the name-to-id binding was physically embedded in every page and
logically derived from replaying the log from byte zero.

Truncate the log and the surviving `CreateCollection` entries renumber. Every page
is then attributed to a different collection than the one that wrote it — or to
none, in which case it is silently treated as an orphan. Nothing notices: the
pages are intact, the checksums pass, the records decode. They belong to the wrong
table.

`catalog.adabt` writes the binding down. It is authoritative, so unlike the caches
a failure to write it is propagated rather than swallowed, and it carries
`log_start_lsn` — zero while the log is complete, non-zero once truncation exists.
While it is zero a missing catalog is recoverable by walking the log, and
`the_catalog_and_a_full_log_walk_agree` is the test that keeps the two answers
identical for as long as both can answer.

## A change must go on justifying itself by the standard that admitted it

Admission required a score above `+0.5`; retraction required one below `-0.5`.
Between them sat a band in which a change could never be reconsidered, and
calibration walks changes straight into it — correcting an optimistic prior pulls
a score *toward* zero, not below it.

The bar to keep is now the bar to admit less a hysteresis margin. The margin is
what stops the two rules arguing, which is the M8 failure this project already
found once.

## The finding: one observation is not enough, and there is no way to get more

The test written for the fix failed at first, and the reason is worth recording.
Across twenty cycles the model had accumulated exactly **one** measurement of the
change, because `learn` drops a pending change after observing it once and never
re-arms. With `FULL_TRUST_AT = 8`, one observation moves a calibrated estimate
about an eighth of the way, which is not enough to cross any threshold.

The obvious repair — re-arm the observation and keep measuring — is wrong, and
wrong in an instructive way. `Observation::between` measures a delta across a
window. The first window is before-and-after the change; every later one is
after-and-after, in which the change's effect is baked into *both* endpoints. Its
measured effect would come out as ~1.0, and the model would learn that every
optimization does nothing, including the good ones.

There is no sound way to re-measure an applied change without turning it off
again. That is the experiment loop covering changes which are not
derived-representation additions, and it is M30. Until then the dead-band fix at
least lets the one correction that does exist have consequences.

**Measured:** the soak's aggregate phase moved from −1% to +4%, and the
configuration now sheds `auto_index[users.country]` during that phase rather than
carrying it to the end of the run. A real improvement and a small one, which is
what the analysis above predicts.

## Also

The soak runs in CI as its own job. It found four defects in one sitting and was
being run by hand.

---

# M17 — the log stops growing

`wal.adabt` was one file, appended to for the life of the database, never
truncated, and read **entirely into memory** on every open. Two consequences:
disk usage unbounded in the number of writes ever made, and open time
proportional to the whole history rather than to what happened since the last
checkpoint. It was the entire remaining cost of opening a database — 768ms of
768ms at 200k records.

The log is now a directory of bounded segments. A segment carries a header with
its first LSN and creation time, so the sequence can be understood without
reading any of the bodies, and a segment lying entirely below the last checkpoint
is dropped — or moved to an archive, which is the hook point-in-time recovery
will need.

The ordering matters and is the whole safety argument. Pages flushed, checkpoint
entry logged and synced, directory cache written, **catalog written and fsynced**,
and only then are segments discarded. Discarding any earlier would remove the only
remaining record of which collection each page belongs to — which is exactly what
M16's catalog exists to prevent.

Every entry now carries a wall-clock reading. A log position answers "restore to
entry 4,102,993"; only a clock answers the question an operator asks. It is
written now, while the framing was being rewritten anyway, because adding it once
segments have been archived would mean a format change reaching into files that
are already somebody's backup. It is not trusted for ordering — the LSN is the
order, so a clock that steps backwards makes point-in-time recovery approximate
rather than inconsistent.

## The migration worked

A pre-segment log is adopted rather than refused: the old file becomes the first
segment, which is what it already is once a header is put in front of it. That is
the second migration this project has written and the first that moves real data.
Building the version gate first is what made it a version bump rather than an
incident.

## Vacuum

The free-space map let a deleted record's space be *reused*; nothing could give
it *back*. A dropped collection, or the old copy left behind by a schema
migration, left holes only a future insert could fill, and an operator with a full
disk had no answer except restoring from a backup.

`vacuum` moves live records out of the trailing pages into those holes and
truncates the file. Each move goes through the ordinary write path, so a crash
part-way through leaves the record readable in one place or the other, never
neither.

## Segment size is a real knob

Smaller segments are discarded at a finer grain, so a busy database holds less
redundant log; the cost is more rotations and more files. It is exposed rather
than hard-coded because it is a genuine trade, and because a test that has to
write hundreds of megabytes to cross a boundary is a test nobody runs.

---

# M19 — multi-statement transactions, single-shard snapshot isolation

The largest gap and, per the plan, the highest-risk milestone. It shipped
smaller than planned, and the reason is worth recording: two pieces of
groundwork the approved plan called for turned out to be unnecessary once the
actual mechanism was chosen.

## What was simplified, and why

**No `WalOp::Begin`/`Prepare`/`InDoubt`, no logging-as-you-go.** A `Transaction`
buffers its reads and writes entirely in memory against a fixed snapshot; the
log is touched only at `commit`, atomically, for every write at once. A
transaction that never commits leaves nothing in the log to recover — there is
no in-doubt state to reach because nothing was ever written that could be in
doubt. Three-valued recovery is what a *participant* in two-phase commit needs,
because it must durably remember its intent before voting; there is no
coordinator yet, so there is nothing to remember. When cross-shard 2PC arrives,
a participant will need a durable "prepared" record — a new `WalOp` variant
added then, additively, not a rewrite of this.

**No `CommitTs` split from `TxnId`.** Every write already carries its own
monotonic `TxnId` for MVCC visibility. The worry was that a transaction's five
writes landing under five different stamps could let a reader see three and not
the other two. It cannot: `Database`'s methods take `&mut self`, this codebase
has no internal concurrency, and `commit` runs to completion as one synchronous
call. Nothing can open a new snapshot while it is on the stack — doing so would
need a second live borrow of the same `&mut Database`, which the borrow checker
forbids. A snapshot opened before commit sees none of the five writes; one
opened after sees all of them. There is no window in which only some are
visible, so a shared timestamp buys nothing. `TxnId`'s doc comment now says
plainly what it has always meant — a write stamp, not a transaction's identity
— now that a type with the latter name, `TransactionId`, exists to mean it.

Both simplifications are explained at length in `crates/adabt-engine/src/
transaction.rs`'s module documentation, including exactly what would have to
change for either to stop holding — the codebase gaining real internal
concurrency, in the second case.

## What was kept from the plan, because the reasoning still applied

**One shared `Arc<VersionTracker>` across every shard.** `HeapStore::open_shared`
and `Database::open_shared` accept an external tracker; `ShardedDatabase::open`
constructs one and hands it to every shard. Nothing today compares a timestamp
across shards, so this changes nothing observable — and that is exactly why it
is cheap now and would not be later: retrofitting it once shards hold years of
data means rewriting every timestamp already written. `shards_share_one_
comparable_timestamp_space` checks it directly.

**First-committer-wins conflict detection**, via `HeapStore::latest_write_ts` —
the newest stamp in a record's version chain, which reclamation never discards.
A transaction's write-set is checked against it before anything is applied: any
key touched since the transaction's snapshot was taken aborts the whole
transaction, not just that key.

**Genuine all-or-nothing atomicity**, via two passes over `commit`'s write-set —
conflicts, schema and unique constraints checked for every key first, with
nothing yet touched; only if every key clears does the second pass apply any of
them, through the ordinary `update`/`delete` paths so reindexing, epoch bumps
and telemetry happen exactly as they would for a standalone write. This
duplicates a check `Database::update` makes again on the way in, deliberately —
the alternative interleaves validation with application and can abort a
transaction that has already partly landed.

## DDL is not transactional, on purpose

There is no `txn.create_collection`. Schema and index changes take effect
immediately, outside any transaction, because they already have their own
crash story — `AdoptMigration`'s atomic flip, the persisted catalog — built
before transactions existed and correct on its own terms. Folding DDL into
transactional semantics raises a real question this milestone does not need to
answer: what an aborted collection-creating transaction should do to concurrent
writers already using it.

## What this does not do

Snapshot isolation, not serializability — two transactions with disjoint
write-sets can still produce a result no serial execution would (write skew).
Serializability is later, policy-selectable work, offered as a choice like
every other guarantee here.

Nothing crosses shards. A transaction is born from one `Database` and can only
write to it. `docs/m19-notes` — this section — is where cross-shard 2PC's actual
prerequisite work is recorded as *not yet started*, deliberately, since the
groundwork above is what makes starting it later additive rather than a rewrite.

**Testing.** Rather than extending the generic differential `Op`/`Generator`
machinery with transactional operations — a substantial undertaking on its own,
effectively reimplementing snapshot isolation a second time inside the
reference model — this shipped with 20 hand-written engine tests plus 2
sharding tests covering: isolation in both directions (a reader before commit
sees nothing, one after sees everything), read-your-own-writes for both `get`
and `scan`, first-committer-wins under both disjoint and conflicting write-sets,
atomicity under three distinct failure causes (conflict, schema, unique
constraint) each verified to leave *zero* partial state, reindexing correctness
after commit, and the crash-safety claim directly — an uncommitted transaction
followed by a real restart leaves the pre-transaction state exactly, not
approximately, intact. This is a scoping choice, not an oversight, and it is
recorded here as one.

---

# M20 — widening the query surface, before anything freezes

## What shipped

**The IR is a tree, not a chain.** `LogicalOp::children() -> Vec<&LogicalOp>`
and `sources() -> Vec<&str>` replace the assumption — never stated, only
implied by every variant having at most one child — that a plan has exactly
one input and reads exactly one collection. `child()`/`collection()` still
work for every existing variant and now panic loudly if ever reached on a
multi-child node, which cannot happen today: `Database::query` rejects any
plan containing a `Join` via `LogicalOp::contains_join()` before the planner
sees it.

**`Join` is reserved, not built.** Two children, a kind (`Inner`/`Left`), a
single equi-join key. The planner and executor gained exactly one new arm each
— a panic documented as "should have been rejected earlier" — because the real
rejection happens once, at the query entry point, with a proper
`Error::Unsupported` a caller can act on. M23 turns this into a feature by
adding an algorithm; nothing here needs to change to make room for it.

**The arity bug is fixed before the bug exists.** `QueryShape`'s hash walked
"the" child and stopped — sound while every node had at most one, silently
wrong the moment a node could have two. Adding `Join` without first fixing
this would have meant a naive `.child()`-shaped implementation could hash only
one side of a join and never notice the other changed at all;
`arity_is_hashed_even_when_every_other_field_matches` is the regression test
for exactly that failure, written against a `Join` that cannot yet be queried
but whose shape already has to be trustworthy — a shape is a decision-log key,
and the decision log is compared across builds.

**Expressions gained arithmetic, `IN`, and `LIKE`.** Arithmetic is exact
wherever both operands are exact (`checked_arith` in `adabt-core`, rescaling
`Decimal`/integer operands to a common scale in `i128`), falling back to `f64`
only once an `F64` operand or an overflow forces it —
`exact_decimal_arithmetic_never_drifts_over_many_operations` chains a thousand
additions of `0.01` and lands on exactly `10.00`. `IN` and `LIKE` (SQL
wildcards, `%`/`_`, backslash-escaped) both propagate `Unknown` on a null or
type-mismatched operand, matching this file's existing three-valued logic
rather than introducing a second convention next to it.

**The typed builder API is `Expr::field(...) + Expr::lit(...)`,
`.in_values([...])`, `.like("...")`** — real `std::ops::{Add,Sub,Mul,Div}`
implementations rather than same-named inherent methods (clippy was right that
`fn add` shadowing the trait invites exactly the confusion it warns about, and
the trait is strictly more useful: it is what makes `a + b` work at all).
Building a *separate* new builder API was judged unnecessary: `LogicalOp` and
`Expr` already had one, used throughout this codebase's own tests, and
extending it is smaller and more consistent than replacing it.

## What was scoped out, and why

**Result row type stays `Vec<(RecordId, Record)>`.** A join row has no
`RecordId` in any meaningful sense, and this type is deeply embedded — every
store, cache, the wire codec, `ShardedDatabase::merge_by_id`. Widening it now,
with no working join to test the new shape against, is exactly the untested,
premature abstraction this project's own practice argues against. It moves to
M23, where a real join produces real rows to design the type around.

**`PlanDecision` stays singular.** The same reasoning: it becomes a real
problem only once a plan has two leaves needing two different access
decisions, which does not happen before M23.

**No streaming/cursor API, no new wire-level IR encoding, no fuzzing harness,
no differential-runner `Op` extension.** Each is real, separable work with its
own design space. M16 through M19 already shipped a working precedent for
covering new correctness surface without extending the generic differential
machinery — direct, hand-written tests, at the same rigor — and this milestone
follows it rather than taking on a second large piece of infrastructure to
prove roughly the same properties the direct tests already prove.

---

# M21 — schema evolution, without the tax on scale

## What shipped

**`alter_schema` picks its own cost now.** `codec::schema_editable_in_place`
(`crates/adabt-storage/src/codec.rs`) decides, from the two schemas alone and
without reading a single row, whether every record already on disk still
decodes correctly under the new one. When it does, `HeapStore::alter_schema`
logs one `WalOp::AlterSchemaInPlace` entry — a catalog edit — and returns;
when it does not, it falls back to the existing copy-and-swap
(`AdoptMigration`) path unchanged. The public surface is one method, at both
the storage layer and now the engine layer (`Database::alter_schema`,
`ShardedDatabase::alter_schema` — new: before this milestone the only caller
of `HeapStore::alter_schema` was the auto-optimizer's `freeze_schema`, so
there was no way for an application to evolve its own schema at all). The
`Ok(usize)` it returns is rows physically rewritten; `0` is the honest,
directly-checkable signal that the cheap path was taken.

**The eligible set is real, but narrower and more mode-specific than "add a
nullable column, drop the last one" suggests**, because the codec's binary
layout is a different rule per `SchemaMode`:

| Mode | Append (nullable, tail) | Drop (last field) |
|---|---|---|
| `Fixed` | in place, if the field count does not cross a bitmap byte boundary | in place, unconditionally |
| `Strict` | always copy-and-swap | in place, only if the dropped field was variable-width |
| `Declared` | always copy-and-swap | always copy-and-swap |
| `Dynamic` | n/a — no declared fields | n/a |

`Fixed` is the only mode where appending is ever free: its layout is bitmap
then fixed region then nothing, so there is no offset table whose position
depends on how large the fixed region is. `Strict` and `Declared` both keep an
offset table immediately after the fixed region, and that table's position is
`fixed_at + fixed_region_len` — appending *or* dropping a fixed-width field
moves it, full stop, regardless of whether the new field's own bytes are
presence-gated. `Strict`'s one further affordance — dropping a trailing
*variable*-width field — works because that never touches the fixed region at
all: the table's position is untouched, and reading one fewer entry from it is
a safe prefix of real bytes. `Declared` cannot take even that, because a real
record there may carry an overflow bag right after its last declared field,
and the new schema's table would then be one entry short of the old one and
read the dropped field's bytes as the overflow section's own length-prefixed
count — silently wrong, not a decode error, and telling the two cases apart
needs the full-scan read this feature exists to avoid.

**Every one of those rules exists because a version of this function got one
of them wrong first.** The initial draft allowed a nullable fixed-width
append for `Strict`/`Declared` on the argument that the new field is
presence-gated before its bytes are read — true, but irrelevant, because the
offset table's *position*, not the new field's own bytes, is what breaks.
`crates/adabt-storage/tests/schema_evolution.rs` caught it immediately: a
straightforward append-then-read-back test failed with
`Error::Corruption("record too short for its offset table")` the first time
it ran. `codec::in_place_eligibility` (a unit-test module inside
`codec.rs`, next to the function it tests) is the permanent regression
suite for exactly this: it encodes a record under one schema and decodes it
under another, for every case the doc comment claims is safe and several
specific cases — a trailing fixed-width drop in `Strict`, a trailing append
in `Strict` however safe it looks — that must still copy-and-swap. The
lesson generalizes past this one function: for bit-packed layout code in this
project, "the reasoning sounds right" is not evidence: a written-out record,
decoded and checked, is.

**A second, independent gap surfaced from the same test suite: copy-and-swap
could not drop a field at all**, for any mode that forbids extra fields.
`alter_schema`'s row loop validated the *raw* old record against the new
schema before rewriting it, and a `Strict` or `Fixed` record still carrying a
field the new schema no longer declares fails `Schema::validate_record` with
`UnknownField` — not "the row needs projecting," an outright refusal. This
was already true before this milestone; it never surfaced because
`freeze_schema`'s only caller infers the new schema from the existing data by
construction, so an old record can never carry a field the inferred schema
doesn't also declare. It became reachable the moment `alter_schema` got a
caller that picks its own target schema independent of the data — this
milestone's whole point. Fixed by `project_onto_schema`: for a mode that
forbids extras, a record is projected onto only the fields the new schema
still recognizes before it is validated and re-encoded; for `Declared` and
`Dynamic`, which already carry an unrecognized field through to the encoded
overflow bag, it is an explicit no-op. `freeze_schema`'s behavior is
unchanged by construction — inference can never produce a schema the
projection would touch.

**Recovery gained one pass-1 arm**, alongside `CreateCollection` and the
other collection-definition ops: `WalOp::AlterSchemaInPlace` rebuilds a
collection's codec from the logged schema, in the same log-order pass that
already handles every other collection-definition change, since — unlike
`AdoptMigration` — it moves no record and so needs no pass-4 deferral.
`schema_evolution.rs`'s restart test exercises it directly, both from a raw
WAL replay and, after a checkpoint, from the persisted catalog — the same
generic path (`catalog_snapshot` already serializes whatever codec a
collection currently has) every other schema already took.

## What was kept from the plan, because the reasoning still applied

*"Keep the `AdoptMigration` design. Its crash story is sound and hard-won."*
Nothing about it changed: it is still the only path for anything the
in-place rule excludes, its single-flip-entry recovery story is untouched,
and `crash_during_optimization.rs`'s log-truncation sweep still exercises it
end to end. The in-place path's own crash story does not need that
machinery — one `WalOp` entry is exactly as atomic as any other single log
entry the WAL already guarantees, so there is no new crash surface to design
against, only the existing one to reuse.

## What was scoped out, and why

**No general-purpose field rename, retype, or nullability-narrowing gets an
in-place path.** Every one of those needs to inspect or transform existing
values, not just leave bytes alone — a fundamentally different, riskier
problem than "does this byte still mean the same thing," and outside what
this milestone's plan text asked for ("a nullable added column," "a dropped
column").

**No lazy space reclamation for the copy-and-swap path.** The plan named
tombstone-and-reclaim for a *dropped* column specifically, and the in-place
drop path already reclaims for free — dropped bytes are simply never read
again, no rewrite needed. A copy-and-swap already reclaims everything by
construction (it rewrites every row into a fresh collection). There was no
remaining case this item was pointing at.

---

# M22 — operations: the things that make a real deployment survivable

The largest milestone in Track A by shipped surface. Six independent gaps
named by the plan, each closed at both the storage/exec layer and the engine
layer, plus the server hygiene that follows from actually running the thing.

## Backup, restore, point-in-time recovery

**A backup is not a new file format — it is a database directory.**
`HeapStore::backup_to` checkpoints (folding the log into pages and rotating
segments, exactly as any checkpoint does), then copies only what a restart
itself depends on: `heap.adabt`, the `wal/` segment directory, the superblock,
the catalog. `directory.adabt` and `derived.adabt` are left behind on purpose
— they are caches, a restored directory rebuilds them exactly as any other
reopen would if they turned out to be missing, and a cache stamped against a
checkpoint that is not necessarily `dest`'s most recent has no advantage over
not being there. `HeapStore::restore_from` is the mirror: refuses a source
with no superblock (not a backup) and a destination that already holds
anything (never silently overwritten). The engine layer
(`Database::backup_to`) adds exactly the one file storage cannot know about —
`unique.adabt`, the constraint sidecar, which lives at that layer because
whether a field is constrained is a logical decision, not a physical one.
`ShardedDatabase::backup_to` backs up every shard into `dest/shard-N`, the
same layout `open` already expects, so the result is directly openable with
no translation step.

**Point-in-time recovery cost nothing new to build, because it is not a new
mechanism.** `RecoverTarget::Lsn` truncates the entries `recover()` replays to
a prefix ending at the target lsn, then runs the *same* recovery passes every
other open runs. That is deliberate and load-bearing: a log that stops
partway through is exactly the case M16–M19's crash-recovery tests already
prove correct, over and over, at arbitrary byte offsets. PITR is that same
proven mechanism, fed a log shortened on purpose instead of by a crash.
Restoring to a target the backup's own checkpoint already passed is refused
(`Error::RestoreTargetUnreachable`) rather than silently answered with
whatever the catalog already reflects — the catalog is unconditionally
adopted in pass 0, so "replay less" cannot undo state already folded into it;
the only fix is an earlier backup. `Wal::lsn_at_or_before` turns "restore to
14:32" into the lsn `open_at` actually wants, by walking the log in lsn order
and keeping the last entry whose clock reading had not yet passed the target
— approximate under a clock that steps backward, exactly as the M17 doc
comment on `WalEntry::nanos` already promised, never inconsistent, because
what it returns is still a real prefix of the log.

**Deliberately not wired into the wire protocol.** Backup, restore and PITR
are administrative operations on a directory the server process already has
filesystem access to; adding wire RPCs for them would mean either running
them through the same unauthenticated, untrusted-network-hostile connection
every other request does, or inventing a second, privileged channel — both
bigger and riskier than the actual ask. An operator or a wrapping process
calls these through the Rust API directly, the same way `adabt-server`'s own
`main.rs` calls `checkpoint()`.

## Per-query memory budget and cancellation

**`Constraints::max_ram_bytes` turned out to already mean something else.**
The first version of this work read that exact field at query time and broke
an existing, passing test (`a_hard_memory_ceiling_is_respected_by_the_driver`)
the moment it ran: that field is the *optimizer's* build budget — how much
extra memory a derived representation may cost while being built — and a
policy tuned to keep the optimizer thrifty (64 KiB, in that test) was never
making a claim about how much a single ordinary query's own sort or aggregate
may buffer. Conflating the two meant a policy correct for one purpose started
silently rejecting queries for an unrelated reason. Fixed by adding
`Constraints::max_query_ram_bytes` as its own field rather than reusing the
old one — `max_ram_bytes` and everything that already read it are completely
unchanged, and the new enforcement reads only the new field. The general
lesson repeats the one from M21's schema-eligibility bug: an existing,
established field's meaning is not something to infer from its name and
extend on that basis — it is something to find the actual readers of and
check against.

**Enforcement is `adabt_exec::exec::ExecBudget`**, threaded through
`execute_with_budget` (the original `execute` still exists, unbounded, so
every prior caller keeps its old behaviour verbatim) alongside the existing
`ExecStats` parameter. Two independent checks: `check_ram`, called as `Sort`
and `Aggregate` accumulate their whole input (the only operators that ever
buffer unboundedly — everything else is already bounded by one
`RecordBatch` regardless of collection size), using a new
`Record::approx_size`/`Value::approx_size` — deliberately approximate,
documented as such, since a budget is a circuit breaker against a query
unboundedly larger than expected, not an accountant. And `check_cancelled`,
polled once per operator node and every 4096 rows inside the row-at-a-time
scan loop (`fetch_batches`) — cheap enough to amortize, frequent enough that
a cancelled query's response time stays small next to how long a large scan
takes anyway. There is no timer anywhere in this: `ExecBudget::cancel` is one
`Arc<AtomicBool>`, and a caller wanting "stop after 5 seconds" spawns a thread
that sleeps 5 seconds and sets it — `Database::query_cancellable` and
`ShardedDatabase::query_cancellable` (which threads the same flag to every
shard's scan *and* the centralized merge step) are the entry points, and
plain `query()` stays exactly as uncancellable as it always was.

## Server operations

**Idle timeout and a connection cap**, because this is a thread-per-connection
server and a stuck thread is a thread nothing else can use.
`stream.set_read_timeout` closes a connection that opened and sent nothing
(default 300s, `--idle-timeout-secs`); a cap (default 1024,
`--max-connections`) refuses a new connection outright once that many are
open, closing it immediately rather than queuing it behind however many
threads the process would spawn.

**Graceful shutdown drains rather than just stopping.** `Stopper::stop`
already existed and stopped the accept loop; it was never wired to anything.
`serve` now waits, bounded by `DRAIN_TIMEOUT` (30s), for connections already
accepted to finish on their own before returning, so a request in flight when
shutdown is requested gets to complete. `main.rs` installs `SIGINT`/`SIGTERM`
handlers via two raw `signal(2)` FFI calls — no crate: the workspace's one
external dependency is `thiserror`, and a signal-handling crate for two calls
was a poor trade for that property. Unix-only (`#[cfg(unix)]`); on any other
platform the server has no graceful path yet, which is the same behaviour it
had before this milestone, not a regression. Verified by hand against the
actual compiled binary — `kill -TERM`/`kill -INT` against a running server,
both single- and multi-shard, both exit cleanly and leave a checkpointed
directory behind (`Database::checkpoint`, called from `main` after `serve`
returns, using the `Shared` handle obtained *before* `serve(self)` took
ownership).

**Metrics export** formats the `Snapshot` every query and optimization
decision already populates — `adabt_telemetry::to_prometheus_text`, a new,
pure formatting function with no new instrumentation behind it — as
Prometheus exposition text, reachable over the wire as `RequestKind::Metrics`
/ `Client::metrics()`. `ShardedDatabase::metrics_text` emits one block per
shard rather than merging them into one series set: shards already optimize
from independent traffic and are not expected to agree, which
`explain_optimizations` already treats as a feature this has no business
hiding by summing it away.

**Slow-query log** is opt-in (`Database::set_slow_query_sink` /
`enable_slow_query_log`, `--slow-query-log-ms` on the server), checked against
the same `started: Instant` `query_in` already captures for its own
telemetry — no second timer. The event carries `explain` text built lazily,
only once a query has actually crossed the threshold, so a database with
nothing configured — the default — pays exactly what it already paid before
this existed. Applies to the general query path only, not the compiled or
direct-lookup shortcuts an O(1) identity read takes; those are not the kind
of query this exists to surface, and timing them anyway just to confirm that
would spend the exact per-query cost the opt-in default is there to avoid.

**`StatusCode` gained `NotImplemented` and `Cancelled`**, and `of()` now maps
`UniqueViolation`, `TransactionConflict`, `Unsupported` and `InvalidRestore`
to distinct, correct statuses instead of falling into `Internal` — found by
re-reading `of()` against the full `Error` enum while already touching this
file for `Metrics`, not a separately-scoped fix.

**Posture, stated rather than implied.** `adabt-server`'s module docs and
`--help` now say plainly what was already true: no authentication, no
transport encryption, every connection that can reach the port has full
access. Auth is M38. The instruction is to bind this to a trusted network
only — loopback, a private subnet, or a socket reachable only from processes
that already trust each other — not to expose it and rely on the connection
cap and idle timeout as a substitute for a perimeter they were never meant to
be.

## What was scoped out, and why

**No merged cross-shard metrics.** Summing `Histogram`s and per-key maps
across shards is real, separable work, and the plan named export, not
aggregation — one block per shard is a complete, honest answer to "what has
this server observed," not a placeholder for a better one.

**No `ShardedDatabase::enable_slow_query_log` convenience wrapper.** The
capability already exists without it: `ShardedDatabase::shard(i)` already
hands out each shard's `Database`, and `main.rs`'s own
`--slow-query-log-ms` wiring uses exactly that path. A wrapper would be a
thin loop over an already-public accessor, not a new capability.

---

# M23 — joins, and the close of Track A

The last milestone of Track A. A real join algorithm over the tree-shaped IR
M20 froze, with the memory discipline M22 built reused rather than
reinvented.

## What shipped

**`LogicalOp::Join` executes.** `Database::query` no longer rejects a plan
containing one; it routes to a small, dedicated `query_join` path instead of
entering the general `query_in` machinery at all — deliberately, not as a
shortcut. `query_in`'s first real step is `logical.collection()`, which
`LogicalOp::collection()` documents as asserting exactly one source and
panicking otherwise; a join's whole premise is two. Every cache `query_in`
consults (plan cache, result cache, materialized views, the compiled-path
shortcut) is keyed or reasoned about the same way — one collection, one
epoch — so retrofitting them to a two-source plan would have meant
redesigning each one's key, not just adding a branch. A join is planned
fresh every call, checked against the query memory budget and cancellation
flag exactly as any other query is, and its own `explain()` and slow-query
logging both work — but it does not populate the plan cache, does not
consult or populate the result cache, and its index usage does not yet
reach the adaptive driver's telemetry. That last point is a real, narrow,
separable gap — a join's own access pattern does not teach the optimizer
anything about indexing either side yet — not a correctness concern.

**The planner reuses every existing single-collection rule, twice.** Rather
than inventing a per-node `PlanDecision` — the thing M20 explicitly deferred
to "M23, where a real join produces real rows to design the type around" —
`plan()` now checks `contains_join()` and, when true, routes through
`plan_join`, which plans each side by recursing into `plan()` itself. Each
side is planned exactly as if it were the whole query: an index-eligible
filter still gets `IndexLookup`, a projected aggregate still gets pushed
into a column store if one exists. No new decision type was needed because
nothing about *what* decision applies changed — only that there are now two
independent places it gets made. `PhysicalOp` gained the same `children()` /
arity-aware `child()` widening `LogicalOp` got in M20, for the identical
reason: `.collection()`, `.is_full_scan()`, `.access_path()` and `explain()`
all used to assume one child, and a `Join` node breaks that assumption in
exactly the way M20's own postmortem said it would.

**Two algorithms, chosen for a real reason, not a coin flip.** An *indexed
nested loop* probes the right side's join-field index once per left row,
through the same `Source::index_lookup` every `IndexLookup` operator
already uses — and never materializes the right side at all, which is
directly the "unbounded hash build" the plan text warns about, avoided by
construction rather than mitigated after the fact. It only applies when
`right` is a bare, unfiltered `HeapScan`: anything else — a filter, a sort,
a projection — means bypassing it via a direct index probe would silently
ignore whatever that subtree computes, which `a_filter_on_the_right_side_is
_still_applied_under_the_fast_path` exists specifically to catch (it does,
having failed against an earlier draft that checked mere index existence
without checking the right side's shape first). Everything else — no index,
or a wrapped right side — falls back to a *hash join*: both sides
materialized through the same budget-checked `collect_rows` `Sort` and
`Aggregate` already use, a `HashMap` built over the right side, the left
side driving the probe in order.

**The build side is fixed, on purpose, not chosen by size.** An early draft
considered building the hash table on whichever side had fewer rows — the
standard hash-join heuristic, and a reasonable read of "join ordering" in
the plan text. It was dropped: row counts can change between two runs of the
same query as data grows, so a size-driven choice means the *same query*
could return the *same rows in a different order* purely because the data
changed size — exactly what "optimization never changes the answer" rules
out, applied to order rather than content for once. The right side is always
the build side and the left side is always the one driving output order, for
both `Inner` and `Left` — which also has a correctness reason for `Left`
specifically: every left row must be visited exactly once regardless of
whether it matched, and driving the loop over anything else could not
guarantee that as directly. `hash_join_and_the_indexed_fast_path_agree` is
the direct evidence both algorithms produce the identical result set (and,
via `fingerprint`, in comparable order) regardless of which one executed —
the same property `indexes_never_change_query_results` already proves for
ordinary scans, now proven for a join too.

**A joined row has no natural id, so it does not get one.** Every output row
is assigned `RecordId(i)` from its position in the join's output — exactly
the trick `MaterializedViews::rows()` and `exec::aggregate()` already use for
the same reason. This is what let M20's "result row type with an optional
id" deferral stay deferred: `Vec<(RecordId, Record)>` did not need widening,
because a join row already had a precedent for fitting into it. Every field
from both sides is prefixed `collection.field` — unconditionally, not only
on an actual name collision, because a join result has no schema of its own
declaring which fields might collide as either side's schema evolves later;
prefixing always is SQL's own `table.column` qualification, applied
consistently rather than only when ambiguous. A join whose two sides read
the same collection (a self-join) is refused outright, because that
prefixing scheme collapses to one ambiguous name when both sides share a
collection name, and aliasing is not designed yet.

**One join per query, and it says so.** A plan may contain at most one
`Join` node anywhere in it — checked once, in `query_join`, by counting
them. Real multi-way join planning is choosing an execution order among an
n-way join's n! orderings, a cost-based search problem with no cost model
behind it in this codebase outside `adabt-opt`'s representation-choice
scoring, which answers an unrelated question. "Nested-loop and hash join"
in the plan text is about the algorithm for *a* join; ordering among several
is real, separable, future work, refused with a clear error rather than
silently mis-executed or accepted and left to produce whatever a naive
recursive descent happened to do with it.

**`ShardedDatabase` refuses a join instead of panicking on one.** Before this
milestone, handing a `Join`-containing plan to `ShardedDatabase::query`
crashed the process: `pushdown`'s fallback calls
`other.child().expect("non-leaf has a child")`, and `LogicalOp::child()`
panics by design on a `Join`. That was a real, already-reachable bug
independent of whatever join algorithm got built — found by the same
research pass that scoped the rest of this milestone, fixed with one early
`contains_join()` check before `pushdown` ever runs. It returns
`Error::Unsupported` now, not a crash, and names the actual escape hatch:
`ShardedDatabase::shard(i)` already hands out one shard's own `Database`,
which supports a join over that shard's data directly — a real, if partial,
capability, not a dead end. Real cross-shard join execution — two
collections each partitioned independently by `RecordId % shards`, so a
matching pair can land on different shards — is broadcast or shuffle join, a
substantially larger problem than the single-node algorithm this milestone
built, and explicitly out of scope for it.

## What was scoped out, and why

**No differential-runner extension for joins.** `testkit::ops::Op` has no
query-shaped variant at all today — not for filter, sort, or aggregate,
which have already shipped for three milestones. Extending it for joins
specifically would mean building an entire query-generation layer (a new
`Op`, join semantics in `ReferenceStore`, generator support for picking two
collections and a field) from nothing, not a small addition — confirmed by
research before writing a line of it. Hand-written direct tests are this
project's own established path for new query-execution correctness (M19's
transaction suite, M20's expression-completeness suite), followed here too:
sixteen tests in `crates/adabt-engine/tests/joins.rs`, including the
algorithm-agreement and filtered-right-side tests that are the actual
correctness evidence a differential run would otherwise have had to supply.

**No cost-based join ordering.** Addressed above — a real, large, separable
problem this milestone does not have the cost model to attempt safely.

**No aliasing for self-joins.** Refused with a clear error instead of
silently losing one side's fields, or of guessing an aliasing scheme with no
real design behind it yet.

**Index usage inside a join does not reach adaptive-driver telemetry.**
`query_join` bypasses `query_in`'s telemetry entirely, by design (see
above); a join's own access decisions do not yet teach the optimizer to
build or drop an index the way an ordinary query's do. Real, narrow, and
separable from correctness.

---

# Track A — closed

M16 through M23 are complete: durable storage with a persisted catalog and a
bounded, archivable log; ten years of runtime optimizations proven not to
change an answer, now including a join; multi-statement snapshot-isolation
transactions; a tree-shaped, versioned query IR with a typed builder API;
schema evolution that is a catalog edit wherever the byte layout allows it
and a safe copy-and-swap everywhere else; online backup, restore and
point-in-time recovery; a per-query memory budget and cooperative
cancellation; a server with connection limits, timeouts, graceful shutdown,
metrics export and a stated trusted-network-only posture; and now joins.
Every milestone closed with `cargo fmt`, `cargo clippy -D warnings`,
`cargo test --workspace`, and a soak run against the level-0 reference
showing zero divergence — the same four gates, every time, including this
one. Track B — manual optimality — starts next.

---

# M24 — expressive policy, and the start of Track B

Track B's first milestone. `Mode::Manual`'s `overrides` could only ever
toggle something globally; this widens it to name a scope and carry params,
closes a real routing bug that made the widening's most important case a
silent no-op, and adds one small, honestly-scoped new capability for the
one thing the compiled-path mechanism can actually specialise.

## What shipped

**`overrides: Vec<(String, bool)>` became `overrides: Vec<Override>`**,
where `Override{name, scope, enabled, params}`. `Override::toggle(name, on)`
reproduces the old behaviour exactly (scope defaults to `"global"`, no
params); `Override::scoped(name, scope, on).with_param(k, v)` is new. `scope`
and `params` are not new concepts invented for this — they are the exact
vocabulary `adabt-opt`'s `Registry`/`Optimization`/`OptScope` machinery
already had, just never reachable from a `Policy`. `params` is
`Vec<(String, i64)>`, not `adabt_opt::config::Params` directly: `adabt-core`
has no dependency on `adabt-opt` and must not gain one to name a policy
directive, the same reasoning that already put `IndexKind` in `adabt-core`
instead of the crate that builds one. `ManualDriver::decide` converts it to
a real `Params` where it is actually consumed.

**A real bug, found by trying to write the first test.** Widening `scope`
alone was not enough: `AutoIndexOpt::plan_enable`'s signature had no
parameter for `params` at all — `Decision.params` was recorded into
`OptimizationConfig` but never reached the trait method that decides *what*
to build. Fixed by adding `params: &Params` to
`Optimization::plan_enable`/`plan_disable`, threaded through the ~13
implementations (mechanical for every optimization but one) and the two
controller call sites. `AutoIndexOpt::plan_enable` now checks for an
explicit `kind` param — encoded as `IndexKind::as_ordinal()`, since `Params`
is `i64`-only and `IndexKind` already exists specifically to be *named*
without depending on what builds one — and only falls back to
`index_kind_for`'s telemetry-driven guess when the caller did not name one.

**A second bug, found once the first test actually ran against real level
presets.** A level enables `auto_index` at `"global"` too — the same blanket
request an unscoped override makes — and a scoped override's specific entry
was simply *added* alongside it, not replacing it: both survived in the
target config, the blanket one still expanded via `candidate_scopes` into
every qualifying field including the one just named, and its empty params
silently out-raced the named override's `kind` in the observed result. Fixed
by clearing an optimization's `"global"` entry whenever any override for
that name specifies an explicit scope, before applying any of them — naming
a scope takes over from the blanket default rather than living beside it.
Both bugs were caught the way this project's own practice keeps catching
them: by writing the test against the real path (`Database::open` → seeded
workload → `optimize()` → check the actual access path chosen) rather than
trusting that a scope string threaded through correctly meant it was used
correctly.

**`applicability()` still gates a manual override exactly as it gates an
adaptive one — deliberately, and this was checked against the code, not
assumed.** `driver.rs`'s own module doc already states the principle:
"nothing a human can ask for bypasses the machinery the optimizer uses."
`OptimizationController::apply` calls `opt.applicability(ctx)` uniformly for
every `Decision`, manual or adaptive, before anything is built. An early
draft of this milestone considered letting an *explicit* override skip that
gate, on the reasoning that the workload-evidence thresholds
(`MIN_ROWS_FOR_INDEX`, `MIN_QUERIES_FOR_INDEX`) exist to keep the automatic
driver from jumping to conclusions, not to second-guess a deliberate
declaration. Reading `controller.rs` settled it the other way: that
uniformity is load-bearing architecture, not an oversight, and an expert
who wants an index unconditionally, evidence or not, already has the tool
for that — `Database::create_index`, which sits outside the
policy/optimizer system entirely and always has. "Index users.country hash"
as a *policy directive* means "the optimizer should treat this as
index-eligible, honoring my choice of kind, once its ordinary gates are
satisfied" — a real, different, and correctly narrower thing than an
imperative command.

**`Database::compile_identity_lookups(collection)`** forces the one shape
`CompiledPaths` is able to specialize — a bare `GetById` — on immediately,
bypassing the `HOT_THRESHOLD` (256) call-count gate. Reading `compiled.rs`
before designing anything here mattered: `CompiledPaths::candidate` only
ever recognizes an identity lookup, by design, since anything else has real
work left to do that skipping the general path would mean reimplementing.
"Compile shape X" for an arbitrary shape is therefore not a capability this
mechanism has today, forced or not — there is no other shape *to* compile.
What forcing legitimately buys, honestly scoped to what it is: a workload
doing a handful of expensive identity lookups, fewer than the threshold,
gets the specialised path from its first call instead of earning it at 256.

**A typo'd override name is now reported at `Database::open`.**
`ManualDriver::decide` still silently skips a `Decision` naming an
unregistered optimization every cycle — correctly: a name that existed in
an older build and has since been dropped must not crash a database that
opens with it in its policy. But that same silence made "auto_indx" (missing
an `e`) undiagnosable except by wondering why an index never appeared.
`Registry::validate_overrides`, called once in `open_with_store`, checks
every override's name against the registry before anything is built and
returns `Error::InvalidOptimization` naming exactly which ones do not exist.

## What was scoped out, and why

**Clustering — "cluster orders by customer_id" — was not attempted.**
Confirmed by search before writing anything: no clustered or
sorted-by-arbitrary-field physical representation exists anywhere in this
codebase. `DirectLookup` addresses by `RecordId` arithmetic only, unrelated
to a field's value ordering. This is the plan's own M25 ("Index and layout
library"), not M24's to build a mechanism for early — a policy directive
naming it here would have nothing real to route to.

**Mixing a blanket toggle and a scoped override for the same optimization
within one `overrides` list has ambiguous, order-dependent semantics** —
whichever was applied last wins, entry by entry, the same way a repeated
key in any list-applied-in-order structure would. Not resolved further:
this is a genuinely contradictory input ("turn it on everywhere" and "turn
it on specifically here," from the same caller, in the same policy), and
the realistic case this milestone's own bugs were about — a *level's*
blanket entry interacting with an *override's* scoped one — is the one that
is fixed.
