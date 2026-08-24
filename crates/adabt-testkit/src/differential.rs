//! The differential runner.
//!
//! Drives the same operation sequence through two `LogicalStore`
//! implementations and demands identical outcomes at every step. Once the
//! engine exists, this is run once per optimization level: identical results at
//! every level is the mechanised form of "optimization must never change
//! logical semantics".
//!
//! A divergence report carries the seed and step index, so any failure replays
//! exactly.

use crate::ops::{apply, Op, OpOutcome};
use crate::rng::Rng;
use adabt_core::store::LogicalStore;

#[derive(Debug, Clone)]
pub struct Divergence {
    pub seed: u64,
    pub step: usize,
    pub op: Op,
    pub left_name: &'static str,
    pub right_name: &'static str,
    pub left: OpOutcome,
    pub right: OpOutcome,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "divergence at step {step} (seed {seed}) on {op}\n  {ln:>12}: {l:?}\n  {rn:>12}: {r:?}\n  op: {opdbg:?}\n\
             replay with: seed={seed}, steps={step_plus}",
            step = self.step,
            seed = self.seed,
            op = self.op.name(),
            ln = self.left_name,
            rn = self.right_name,
            l = self.left,
            r = self.right,
            opdbg = self.op,
            step_plus = self.step + 1,
        )
    }
}

impl std::error::Error for Divergence {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifferentialRun {
    pub seed: u64,
    pub steps: usize,
}

/// Run `ops` through both stores, stopping at the first disagreement.
pub fn compare<A, B>(
    left: &mut A,
    right: &mut B,
    left_name: &'static str,
    right_name: &'static str,
    ops: &[Op],
    seed: u64,
) -> Result<DifferentialRun, Box<Divergence>>
where
    A: LogicalStore,
    B: LogicalStore,
{
    for (step, op) in ops.iter().enumerate() {
        let l = apply(left, op);
        let r = apply(right, op);
        if l != r {
            return Err(Box::new(Divergence {
                seed,
                step,
                op: op.clone(),
                left_name,
                right_name,
                left: l,
                right: r,
            }));
        }
    }
    Ok(DifferentialRun {
        seed,
        steps: ops.len(),
    })
}

/// Shrink a failing sequence to a shorter prefix that still diverges.
///
/// Delta-debugging on a prefix only: the operations are stateful, so an
/// arbitrary subsequence would not reproduce the same state. A prefix always
/// does, which makes this both correct and cheap.
pub fn shrink_prefix<A, B, F>(ops: &[Op], mut fresh: F) -> Vec<Op>
where
    A: LogicalStore,
    B: LogicalStore,
    F: FnMut() -> (A, B),
{
    let diverges = |n: usize, fresh: &mut F| -> bool {
        let (mut a, mut b) = fresh();
        compare(&mut a, &mut b, "l", "r", &ops[..n], 0).is_err()
    };

    let mut lo = 0usize;
    let mut hi = ops.len();
    if !diverges(hi, &mut fresh) {
        return ops.to_vec();
    }
    // Smallest prefix length that still diverges.
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if diverges(mid, &mut fresh) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    ops[..hi].to_vec()
}

/// Convenience: build a fresh pair, generate `n_ops` from `seed`, compare.
pub fn run<A, B>(
    left: &mut A,
    right: &mut B,
    left_name: &'static str,
    right_name: &'static str,
    cfg: &crate::generator::GenConfig,
    seed: u64,
    n_ops: usize,
) -> Result<DifferentialRun, Box<Divergence>>
where
    A: LogicalStore,
    B: LogicalStore,
{
    let ops = crate::generator::Generator::new(cfg, seed).take(n_ops);
    compare(left, right, left_name, right_name, &ops, seed)
}

/// Seeds for a multi-seed sweep, derived deterministically from a base seed.
pub fn seeds(base: u64, n: usize) -> Vec<u64> {
    let mut r = Rng::new(base);
    (0..n).map(|_| r.next_u64()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::GenConfig;
    use crate::reference::ReferenceStore;
    use adabt_core::error::Result;
    use adabt_core::ids::RecordId;
    use adabt_core::record::Record;
    use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};

    fn cfg() -> GenConfig {
        GenConfig::with_collections(vec![
            (
                "users".into(),
                Schema::new(
                    SchemaMode::Fixed,
                    vec![
                        FieldDef::new("id", FieldType::U64).required(),
                        FieldDef::new("balance", FieldType::I64).required(),
                        FieldDef::new("name", FieldType::Char(16)),
                    ],
                )
                .unwrap(),
            ),
            ("events".into(), Schema::dynamic()),
        ])
    }

    #[test]
    fn reference_agrees_with_itself_across_many_seeds() {
        let c = cfg();
        for seed in seeds(0xA11CE, 32) {
            let (mut a, mut b) = (ReferenceStore::new(), ReferenceStore::new());
            for (name, schema) in &c.collections {
                a.create_collection(name, schema.clone()).unwrap();
                b.create_collection(name, schema.clone()).unwrap();
            }
            run(&mut a, &mut b, "ref", "ref", &c, seed, 400).unwrap_or_else(|d| panic!("{d}"));
        }
    }

    /// A store that silently drops one specific record id. Stands in for the
    /// class of bug an optimized physical layout would introduce.
    struct BuggyStore {
        inner: ReferenceStore,
    }
    impl LogicalStore for BuggyStore {
        fn create_collection(&mut self, n: &str, s: Schema) -> Result<()> {
            self.inner.create_collection(n, s)
        }
        fn drop_collection(&mut self, n: &str) -> Result<()> {
            self.inner.drop_collection(n)
        }
        fn collection_names(&self) -> Vec<String> {
            self.inner.collection_names()
        }
        fn schema_of(&self, c: &str) -> Result<&Schema> {
            self.inner.schema_of(c)
        }
        fn insert(&mut self, c: &str, id: RecordId, r: Record) -> Result<()> {
            if id == RecordId(3) {
                return Ok(()); // the bug: acknowledged but not stored
            }
            self.inner.insert(c, id, r)
        }
        fn get(&mut self, c: &str, id: RecordId) -> Result<Option<Record>> {
            self.inner.get(c, id)
        }
        fn update(&mut self, c: &str, id: RecordId, r: Record) -> Result<bool> {
            self.inner.update(c, id, r)
        }
        fn delete(&mut self, c: &str, id: RecordId) -> Result<bool> {
            self.inner.delete(c, id)
        }
        fn scan(&mut self, c: &str) -> Result<Vec<(RecordId, Record)>> {
            self.inner.scan(c)
        }
        fn count(&mut self, c: &str) -> Result<usize> {
            self.inner.count(c)
        }
    }

    fn seeded_pair(c: &GenConfig) -> (ReferenceStore, BuggyStore) {
        let (mut a, mut b) = (
            ReferenceStore::new(),
            BuggyStore {
                inner: ReferenceStore::new(),
            },
        );
        for (name, schema) in &c.collections {
            a.create_collection(name, schema.clone()).unwrap();
            b.create_collection(name, schema.clone()).unwrap();
        }
        (a, b)
    }

    #[test]
    fn a_planted_bug_is_detected() {
        let c = cfg();
        let (mut a, mut b) = seeded_pair(&c);
        let err = run(&mut a, &mut b, "ref", "buggy", &c, 4242, 2_000)
            .expect_err("planted bug went undetected");
        assert_eq!(err.seed, 4242);
        // The message must name the seed and step so the failure replays.
        let msg = err.to_string();
        assert!(msg.contains("4242"), "{msg}");
    }

    #[test]
    fn shrinking_finds_a_short_reproducer() {
        let c = cfg();
        let ops = crate::generator::Generator::new(&c, 4242).take(2_000);
        let short = shrink_prefix::<ReferenceStore, BuggyStore, _>(&ops, || seeded_pair(&c));
        assert!(short.len() < ops.len(), "shrinking made no progress");
        // Still reproduces.
        let (mut a, mut b) = seeded_pair(&c);
        assert!(compare(&mut a, &mut b, "ref", "buggy", &short, 4242).is_err());
        // And is minimal: one op shorter no longer reproduces.
        let (mut a, mut b) = seeded_pair(&c);
        assert!(compare(
            &mut a,
            &mut b,
            "ref",
            "buggy",
            &short[..short.len() - 1],
            4242
        )
        .is_ok());
    }
}
