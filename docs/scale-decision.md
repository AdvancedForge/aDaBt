# The scale question, decided

`docs/roadmap.md` Stage 3 forced a choice: is this engine for datasets that
fit in RAM, or not? Everything after the decision — thread-per-core's
per-core memory model, the value of further layout work — assumes an answer.
This document is that answer, with the numbers that forced it.

## The decision

**aDaBt is for datasets that fit in RAM. The ceiling is a documented
property of the design, not a gap in it.**

The page directory and every index are fully resident by construction. That
is why point lookups win against SQLite by 2–4× and why the optimizer can
restructure physical representations without I/O in the loop; it is also,
measured, what makes ~100M rows unreachable on any plausible machine.

## What it costs to change, and why not now

Paging the directory to disk and giving indexes a spill path is the correct
fix. It is also a storage-engine rewrite: `HeapStore`'s
`BTreeMap<RecordId, VersionChain>` directory, every index's in-memory map,
and the buffer pool's assumptions all sit behind it. It would invalidate
Stage 2's remaining layout work (which tunes the resident representation)
and Stage 4's per-core memory model (which partitions it). A decision this
load-bearing is written down before the work that depends on it, which is
what this file is.

Revisit triggers, so this is falsifiable rather than permanent:

- A workload where the resident set fits but the *dataset* does not, and
  the comparison harness can express it.
- Thread-per-core landed and measured, making the residency model the
  dominant remaining ceiling rather than one of several.

Until either fires, "how big can it go" is answered by measurement:

| rows | marginal bytes/row | source |
|---|---|---|
| 100k → 1.6M | 470–758, converging near 470 | `docs/m36-notes.md`, scale rungs |

At ~470 B/row: **1 GB of RAM ≈ 2M records; 10 GB ≈ 20M.** The practical
ceiling on ordinary developer hardware is single-digit millions of records.

## What the decision changes

- **Track A** moves to roughly 95%: cross-shard 2PC, serializable as a
  selectable level, and DDL's documented non-transactionality are what
  remain between here and "you could ship on it."
- **Sharding is the growth story.** With residency fixed by decision,
  more data means more shards, not a paged directory — which makes Stage 5
  load-bearing rather than ceremonial.
- **The server states the property.** Operators deserve the ceiling at
  connect time, not in a doc they were never told to read.
- Stage 4 may build its per-core memory model on resident collections
  without a caveat, because the caveat is now the contract.
