# M11: online experiments

Shadow execution, and the lifecycle that consumes it.

## Why this needed M10

The M9 cost model corrects estimates from a before-and-after on a live system,
which confounds the change with everything else that moved. Shadow execution is
the controlled version: both paths answer the *same query* against the *same
snapshot*, so a difference is attributable to the change and nothing else.

That is impossible without stable reads. `shadow_reads_are_taken_against_a_
stable_snapshot` asserts the property directly: a scan under a snapshot returns
identical rows while 500 records are rewritten underneath it.

## Divergence is fatal, not thresholded

Any difference in results aborts immediately — not "if it exceeds a threshold",
not "if it persists". Every derived representation is rebuildable from the
primary, so a derived representation disagreeing with the primary is *always* a
bug, never a reconcilable difference. Tolerating one instance would be
tolerating silent corruption.

Latency, by contrast, is expected to differ. That is the entire point.

## Shadow is not canary

`trial()` returns the **baseline** result to the caller, always. The candidate
is being evaluated, not trusted. A shadow that served candidate results would be
a canary, and keeping them distinct is the whole safety argument: nothing a
candidate produces reaches a user until it has been shown to agree.

The lifecycle enforces the ordering — `a_confirmed_candidate_ramps_through_
shadow_to_promotion` asserts that the shadow phase precedes any canary phase, so
traffic never moves before correctness is established.

## Credibility, deliberately blunt

`improvement_is_credible` demands a minimum trial count *and* a minimum effect
size, rather than running a significance test. With paired trials against
identical state the noise is mostly scheduling, and a t-test would wave through
marginal effects on a technicality. Requiring a large effect from a decent
sample rejects exactly the cases not worth a rebuild.

`shadow_execution_shows_a_useless_index_is_no_faster` is the test that matters
here: a hash index on a range-filtered field is perfectly *correct* and entirely
pointless, and shadow execution reports it as such — which is what the M7 matrix
measured the hard way.

## What remains

The two paths run as two engines over identical data, which isolates the
comparison cleanly but is not how a deployment would work. Running both
representations inside one engine, with traffic actually split at the canary
percentages, is the remaining piece.
