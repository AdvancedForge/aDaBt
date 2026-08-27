# M8: the rule-based adaptive driver

`AdaptiveDriver` observes telemetry, scores candidates against the policy, and
proposes changes through the same `OptimizationController` the manual driver
uses. Deliberately rule-based: the estimates it scores are hand-written guesses,
and a model calibrated against measurements is M9's job. Dressing guesses up as
a model would only make them harder to doubt.

## The claim, tested

`different_priorities_converge_on_different_configurations`: the same workload,
the same data, two policies, two physical configurations. A
`resources: 10, speed: 2` database chooses record compression; a
`speed: 10, resources: 2` one refuses it because of its latency cost. Neither
changes a single answer (`adaptation_never_changes_an_answer`).

That assertion is the premise of the whole project, and until M7 added an
optimization that trades resources *down* it could not have been written.

## Scoring, and a flaw worth recording

`score()` is the only place a `CostEstimate` vector collapses to a number, and
it lives where the policy is in scope. Guarantees and constraints never reach
it — they are hard filters applied earlier, so scoring only ranks what is
already permitted and affordable.

The first version was wrong in a way that would have made priorities
meaningless. Latency gain is naturally bounded (a change cannot be more than
100% faster), but the resource term was raw gigabytes and unbounded. A six-
gigabyte saving swamped any possible speed weighting, so a `speed: 10` policy
kept selecting whatever saved the most memory. The axes have to be
**commensurate** before the weights mean anything.

The fix exposed a second flaw. Expressing resources as a fraction of a fixed
8 GiB reference makes small databases un-optimizable: halving a 600KB store
registers as noise against that yardstick while its CPU cost is charged in full.
The scale is now the database's actual footprint — "fraction of what this
database costs" means the same thing at every size — with a stated `max_ram`
ceiling overriding it, because a user who names a budget means that budget.

## Retraction, and why it is the harder half

An optimizer that only adds is a ratchet. The M7 matrix showed what that costs:
level 2 built an index the planner never chose and paid memory and write
maintenance for it forever, and no threshold on "was this field filtered often
enough" could have noticed — the evidence needed is *which access paths were
chosen*, which is now recorded as `Event::IndexUsed`.

Implementing retraction surfaced the sharpest finding of the milestone.

The driver dropped the unused index on **evidence** (the planner never chose it
over 2,400 filtered queries), then re-added it three cycles later on
**estimate** (which claims a 70% p50 improvement), then dropped it again. It
oscillated, and every flip paid a rebuild.

The estimate and the measurement disagreed, and nothing reconciled them.

**Evidence outranks estimate** is the only defensible ordering: one is a guess
about what would happen, the other is a record of what did. An evidence-based
retraction now blocks re-proposal for 50 cycles rather than 3. That is a
mitigation, not a fix — the real repair is correcting the estimate from the
observed outcome, which is precisely what M9's calibrated cost model exists to
do. The oscillation is the clearest argument yet for building it.

## Stability

Three mechanisms, because an oscillating optimizer is strictly worse than none:

- **Minimum observation window** (500 operations) before acting at all, so the
  driver reacts to a workload rather than to startup.
- **Per-optimization cooldown** (3 cycles ordinarily, 50 after an evidence-based
  retraction).
- **At most 2 changes per cycle**, so each one's effect stays separable in the
  decision log rather than tangled with four others.

`the_driver_settles_rather_than_oscillating` runs 14 cycles and asserts the last
four configurations are identical.

## Known limitation

The driver operates every optimization at `"global"` scope, including
`auto_index`, whose metadata declares `ScopeKind::PerField`. So dropping
`auto_index` drops *every* index rather than the one that is not earning its
keep, and it only retracts when all of them are unused. Per-scope decisions are
M9 work; the coarse scoping is why the retraction test has to make the whole
index set go unused rather than just one of them.
