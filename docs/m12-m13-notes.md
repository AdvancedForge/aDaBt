# M12 and M13: self-specialization, and where it stops

## M12: the transition the design exists for

`Dynamic → Declared → Strict → Fixed` has been a declared dial since M0. M12 is
the machinery that lets the database *move along it* from evidence: a collection
that started schemaless and settled into a stable shape becomes directly
addressable, and its API does not change.

`a_schemaless_collection_that_settled_becomes_directly_addressable` runs the
whole arc — same `get` calls, same `query` calls, same answers, while the
collection goes from tag-length-value records to a flat array of fixed-size
slots underneath.

### Freezing costs freedom, and says so

Inference is conservative by construction: a field is frozen only if every
sampled record agreed about it. A mixed-type field, a nested value, or a field
absent from some records all prevent a `Fixed` result, and the schema degrades
to `Declared` rather than to one that would reject data already stored.

Headroom matters more than it looks. Without slack beyond the longest value
seen, the first slightly-longer record is rejected — turning an optimization
into an outage. With too much, the fixed layout wastes the space it exists to
save.

### Irreversibility is now enforced, not declared

`Reversibility::Destructive` existed as metadata from M3 and meant nothing.
Writing `freeze_schema` exposed that: a test asserting every optimization can
undo itself failed, correctly.

The rule is now real. **The adaptive driver refuses to propose any irreversible
change.** Every safety mechanism in the optimizer — measurement, retraction,
shadow comparison, canary rollback — assumes a bad decision can be taken back.
Where it cannot, the decision belongs to a human who can weigh what is being
given up. A human choosing level 8 may freeze a schema; the driver may not.

### Compiled paths

Not a JIT, and the docs say so. "Compiled" means *specialised*: a precomputed
decision that a hot shape can skip the general machinery entirely. For an
identity lookup against a directly-addressed array that removes filter
accounting, two cache probes, plan construction, the operator tree, and
batching.

Measured, 50,000 records:

| path | p50 | p99 |
|---|---:|---:|
| general query path | 1280 ns | 3200 ns |
| compiled path | 432 ns | 896 ns |

A specialisation that is not measurably faster is just more code to maintain,
so the comparison is a benchmark rather than an assumption. (The general figure
is 200 samples — taken before the specialisation threshold trips — so its p99 is
noisy; the p50 comparison is the solid one.)

## M13: what was reached, and what was not

### Single-field reads from a computed address

The Level 11 idea taken literally:

```text
address = base + id * stride + field_offset
```

A `Fixed` schema puts every field at a constant offset, so reading one field of
one record touches no other bytes: no full decode, no `Record`, no map
allocation.

| read | p50 | p99 |
|---|---:|---:|
| whole record | 432 ns | 768 ns |
| **one field** | **92 ns** | 384 ns |

14x from the general query path. What is removed is not overhead in the usual
sense — it is *generality*, the ability to answer for a record whose shape is
not known in advance.

### What was deliberately not built

**Per-core ownership and shared-nothing execution.** This is a rearchitecture,
not a feature. The engine is a single `&mut self` object; making it thread-per-
core means partitioning every structure by core, replacing the buffer pool with
per-core pools, and rewriting the write path to route by partition. Scaffolding
it would produce interfaces with no implementation behind them and a claim the
benchmarks could not support.

**Kernel-bypass networking.** There is no network layer to bypass. `adabt-server`
has a wire protocol and no listener; io_uring or a userspace stack presupposes a
server that exists.

**Lock elimination.** Mostly moot: there is no concurrency to contend for.
Telemetry was sharded in M7 because it was the one place a lock sat on the hot
path. Everything else is `&mut`, which is exclusive by construction rather than
by locking.

These are the honest boundary. The techniques are real and the roadmap for them
is sound; they are simply a different size of undertaking from everything above,
and reporting them as done because a module exists would make every number in
these documents less trustworthy.
