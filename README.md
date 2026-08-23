# aDaBt

A database whose **logical interface stays fixed while its physical
implementation ranges from completely conventional to radically specialized** —
and where the choice of specialization can be made by a human or, later, by the
database itself, through the same mechanism.

Status: **M0 — foundation.** No storage engine yet. See
`~/.claude/plans/alright-you-will-be-linked-peacock.md` for the full plan.

## Layout

| Crate | Role |
|---|---|
| `adabt-core` | Logical vocabulary: `Value`, `Record`, `Schema`, ids, `Policy`, and the `LogicalStore` contract. No storage or execution code. |
| `adabt-telemetry` | Probes, events, log-linear latency histograms. Compiles away entirely when disabled. |
| `adabt-testkit` | Reference model, deterministic op generator, differential runner. |
| `adabt-bench` | Workloads and the level x workload measurement harness. |

## Three ideas that carry the design

**The schema-mode spectrum.** How rigid a collection is, is a declared dial:
`Dynamic → Declared → Strict → Fixed`. `Dynamic` is the freedom endpoint;
`Fixed` gives constant-size records, which is the precondition that makes
`address = BASE + id * RECORD_SIZE` legal at Level 10+. The logical call is
`store.get("users", id)` in every mode — only the physical path differs.

**Derived representations are rebuildable.** Each collection will hold one
authoritative *primary* representation and N *derived* ones (indexes, caches,
column stores, fixed arrays), every derived one fully rebuildable from the
primary. Adding one can never lose data, rollback is a drop, and any divergence
is a bug to hard-fail on rather than a data-loss event to reconcile.

**Guarantees filter; priorities score.** `durability: strict` makes
async-durability techniques *invisible* to the optimizer — not merely expensive.
Constraints are hard feasibility; only the surviving set gets scored against the
speed/resources/freedom priorities.

## Build

Requires a C linker (`build-essential` on Debian/Ubuntu).

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Build artefacts go to `~/.cargo-target/adabt` (see `.cargo/config.toml`): the
source lives on a 9p-mounted Windows drive where Cargo's I/O is 5-15x slower
than native.

## Benchmarks

```sh
cargo build --profile bench-fast -p adabt-bench   # release codegen, fast to compile
B=~/.cargo-target/adabt/bench-fast/adabt-bench

$B list
$B run    --workload point_lookup --size 100000 --ops 200000
$B matrix --size 50000 --ops 300000 --duration 4 --out results.json
```

Every run is bounded by **both** an op count and a wall-clock deadline
(`--duration`, default 10s), and reports the ops it actually completed. An
op-count budget alone is the wrong bound for a matrix: an unbounded scan costs
four orders of magnitude more than a point get, so a count that is brisk for one
workload runs for hours on another.

At M0 every run is against the reference model at level 0 — a deliberately
unoptimised `BTreeMap` floor that Level 0 must not fall below. The matrix
rejects non-zero levels rather than printing identical rows under different
labels, which would read as "optimization made no difference" instead of "not
built yet".
