//! Compiled predicate evaluation: a small stack machine over `Expr`.
//!
//! # Why this exists
//!
//! `Expr::evaluate` walks the tree once per row. For a filter over a large
//! collection that is the innermost loop in the engine, and it pays three
//! costs per row that do not depend on the row at all: recursive descent
//! through `Box`ed nodes, a `match` on the node kind at every step, and —
//! for `Like` — re-parsing the pattern string from scratch on every single
//! call (`like_matches` calls `parse_like_pattern` internally).
//!
//! Compiling to a flat instruction sequence moves all three off the hot
//! path: the tree is walked once, at compile time; the pattern is parsed
//! once, at compile time; and evaluation becomes a loop over a `Vec` with no
//! pointer chasing.
//!
//! # The invariant, and how it is kept
//!
//! **A compiled program must agree with `Expr::evaluate` on every input,
//! including the unknown cases.** Three-valued logic is where a
//! reimplementation quietly diverges — a null operand, a missing field, a
//! type mismatch — so this is not left to inspection: `mod differential`
//! generates thousands of random expression/record pairs and asserts both
//! evaluators return the identical `Truth`. That is the same discipline the
//! rest of this project applies to two *stores*, applied here to two
//! *evaluators*, which is exactly what the milestone plan asked for
//! ("the differential runner compares two stores, not two executors").
//!
//! # Not a JIT
//!
//! No machine code is generated. This is a bytecode interpreter, and the
//! honest description of the win is that it removes per-row interpretive
//! overhead that was never row-dependent — not that it approaches native
//! speed. `compiled.rs` in the engine makes the same distinction for the
//! same reason.

use crate::expr::{matches_from, parse_like_pattern, CmpOp, Expr, Piece, Truth};
use adabt_core::record::Record;
use adabt_core::value::{checked_arith, ArithOp, Value};
use std::borrow::Cow;

/// One instruction.
///
/// Two operand stacks rather than one tagged stack: which stack an
/// instruction touches is fixed by the instruction, so the split is known at
/// compile time and costs no per-step discriminant check.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    /// Push a constant onto the value stack.
    PushVal(Value),
    /// Push "no value" — what a `value()` of `None` means in the tree-walker.
    PushMissing,
    /// Push a field's value, or missing when the record lacks it.
    LoadField(u32),
    PushTruth(Truth),
    Arith(ArithOp),
    /// Pop two values, push a truth.
    Compare(CmpOp),
    /// Pop a value, push a truth — the tree-walker's "a bare literal or field
    /// in predicate position is its boolean value, or unknown".
    ToTruth,
    Not,
    IsNull,
    IsNotNull,
    /// Pop `n` list elements and then the needle; push a truth.
    In(u32),
    /// Pop a value, match against a pre-parsed pattern, push a truth.
    Like(u32),
    /// Pop two truths, push their three-valued conjunction.
    FoldAnd,
    FoldOr,
    /// Short-circuit: if the truth on top is already the absorbing element,
    /// jump. `False` absorbs `And`, `True` absorbs `Or`, so the accumulator
    /// already *is* the answer and the rest cannot change it.
    JumpIfFalse(u32),
    JumpIfTrue(u32),
}

/// A predicate compiled for repeated evaluation against many records.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    ops: Vec<Op>,
    /// Field names, interned so an instruction carries an index rather than a
    /// `String` — the lookup still hashes the name, but the instruction
    /// stream stays compact and cloning a `Program` does not clone names.
    fields: Vec<String>,
    /// `Like` patterns, parsed once here instead of per row.
    patterns: Vec<Vec<Piece>>,
}

impl Program {
    /// Compile a predicate.
    pub fn compile(e: &Expr) -> Program {
        let mut p = Program {
            ops: Vec::new(),
            fields: Vec::new(),
            patterns: Vec::new(),
        };
        p.emit_pred(e);
        p
    }

    /// Instructions emitted, for tests and `EXPLAIN`-style reporting.
    pub fn len(&self) -> usize {
        self.ops.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    fn intern_field(&mut self, name: &str) -> u32 {
        if let Some(i) = self.fields.iter().position(|f| f == name) {
            return i as u32;
        }
        self.fields.push(name.to_string());
        (self.fields.len() - 1) as u32
    }

    fn intern_pattern(&mut self, pattern: &str) -> u32 {
        let parsed = parse_like_pattern(pattern);
        if let Some(i) = self.patterns.iter().position(|p| *p == parsed) {
            return i as u32;
        }
        self.patterns.push(parsed);
        (self.patterns.len() - 1) as u32
    }

    /// Compile `e` in *value* position — mirrors `Expr::value`, including its
    /// `_ => None` fallthrough for anything that is not value-producing.
    fn emit_value(&mut self, e: &Expr) {
        match e {
            Expr::Literal(v) => self.ops.push(Op::PushVal(v.clone())),
            Expr::Field(name) => {
                let i = self.intern_field(name);
                self.ops.push(Op::LoadField(i));
            }
            Expr::Arith { op, lhs, rhs } => {
                self.emit_value(lhs);
                self.emit_value(rhs);
                self.ops.push(Op::Arith(*op));
            }
            // Every other node yields no value, exactly as `Expr::value` does.
            _ => self.ops.push(Op::PushMissing),
        }
    }

    /// Compile `e` in *predicate* position — mirrors `Expr::evaluate`.
    fn emit_pred(&mut self, e: &Expr) {
        match e {
            Expr::Literal(Value::Bool(b)) => {
                self.ops
                    .push(Op::PushTruth(if *b { Truth::True } else { Truth::False }))
            }
            Expr::Literal(Value::Null) => self.ops.push(Op::PushTruth(Truth::Unknown)),
            Expr::Literal(_) | Expr::Field(_) | Expr::Arith { .. } => {
                self.emit_value(e);
                self.ops.push(Op::ToTruth);
            }
            Expr::Compare { op, lhs, rhs } => {
                self.emit_value(lhs);
                self.emit_value(rhs);
                self.ops.push(Op::Compare(*op));
            }
            Expr::Not(inner) => {
                self.emit_pred(inner);
                self.ops.push(Op::Not);
            }
            Expr::IsNull(inner) => {
                self.emit_value(inner);
                self.ops.push(Op::IsNull);
            }
            Expr::IsNotNull(inner) => {
                self.emit_value(inner);
                self.ops.push(Op::IsNotNull);
            }
            Expr::In { needle, list } => {
                self.emit_value(needle);
                for item in list {
                    self.emit_value(item);
                }
                self.ops.push(Op::In(list.len() as u32));
            }
            Expr::Like { text, pattern } => {
                self.emit_value(text);
                let i = self.intern_pattern(pattern);
                self.ops.push(Op::Like(i));
            }
            Expr::And(parts) => self.emit_fold(parts, true),
            Expr::Or(parts) => self.emit_fold(parts, false),
        }
    }

    /// `And`/`Or` over `parts`, folded left with a short-circuit jump after
    /// each step.
    ///
    /// An empty list matches the tree-walker's own answer for one: its loop
    /// body never runs and `saw_unknown` stays false, so `And([])` is `True`
    /// and `Or([])` is `False`.
    fn emit_fold(&mut self, parts: &[Expr], is_and: bool) {
        if parts.is_empty() {
            self.ops.push(Op::PushTruth(if is_and {
                Truth::True
            } else {
                Truth::False
            }));
            return;
        }
        let mut patches = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            self.emit_pred(part);
            if i > 0 {
                self.ops.push(if is_and { Op::FoldAnd } else { Op::FoldOr });
            }
            // No point testing after the last operand: there is nothing left
            // to skip.
            if i + 1 < parts.len() {
                patches.push(self.ops.len());
                self.ops.push(if is_and {
                    Op::JumpIfFalse(u32::MAX)
                } else {
                    Op::JumpIfTrue(u32::MAX)
                });
            }
        }
        let end = self.ops.len() as u32;
        for at in patches {
            match &mut self.ops[at] {
                Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => *t = end,
                other => unreachable!("patched a non-jump instruction: {other:?}"),
            }
        }
    }

    /// Evaluate against one record.
    ///
    /// Returns the same `Truth` `Expr::evaluate` would for the expression this
    /// was compiled from — see `mod differential` for the evidence.
    pub fn run<'a>(&'a self, rec: &'a Record) -> Truth {
        // `Cow` rather than `Value`: a literal borrows from the program and a
        // field value borrows from the record, so the common path — load a
        // field, compare it against a literal, discard both — now copies
        // nothing at all. Only arithmetic, which genuinely produces a new
        // value, allocates. This is the zero-copy read path applied where it
        // actually pays: the innermost per-row loop in the engine.
        let mut vals: Vec<Option<Cow<'a, Value>>> = Vec::with_capacity(8);
        let mut truths: Vec<Truth> = Vec::with_capacity(8);
        let mut pc = 0usize;
        while pc < self.ops.len() {
            match &self.ops[pc] {
                Op::PushVal(v) => vals.push(Some(Cow::Borrowed(v))),
                Op::PushMissing => vals.push(None),
                Op::LoadField(i) => {
                    vals.push(rec.get(&self.fields[*i as usize]).map(Cow::Borrowed));
                }
                Op::PushTruth(t) => truths.push(*t),
                Op::Arith(op) => {
                    let b = vals.pop().flatten();
                    let a = vals.pop().flatten();
                    vals.push(match (a, b) {
                        (Some(a), Some(b)) => checked_arith(*op, &a, &b).map(Cow::Owned),
                        _ => None,
                    });
                }
                Op::Compare(op) => {
                    let b = vals.pop().flatten();
                    let a = vals.pop().flatten();
                    truths.push(match (a, b) {
                        (Some(a), Some(b)) if !a.is_null() && !b.is_null() => {
                            if op.apply(a.cmp(&b)) {
                                Truth::True
                            } else {
                                Truth::False
                            }
                        }
                        _ => Truth::Unknown,
                    });
                }
                Op::ToTruth => {
                    truths.push(match vals.pop().flatten().as_deref() {
                        Some(Value::Bool(true)) => Truth::True,
                        Some(Value::Bool(false)) => Truth::False,
                        _ => Truth::Unknown,
                    });
                }
                Op::Not => {
                    let t = truths.pop().unwrap_or(Truth::Unknown);
                    truths.push(t.not());
                }
                Op::IsNull => {
                    let v = vals.pop().flatten();
                    truths.push(match v.as_deref() {
                        None | Some(Value::Null) => Truth::True,
                        _ => Truth::False,
                    });
                }
                Op::IsNotNull => {
                    let v = vals.pop().flatten();
                    truths.push(match v.as_deref() {
                        None | Some(Value::Null) => Truth::False,
                        _ => Truth::True,
                    });
                }
                Op::In(n) => {
                    let at = vals.len() - *n as usize;
                    let list: Vec<Option<Cow<'a, Value>>> = vals.split_off(at);
                    let needle = vals.pop().flatten().filter(|v| !v.is_null());
                    truths.push(match needle {
                        None => Truth::Unknown,
                        Some(n) => {
                            let mut saw_unknown = false;
                            let mut found = false;
                            for item in list {
                                match item {
                                    Some(v) if !v.is_null() => {
                                        if *v == *n {
                                            found = true;
                                            break;
                                        }
                                    }
                                    _ => saw_unknown = true,
                                }
                            }
                            if found {
                                Truth::True
                            } else if saw_unknown {
                                Truth::Unknown
                            } else {
                                Truth::False
                            }
                        }
                    });
                }
                Op::Like(i) => {
                    let v = vals.pop().flatten();
                    truths.push(match v.as_deref() {
                        Some(Value::Str(s)) => {
                            let t: Vec<char> = s.chars().collect();
                            if matches_from(&t, &self.patterns[*i as usize]) {
                                Truth::True
                            } else {
                                Truth::False
                            }
                        }
                        _ => Truth::Unknown,
                    });
                }
                Op::FoldAnd => {
                    let b = truths.pop().unwrap_or(Truth::Unknown);
                    let a = truths.pop().unwrap_or(Truth::Unknown);
                    truths.push(and3(a, b));
                }
                Op::FoldOr => {
                    let b = truths.pop().unwrap_or(Truth::Unknown);
                    let a = truths.pop().unwrap_or(Truth::Unknown);
                    truths.push(or3(a, b));
                }
                Op::JumpIfFalse(t) => {
                    if truths.last() == Some(&Truth::False) {
                        pc = *t as usize;
                        continue;
                    }
                }
                Op::JumpIfTrue(t) => {
                    if truths.last() == Some(&Truth::True) {
                        pc = *t as usize;
                        continue;
                    }
                }
            }
            pc += 1;
        }
        truths.pop().unwrap_or(Truth::Unknown)
    }

    /// Whether a record satisfies the predicate — the compiled counterpart of
    /// `Expr::matches`, and like it, only `True` passes.
    pub fn matches(&self, rec: &Record) -> bool {
        self.run(rec).is_true()
    }
}

/// Three-valued conjunction. `False` absorbs; otherwise any `Unknown` wins.
fn and3(a: Truth, b: Truth) -> Truth {
    match (a, b) {
        (Truth::False, _) | (_, Truth::False) => Truth::False,
        (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
        _ => Truth::True,
    }
}

/// Three-valued disjunction. `True` absorbs; otherwise any `Unknown` wins.
fn or3(a: Truth, b: Truth) -> Truth {
    match (a, b) {
        (Truth::True, _) | (_, Truth::True) => Truth::True,
        (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
        _ => Truth::False,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> Record {
        Record::new()
            .with("a", 5i64)
            .with("s", "hello")
            .with("b", true)
    }

    fn agree(e: &Expr, r: &Record) {
        assert_eq!(
            Program::compile(e).run(r),
            e.evaluate(r),
            "compiled and interpreted disagreed on {e:?}"
        );
    }

    #[test]
    fn simple_comparisons_agree() {
        for e in [
            Expr::eq("a", 5i64),
            Expr::eq("a", 6i64),
            Expr::cmp("a", CmpOp::Lt, 10i64),
            Expr::eq("s", "hello"),
            Expr::eq("missing", 1i64),
        ] {
            agree(&e, &rec());
        }
    }

    #[test]
    fn three_valued_cases_agree() {
        let r = Record::new().with("n", Value::Null).with("a", 1i64);
        for e in [
            Expr::eq("n", 1i64),
            Expr::eq("absent", 1i64),
            Expr::IsNull(Box::new(Expr::field("n"))),
            Expr::IsNull(Box::new(Expr::field("absent"))),
            Expr::IsNotNull(Box::new(Expr::field("a"))),
            Expr::Not(Box::new(Expr::eq("n", 1i64))),
        ] {
            agree(&e, &r);
        }
    }

    #[test]
    fn and_or_short_circuit_without_changing_the_answer() {
        let r = rec();
        // False first: the jump fires and the rest is skipped, and the
        // answer must still be exactly what full evaluation gives.
        agree(
            &Expr::And(vec![Expr::eq("a", 999i64), Expr::eq("missing", 1i64)]),
            &r,
        );
        agree(
            &Expr::Or(vec![Expr::eq("a", 5i64), Expr::eq("missing", 1i64)]),
            &r,
        );
        // Unknown mixed with True must stay Unknown, not collapse to True.
        agree(
            &Expr::And(vec![Expr::eq("a", 5i64), Expr::eq("miss", 1i64)]),
            &r,
        );
        agree(
            &Expr::Or(vec![Expr::eq("a", 999i64), Expr::eq("miss", 1i64)]),
            &r,
        );
    }

    #[test]
    fn an_empty_and_is_true_and_an_empty_or_is_false_as_in_the_walker() {
        let r = rec();
        agree(&Expr::And(vec![]), &r);
        agree(&Expr::Or(vec![]), &r);
    }

    #[test]
    fn arithmetic_in_and_like_agree() {
        let r = rec();
        agree(
            &(Expr::field("a") + Expr::lit(10i64)).compare(CmpOp::Gt, Expr::lit(12i64)),
            &r,
        );
        agree(&Expr::field("s").in_values(["hello", "world"]), &r);
        agree(&Expr::field("s").in_values(["nope"]), &r);
        agree(&Expr::field("s").like("hel%"), &r);
        agree(&Expr::field("s").like("h_llo"), &r);
        agree(&Expr::field("a").like("x%"), &r);
    }

    #[test]
    fn a_like_pattern_is_parsed_once_at_compile_time_not_per_row() {
        // Two `Like`s on the same pattern intern to one entry — the evidence
        // that patterns are compiled rather than re-parsed per evaluation.
        let e = Expr::And(vec![
            Expr::field("s").like("a%b"),
            Expr::field("s").like("a%b"),
        ]);
        assert_eq!(Program::compile(&e).patterns.len(), 1);
    }

    #[test]
    fn repeated_fields_are_interned_once() {
        let e = Expr::And(vec![Expr::eq("a", 1i64), Expr::eq("a", 2i64)]);
        assert_eq!(Program::compile(&e).fields.len(), 1);
    }

    #[test]
    fn nested_and_or_agree() {
        let r = rec();
        agree(
            &Expr::And(vec![
                Expr::Or(vec![Expr::eq("a", 5i64), Expr::eq("a", 6i64)]),
                Expr::Not(Box::new(Expr::eq("s", "bye"))),
            ]),
            &r,
        );
    }
}

/// The load-bearing evidence for this module: thousands of generated
/// expression/record pairs, each evaluated both ways and required to agree.
///
/// Written as a generator rather than a fixed list because the divergences
/// worth fearing are the ones nobody thought to write down — a null inside a
/// nested `Or`, an arithmetic overflow feeding a comparison, an `In` whose
/// list mixes present and missing fields. A hand-written suite tests the
/// cases its author already considered; this tests the ones they did not.
#[cfg(test)]
mod differential {
    use super::*;

    /// A small deterministic PRNG, so a failure reproduces exactly from its
    /// seed rather than only sometimes.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn gen_value(rng: &mut Rng) -> Value {
        match rng.below(6) {
            0 => Value::I64(rng.below(10) as i64),
            1 => Value::Str(format!("s{}", rng.below(4))),
            2 => Value::Bool(rng.below(2) == 1),
            3 => Value::Null,
            4 => Value::F64(rng.below(10) as f64),
            _ => Value::U64(rng.below(10)),
        }
    }

    const FIELDS: [&str; 4] = ["a", "b", "s", "absent"];

    fn gen_record(rng: &mut Rng) -> Record {
        let mut r = Record::new();
        // "absent" is deliberately never set, so missing-field paths are
        // exercised as often as present ones.
        for f in ["a", "b", "s"] {
            if rng.below(4) > 0 {
                r.set(f, gen_value(rng));
            }
        }
        r
    }

    fn gen_value_expr(rng: &mut Rng, depth: u32) -> Expr {
        if depth == 0 {
            return match rng.below(2) {
                0 => Expr::Literal(gen_value(rng)),
                _ => Expr::field(FIELDS[rng.below(4) as usize]),
            };
        }
        match rng.below(3) {
            0 => Expr::Literal(gen_value(rng)),
            1 => Expr::field(FIELDS[rng.below(4) as usize]),
            _ => {
                let op = match rng.below(4) {
                    0 => ArithOp::Add,
                    1 => ArithOp::Sub,
                    2 => ArithOp::Mul,
                    _ => ArithOp::Div,
                };
                Expr::Arith {
                    op,
                    lhs: Box::new(gen_value_expr(rng, depth - 1)),
                    rhs: Box::new(gen_value_expr(rng, depth - 1)),
                }
            }
        }
    }

    fn gen_pred(rng: &mut Rng, depth: u32) -> Expr {
        if depth == 0 {
            let op = match rng.below(6) {
                0 => CmpOp::Eq,
                1 => CmpOp::Ne,
                2 => CmpOp::Lt,
                3 => CmpOp::Le,
                4 => CmpOp::Gt,
                _ => CmpOp::Ge,
            };
            return Expr::Compare {
                op,
                lhs: Box::new(gen_value_expr(rng, 1)),
                rhs: Box::new(gen_value_expr(rng, 1)),
            };
        }
        match rng.below(8) {
            0 => {
                let n = rng.below(3) as usize;
                Expr::And((0..n).map(|_| gen_pred(rng, depth - 1)).collect())
            }
            1 => {
                let n = rng.below(3) as usize;
                Expr::Or((0..n).map(|_| gen_pred(rng, depth - 1)).collect())
            }
            2 => Expr::Not(Box::new(gen_pred(rng, depth - 1))),
            3 => Expr::IsNull(Box::new(gen_value_expr(rng, 1))),
            4 => Expr::IsNotNull(Box::new(gen_value_expr(rng, 1))),
            5 => {
                let n = 1 + rng.below(3) as usize;
                Expr::In {
                    needle: Box::new(gen_value_expr(rng, 1)),
                    list: (0..n).map(|_| gen_value_expr(rng, 1)).collect(),
                }
            }
            6 => Expr::Like {
                text: Box::new(gen_value_expr(rng, 1)),
                pattern: ["s%", "_1", "s1", "%", "a\\%b"][rng.below(5) as usize].to_string(),
            },
            _ => gen_pred(rng, 0),
        }
    }

    #[test]
    fn the_compiled_and_interpreted_evaluators_never_disagree() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut checked = 0u32;
        for _ in 0..4_000 {
            let e = gen_pred(&mut rng, 3);
            let r = gen_record(&mut rng);
            let program = Program::compile(&e);
            assert_eq!(
                program.run(&r),
                e.evaluate(&r),
                "compiled and interpreted disagreed\n  expr:   {e:?}\n  record: {r:?}"
            );
            checked += 1;
        }
        assert_eq!(checked, 4_000);
    }

    #[test]
    fn every_generated_program_leaves_exactly_one_truth_on_the_stack() {
        // A stack machine that under- or over-flows would still often return
        // a plausible answer, because `run` pops with a default. This asserts
        // the stack discipline itself rather than only the answer.
        let mut rng = Rng(0x1234_5678_9ABC_DEF0);
        for _ in 0..2_000 {
            let e = gen_pred(&mut rng, 3);
            let r = gen_record(&mut rng);
            let p = Program::compile(&e);
            assert_eq!(
                p.stack_depth_after(&r),
                (0, 1),
                "unbalanced stacks for {e:?}"
            );
        }
    }
}

#[cfg(test)]
impl Program {
    /// `(values left, truths left)` after a run — a test hook for asserting
    /// the machine's stack discipline, not part of the public surface.
    fn stack_depth_after(&self, rec: &Record) -> (usize, usize) {
        let mut vals: Vec<Option<Value>> = Vec::new();
        let mut truths: Vec<Truth> = Vec::new();
        let mut pc = 0usize;
        while pc < self.ops.len() {
            let before = (vals.len(), truths.len());
            let _ = before;
            match &self.ops[pc] {
                Op::PushVal(v) => vals.push(Some(v.clone())),
                Op::PushMissing => vals.push(None),
                Op::LoadField(i) => vals.push(rec.get(&self.fields[*i as usize]).cloned()),
                Op::PushTruth(t) => truths.push(*t),
                Op::Arith(op) => {
                    let b = vals.pop().flatten();
                    let a = vals.pop().flatten();
                    vals.push(match (a, b) {
                        (Some(a), Some(b)) => checked_arith(*op, &a, &b),
                        _ => None,
                    });
                }
                Op::Compare(_) => {
                    vals.pop();
                    vals.pop();
                    truths.push(Truth::Unknown);
                }
                Op::ToTruth => {
                    vals.pop();
                    truths.push(Truth::Unknown);
                }
                Op::Not => {}
                Op::IsNull | Op::IsNotNull => {
                    vals.pop();
                    truths.push(Truth::Unknown);
                }
                Op::In(n) => {
                    for _ in 0..*n {
                        vals.pop();
                    }
                    vals.pop();
                    truths.push(Truth::Unknown);
                }
                Op::Like(_) => {
                    vals.pop();
                    truths.push(Truth::Unknown);
                }
                Op::FoldAnd | Op::FoldOr => {
                    truths.pop();
                }
                // Jumps are not taken here: this walks every instruction to
                // check the *worst-case* balance, which is the stricter
                // property — a short-circuited path pops strictly less.
                Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => {}
            }
            pc += 1;
        }
        (vals.len(), truths.len())
    }
}
