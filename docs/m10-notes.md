# M10: snapshot reads and persistence

The unglamorous milestone everything after M11 was blocked behind.

## Why snapshots had to come first

Shadow execution compares a candidate representation against a baseline. That
comparison is meaningless unless both read the *same state at the same time* —
if the data moves between the two reads, a divergence proves nothing and an
agreement proves less.

Reads previously took `&mut self` and saw whatever the heap held at that
instant. There was no way to ask "what did this look like a moment ago", so
M11 was not merely unimplemented, it was unimplementable.

## What was built

Version chains per record, using the `txn` field reserved in the record header
since M1 — which is why adding this now is not a format change. Every write is
stamped, superseded versions are retained while a snapshot might reach them, and
a reader at snapshot `T` sees the newest version stamped at or before `T`.

A delete records a *tombstone version* rather than removing the entry, so a
reader whose snapshot predates the delete still finds the record.

Retention is driven by the **oldest open snapshot**. A version superseded after
that point may still be needed; one superseded before it cannot be. Reclaiming
on any other basis risks a reader seeing a hole mid-scan.

## Not full MVCC

This is a version chain, not multi-version concurrency control. No concurrent
writers, no conflict detection, no isolation-level negotiation. It provides the
one property later milestones need — a stable read view — and nothing more.

## Two bugs the tests caught

**Updates leaked pages.** Versioned writes always allocate a new slot, and
reclamation deleted the old slot but never compacted the page. A slot delete
leaves its payload in place until compaction, so the freed bytes were never
reusable: 3 pages grew to 25 over eight update rounds. Reclamation now compacts
any page it touched.

**Every restart doubled the retained history.** Recovery rebuilds the directory
by scanning pages, then replays the log on top — appending a second version to
every chain the scan had already built. Since no snapshot can span a restart,
recovery now collapses them.

## Persistence

Index *definitions* are logged and rebuilt on open. The contents are not
persisted, and deliberately: they are derived, so reconstructing them is a scan
rather than a restore. Losing an index costs time, never data — which is the
rebuildability invariant paying off again.

Persisting index contents to avoid the startup scan is real work that remains.
On a large collection the rebuild is measured in minutes.
