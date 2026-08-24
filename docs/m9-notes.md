# M9: cost-based optimization

Two things: estimates that learn from outcomes, and decisions made per scope.

## The repair for M8's oscillation

M8 ended with the driver contradicting itself. It dropped an unused index on
**evidence** (the planner never chose it), then re-added it three cycles later
on **estimate** (which claimed a 70% latency win), forever. A fifty-cycle
cooldown stopped the flapping without addressing the cause: the estimate never
learned it was wrong.

`CostModel` measures every applied change and corrects the prior. After eight
observations of no effect, an estimate claiming a 0.3x latency ratio reports
~1.0 and stops scoring above the threshold to apply. The cooldown remains as a
backstop for the window before enough measurements exist.

## What the measurement is and is not

Attribution is **crude on purpose**. Comparing before and after on a live system
confounds the change with everything else that moved: traffic shape, data
growth, another optimization applied in the same window. The model compensates
by weighting a single observation very little (~1/8) and by raising confidence
only while observations *agree* with each other.

Scattered results mean the effect depends on something unmodelled, and averaging
them harder does not make the average more true — so spread suppresses
confidence rather than being averaged away. Learned confidence is capped at 0.85
and can never reach certainty, because a confounded before-and-after does not
earn it.

Controlled attribution needs the candidate and the baseline running against the
same state at the same time. That is shadow execution, and it is M11.

## Per-scope decisions

M8 operated everything at `"global"`, including `auto_index`, whose metadata
declared `PerField`. Retracting it dropped *every* index rather than the one not
earning its keep, and it could only retract when all of them were dead at once.

`Optimization::candidate_scopes` now yields one scope per field worth indexing,
and `plan_enable`/`plan_disable` take the scope they are acting on. Both drivers
expand through the same function, so manual and adaptive cannot disagree about
what a scope is — a level names an optimization, not a scope, and gets expanded
the same way the adaptive driver expands it.

## The M7 gap, closed

`auto_index` proposed hash indexes for every filtered field, including fields
filtered with ranges — which a hash index cannot serve at all. The M7 matrix
measured that as a pure loss: an index built, maintained, and never chosen.

Telemetry now records *how* a field was filtered (`Event::FieldFiltered` carries
`equality`), and the index kind follows the evidence: mostly-equality gets a
hash index, anything else gets an ordered one because it serves both.

A predicate under `Or` contributes no equality constraint — correctly, since the
planner cannot serve it from an index either — so such a field falls to the
ordered structure. That is the conservative answer rather than the sharp one,
and it is the right way round: an ordered index is more expensive but always
usable.
