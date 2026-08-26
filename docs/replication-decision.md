# Replication: permanently out of scope for 100%

**Decision:** replication and multi-process distribution remain **not built and not on the roadmap**, even at 100% ×4. This is a deliberate scope cut, not a deferral.

Why the number stays honest at 100% without it:

* The four finish lines are A "you could ship on it" (single-node, sharded if needed), B "the engine's ceiling is the hardware's ceiling" (measured against SQLite/RocksDB/Postgres witnesses, memory bandwidth, `connection_scale` gate), C "the expert adds nothing" (17 optimizations, level-reachable, KEEP_SCORE retraction, shadow-copy proof, M32 compound reasoning), D "there is a workload where it is demonstrably the right answer" (4/8 wins published, 3-witness harness fail-fast, `adabt-cli`/`adabt-ffi`/TLS/`adversarial.rs`/semver). None asks for a replica.

* Sharding already is the growth story (`docs/scale-decision.md`): resident by design, more data → more shards (`RecordId % shards`), not a paged directory. Cross-shard **coordinator-decides durability** (`XSH1` journal, torn-tail safe, windowed visibility) is the honest single-machine answer; hiding the window needs distributed locking and a second failure domain, which is a different system.

* Cost of being honest: adding replication would invalidate Stage 3 (residency), Stage 4 (per-core memory), Stage 7 (crash/chaos matrix), and the version gate (superblock/catalog/wal are single-node). Revisit triggers are therefore explicit: a comparison-expressible workload where the *dataset* does not fit but the *resident set* does, or an operator requirement for multi-machine HA. Until one fires, broader scope is bloat, not progress.

If a future revision revisits, it will be a new roadmap, not a quiet reinterpretation of 100%.
