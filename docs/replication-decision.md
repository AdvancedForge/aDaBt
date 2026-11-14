# Decision: no replication

**Replication and multi-process distribution are out of scope.** This is a
deliberate scope cut, not a deferral, and revisiting it means writing a new
roadmap rather than quietly reinterpreting the current one.

## Why it is not needed for the project's goals

None of the four goals in `docs/roadmap.md` asks for a replica:

- *Usable* means you could ship on it as a single node, sharded if needed.
- *Manually optimal* means the engine's ceiling is the hardware's.
- *Automatically optimal* means an expert tuning by hand adds nothing.
- *Good* means there is a workload where it is demonstrably the right answer.

Growth is handled by sharding instead (`docs/benchmarks/scale-decision.md`):
data stays resident, and more data means more shards (`RecordId % shards`).
Cross-shard writes use coordinator-decides durability (an fsynced journal,
torn-tail safe, with a documented visibility window). Hiding that window would
need distributed locking and a second failure domain, which is a different
system.

## Why it would be expensive

Replication would invalidate decisions the rest of the design rests on:

- **Residency:** each node holds its whole dataset in RAM.
- **Per-core memory:** shards own their buffer pools.
- **Crash testing:** the crash and chaos matrix assumes one machine.
- **On-disk versioning:** the superblock, catalog and WAL formats are single-node.

## What would reopen it

Either of:

- a workload, expressible in the comparison harness, where the *dataset* does
  not fit in memory but the *resident set* does; or
- an operator requirement for multi-machine high availability.

Until one of those appears, broader scope would add bloat rather than progress.
