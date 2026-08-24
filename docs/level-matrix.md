# Level x workload matrix

Engine backend, relaxed durability, 20,000 records, `--disable result_cache`.

The result cache is disabled on purpose. With it on it answers 98-100% of every
mix and masks whatever the access path underneath is doing — the first version
of this matrix measured caching and reported it as columnar performance.

```
query            lvl   queries       q/sec     p50 ns     p99 ns  idx   derived
-------------------------------------------------------------------------------
aggregate          0       256          30   32505856   41943040    0      0.0M
aggregate          1       256          30   32505856   41943040    0      0.0M
aggregate          2       256          30   32505856   41943040    1      0.2M
aggregate          4      1200         287    3407872    5505024    1      0.2M
aggregate         10      1200         299    3276800    4980736    1      1.0M
point_filter       0       256          31   31457280   39845888    0      0.0M
point_filter       1       256          31   31457280   39845888    0      0.0M
point_filter       2      1200         327    3014656    3932160    1      0.2M
point_filter       4      1200         325    3014656    4980736    1      0.2M
point_filter      10      1200         441    2097152    3407872    1      1.0M
by_identity        0      1200      642275       1216       1856    0      0.0M
by_identity        1      1200      642218       1152       1920    0      0.0M
by_identity        2      1200      723675       1152       1536    0      0.0M
by_identity        4      1200      728133       1152       1536    0      0.0M
by_identity       10      1200      892660        896       1280    0      0.9M
```

## Every win is attributable

| Workload | Jump | Level | Cause |
|---|---|---|---|
| `aggregate` | 30 → 287 q/s (9.6x) | 4 | column store, aggregate pushed down |
| `point_filter` | 31 → 327 q/s (10.5x) | 2 | `auto_index` hash index on the equality field |
| `by_identity` | 642k → 893k q/s (+39%) | 10 | direct addressing; p50 1216ns → 896ns |

**Different workloads peak at different levels.** `point_filter` gains nothing
from level 4 and `aggregate` gains nothing from level 2. A level is a preset, not
a ranking, and this is what that means in numbers.

## Three things the matrix says that are not wins

**Level 1 is worth nothing here.** With the result cache disabled, level 1 is
just the plan cache, and it moves no workload at all. Planning is cheap next to
execution. The plan cache earns its keep only when the result cache cannot
serve a query and the shape repeats with varying literals — a narrower case than
its level-1 position implies.

**Level 2 costs `aggregate` something for nothing.** `idx` goes to 1 and derived
memory to 0.2M, and throughput does not move. `auto_index` only ever proposes
*hash* indexes, because its candidates come from equality constraints — and the
aggregate mix filters with a range. So level 2 buys that workload an index it
cannot use, plus the write overhead of maintaining it.

That is a pure loss, and the current rule-based level system cannot see it: the
rule fires on "field was filtered on often enough", not on "an index would
help". A range-aware candidate generator is the obvious fix; noticing that an
existing index is never chosen by the planner is the more valuable one, and it
needs the adaptive driver.

**Level 10 costs 5x the derived memory for +4% on `aggregate`.** Worth it for
`by_identity`, plainly not for the other two. Exactly the trade the resources
axis exists to express.
