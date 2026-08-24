//! Predicate expressions.
//!
//! Null handling follows SQL: a comparison involving a null or absent operand is
//! *unknown*, not false, and only a definitely-true predicate keeps a row. The
//! distinction matters as soon as `NOT` appears — under two-valued logic
//! `NOT (x = 1)` would wrongly admit rows where `x` is absent.

use adabt_core::record::Record;
use adabt_core::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }

    pub(crate) fn apply(self, ord: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            CmpOp::Eq => ord == Equal,
            CmpOp::Ne => ord != Equal,
            CmpOp::Lt => ord == Less,
            CmpOp::Le => ord != Greater,
            CmpOp::Gt => ord == Greater,
            CmpOp::Ge => ord != Less,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Literal(Value),
    Field(String),
    Compare {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    /// A value-producing sub-expression, not a predicate. `Compare` accepts one
    /// on either side for free — arithmetic slots into the existing polymorphic
    /// `lhs`/`rhs` rather than needing its own comparison logic.
    Arith {
        op: adabt_core::value::ArithOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// True if `needle` equals any of `list`. Each element is its own
    /// sub-expression rather than a bare `Vec<Value>` so a caller building one
    /// programmatically has one literal-wrapping convention (`Expr::lit`) for
    /// every kind of value this IR carries, not a second one just for this.
    In {
        needle: Box<Expr>,
        list: Vec<Expr>,
    },
    /// SQL-style pattern match: `%` any run of characters, `_` exactly one,
    /// `\%`/`\_` the literal character. Only meaningful on `Value::Str` — a
    /// non-string operand evaluates unknown, matching how every other
    /// type-mismatched comparison here behaves.
    Like {
        text: Box<Expr>,
        pattern: String,
    },
}

/// `Expr::field("a") + Expr::field("b")`, and so on for the other three —
/// the real `std::ops` traits rather than same-named inherent methods, both
/// because clippy is right that `fn add`/`sub`/`mul`/`div` shadowing the
/// trait names invites confusing them, and because implementing the trait
/// properly is strictly more useful: it is what makes the operators usable at
/// all.
impl std::ops::Add for Expr {
    type Output = Expr;
    fn add(self, rhs: Expr) -> Expr {
        self.arith(adabt_core::value::ArithOp::Add, rhs)
    }
}
impl std::ops::Sub for Expr {
    type Output = Expr;
    fn sub(self, rhs: Expr) -> Expr {
        self.arith(adabt_core::value::ArithOp::Sub, rhs)
    }
}
impl std::ops::Mul for Expr {
    type Output = Expr;
    fn mul(self, rhs: Expr) -> Expr {
        self.arith(adabt_core::value::ArithOp::Mul, rhs)
    }
}
impl std::ops::Div for Expr {
    type Output = Expr;
    fn div(self, rhs: Expr) -> Expr {
        self.arith(adabt_core::value::ArithOp::Div, rhs)
    }
}

/// SQL three-valued logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    pub fn is_true(self) -> bool {
        self == Truth::True
    }
    pub(crate) fn not(self) -> Truth {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }
}

impl Expr {
    pub fn field(name: impl Into<String>) -> Expr {
        Expr::Field(name.into())
    }
    pub fn lit(v: impl Into<Value>) -> Expr {
        Expr::Literal(v.into())
    }
    pub fn eq(name: impl Into<String>, v: impl Into<Value>) -> Expr {
        Expr::Compare {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::field(name)),
            rhs: Box::new(Expr::lit(v)),
        }
    }
    pub fn cmp(name: impl Into<String>, op: CmpOp, v: impl Into<Value>) -> Expr {
        Expr::Compare {
            op,
            lhs: Box::new(Expr::field(name)),
            rhs: Box::new(Expr::lit(v)),
        }
    }

    fn arith(self, op: adabt_core::value::ArithOp, rhs: Expr) -> Expr {
        Expr::Arith {
            op,
            lhs: Box::new(self),
            rhs: Box::new(rhs),
        }
    }

    /// `self IN (list)`. Each element is wrapped as a literal, which covers
    /// the ordinary case; build `Expr::In` directly for a list of general
    /// sub-expressions.
    pub fn in_values(self, list: impl IntoIterator<Item = impl Into<Value>>) -> Expr {
        Expr::In {
            needle: Box::new(self),
            list: list.into_iter().map(Expr::lit).collect(),
        }
    }

    /// `self LIKE pattern`, SQL wildcards (`%`, `_`, escaped with `\`).
    pub fn like(self, pattern: impl Into<String>) -> Expr {
        Expr::Like {
            text: Box::new(self),
            pattern: pattern.into(),
        }
    }

    /// A comparison between two arbitrary sub-expressions — what `Expr::eq`
    /// and `Expr::cmp` build for the common field-against-literal case,
    /// available directly for anything else, an arithmetic result compared
    /// against another expression included.
    pub fn compare(self, op: CmpOp, rhs: Expr) -> Expr {
        Expr::Compare {
            op,
            lhs: Box::new(self),
            rhs: Box::new(rhs),
        }
    }

    /// Resolve to a value, or `None` when the field is absent or the
    /// sub-expression has no value of its own (a nested boolean, for
    /// instance).
    ///
    /// Owned rather than borrowed, unlike the field-only version this used to
    /// be: `Arith` computes a fresh `Value` that does not live inside `rec` or
    /// `self`, so nothing shorter-lived could be returned for it. The clone
    /// this costs `Literal`/`Field` in exchange is not on any path this
    /// project has ever measured as hot — the evaluator runs once per row per
    /// predicate, not per byte.
    fn value(&self, rec: &Record) -> Option<Value> {
        match self {
            Expr::Literal(v) => Some(v.clone()),
            Expr::Field(name) => rec.get(name).cloned(),
            Expr::Arith { op, lhs, rhs } => {
                let (a, b) = (lhs.value(rec)?, rhs.value(rec)?);
                adabt_core::value::checked_arith(*op, &a, &b)
            }
            _ => None,
        }
    }

    pub fn evaluate(&self, rec: &Record) -> Truth {
        match self {
            Expr::Literal(Value::Bool(b)) => {
                if *b {
                    Truth::True
                } else {
                    Truth::False
                }
            }
            Expr::Literal(Value::Null) => Truth::Unknown,
            Expr::Literal(_) | Expr::Field(_) => match self.value(rec) {
                Some(Value::Bool(true)) => Truth::True,
                Some(Value::Bool(false)) => Truth::False,
                _ => Truth::Unknown,
            },
            Expr::Compare { op, lhs, rhs } => {
                match (lhs.value(rec), rhs.value(rec)) {
                    (Some(a), Some(b)) if !a.is_null() && !b.is_null() => {
                        if op.apply(a.cmp(&b)) {
                            Truth::True
                        } else {
                            Truth::False
                        }
                    }
                    // Either side missing or null: the comparison is unknown.
                    _ => Truth::Unknown,
                }
            }
            Expr::And(parts) => {
                let mut saw_unknown = false;
                for p in parts {
                    match p.evaluate(rec) {
                        Truth::False => return Truth::False,
                        Truth::Unknown => saw_unknown = true,
                        Truth::True => {}
                    }
                }
                if saw_unknown {
                    Truth::Unknown
                } else {
                    Truth::True
                }
            }
            Expr::Or(parts) => {
                let mut saw_unknown = false;
                for p in parts {
                    match p.evaluate(rec) {
                        Truth::True => return Truth::True,
                        Truth::Unknown => saw_unknown = true,
                        Truth::False => {}
                    }
                }
                if saw_unknown {
                    Truth::Unknown
                } else {
                    Truth::False
                }
            }
            Expr::Not(inner) => inner.evaluate(rec).not(),
            Expr::IsNull(inner) => match inner.value(rec) {
                None | Some(Value::Null) => Truth::True,
                _ => Truth::False,
            },
            Expr::IsNotNull(inner) => match inner.value(rec) {
                None | Some(Value::Null) => Truth::False,
                _ => Truth::True,
            },
            Expr::Arith { .. } => match self.value(rec) {
                Some(Value::Bool(true)) => Truth::True,
                Some(Value::Bool(false)) => Truth::False,
                _ => Truth::Unknown,
            },
            Expr::In { needle, list } => {
                let Some(n) = needle.value(rec).filter(|v| !v.is_null()) else {
                    return Truth::Unknown;
                };
                let mut saw_unknown = false;
                for item in list {
                    match item.value(rec) {
                        Some(v) if !v.is_null() => {
                            if v == n {
                                return Truth::True;
                            }
                        }
                        // A null or unresolvable element could have matched;
                        // its absence does not prove the needle is not in the
                        // list, only that this element does not settle it.
                        _ => saw_unknown = true,
                    }
                }
                if saw_unknown {
                    Truth::Unknown
                } else {
                    Truth::False
                }
            }
            Expr::Like { text, pattern } => match text.value(rec) {
                Some(Value::Str(s)) => {
                    if like_matches(&s, pattern) {
                        Truth::True
                    } else {
                        Truth::False
                    }
                }
                _ => Truth::Unknown,
            },
        }
    }

    /// Whether the record passes. Only definite truth keeps a row.
    pub fn matches(&self, rec: &Record) -> bool {
        self.evaluate(rec).is_true()
    }

    /// Equality constraints of the form `field = literal`, which is what an
    /// index lookup can serve. Extracted from a top-level `And` chain too.
    pub fn equality_constraints(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        self.collect_equalities(&mut out);
        out
    }

    fn collect_equalities(&self, out: &mut Vec<(String, Value)>) {
        match self {
            Expr::Compare {
                op: CmpOp::Eq,
                lhs,
                rhs,
            } => match (lhs.as_ref(), rhs.as_ref()) {
                (Expr::Field(f), Expr::Literal(v)) | (Expr::Literal(v), Expr::Field(f)) => {
                    out.push((f.clone(), v.clone()))
                }
                _ => {}
            },
            // Only `And` preserves the constraint: under `Or` a matching row
            // need not satisfy this branch at all.
            Expr::And(parts) => {
                for p in parts {
                    p.collect_equalities(out);
                }
            }
            _ => {}
        }
    }

    /// Field names the expression reads, for projection pushdown.
    pub fn referenced_fields(&self, out: &mut Vec<String>) {
        match self {
            Expr::Field(f) => {
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
            Expr::Literal(_) => {}
            Expr::Compare { lhs, rhs, .. } => {
                lhs.referenced_fields(out);
                rhs.referenced_fields(out);
            }
            Expr::And(parts) | Expr::Or(parts) => {
                for p in parts {
                    p.referenced_fields(out);
                }
            }
            Expr::Not(i) | Expr::IsNull(i) | Expr::IsNotNull(i) => i.referenced_fields(out),
            Expr::Arith { lhs, rhs, .. } => {
                lhs.referenced_fields(out);
                rhs.referenced_fields(out);
            }
            Expr::In { needle, list } => {
                needle.referenced_fields(out);
                for item in list {
                    item.referenced_fields(out);
                }
            }
            Expr::Like { text, .. } => text.referenced_fields(out),
        }
    }
}

/// SQL-style pattern match. `%` any run of characters (including none), `_`
/// exactly one, `\%`/`\_` the literal character. Case-sensitive.
///
/// A small hand-rolled matcher rather than a regex crate: the whole point of
/// this workspace's one-dependency discipline is that a feature this size does
/// not need to justify pulling one in, and a linear scan over two short strings
/// costs nothing worth optimising away.
fn like_matches(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p = parse_like_pattern(pattern);
    matches_from(&t, &p)
}

/// One unit of a compiled pattern. Kept as its own type rather than plain
/// `char` — an escaped `%` and a wildcard `%` are both just the character `%`
/// once escaping has been resolved, so the *meaning* has to survive parsing as
/// something a `char` cannot carry on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Piece {
    Literal(char),
    AnyRun,
    AnyOne,
}

pub(crate) fn parse_like_pattern(pattern: &str) -> Vec<Piece> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' => out.push(Piece::AnyRun),
            '_' => out.push(Piece::AnyOne),
            '\\' => match chars.next() {
                Some(escaped) => out.push(Piece::Literal(escaped)),
                // A trailing backslash with nothing to escape is itself.
                None => out.push(Piece::Literal('\\')),
            },
            other => out.push(Piece::Literal(other)),
        }
    }
    out
}

pub(crate) fn matches_from(text: &[char], pattern: &[Piece]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(Piece::AnyRun) => {
            matches_from(text, &pattern[1..])
                || (!text.is_empty() && matches_from(&text[1..], pattern))
        }
        Some(Piece::AnyOne) => !text.is_empty() && matches_from(&text[1..], &pattern[1..]),
        Some(Piece::Literal(c)) => {
            !text.is_empty() && text[0] == *c && matches_from(&text[1..], &pattern[1..])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> Record {
        Record::new().with("a", 5i64).with("s", "hello")
    }

    #[test]
    fn comparisons_work_on_present_fields() {
        assert!(Expr::eq("a", 5i64).matches(&rec()));
        assert!(!Expr::eq("a", 6i64).matches(&rec()));
        assert!(Expr::cmp("a", CmpOp::Lt, 10i64).matches(&rec()));
        assert!(Expr::cmp("a", CmpOp::Ge, 5i64).matches(&rec()));
        assert!(Expr::eq("s", "hello").matches(&rec()));
    }

    #[test]
    fn a_missing_field_makes_a_comparison_unknown_not_false() {
        assert_eq!(Expr::eq("missing", 1i64).evaluate(&rec()), Truth::Unknown);
        assert!(!Expr::eq("missing", 1i64).matches(&rec()));
    }

    #[test]
    fn negating_an_unknown_stays_unknown() {
        // The reason three-valued logic is worth the trouble: under two-valued
        // logic this would be true and the row would wrongly survive.
        let e = Expr::Not(Box::new(Expr::eq("missing", 1i64)));
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
        assert!(!e.matches(&rec()));
    }

    #[test]
    fn and_is_false_if_any_part_is_false_even_with_unknowns() {
        let e = Expr::And(vec![Expr::eq("a", 999i64), Expr::eq("missing", 1i64)]);
        assert_eq!(e.evaluate(&rec()), Truth::False);
    }

    #[test]
    fn or_is_true_if_any_part_is_true_even_with_unknowns() {
        let e = Expr::Or(vec![Expr::eq("a", 5i64), Expr::eq("missing", 1i64)]);
        assert_eq!(e.evaluate(&rec()), Truth::True);
    }

    #[test]
    fn and_or_propagate_unknown_when_undecided() {
        assert_eq!(
            Expr::And(vec![Expr::eq("a", 5i64), Expr::eq("missing", 1i64)]).evaluate(&rec()),
            Truth::Unknown
        );
        assert_eq!(
            Expr::Or(vec![Expr::eq("a", 999i64), Expr::eq("missing", 1i64)]).evaluate(&rec()),
            Truth::Unknown
        );
    }

    #[test]
    fn is_null_distinguishes_absence_from_presence() {
        assert!(Expr::IsNull(Box::new(Expr::field("missing"))).matches(&rec()));
        assert!(!Expr::IsNull(Box::new(Expr::field("a"))).matches(&rec()));
        assert!(Expr::IsNotNull(Box::new(Expr::field("a"))).matches(&rec()));
    }

    #[test]
    fn equality_constraints_are_extracted_from_and_chains() {
        let e = Expr::And(vec![
            Expr::eq("country", "NO"),
            Expr::cmp("age", CmpOp::Gt, 18i64),
            Expr::eq("active", true),
        ]);
        let c = e.equality_constraints();
        assert_eq!(c.len(), 2);
        assert!(c.contains(&("country".to_string(), Value::Str("NO".into()))));
        assert!(c.contains(&("active".to_string(), Value::Bool(true))));
    }

    #[test]
    fn equality_constraints_are_not_extracted_from_or_branches() {
        // A row matching the other branch need not satisfy this one, so using
        // it to drive an index lookup would silently drop results.
        let e = Expr::Or(vec![Expr::eq("a", 1i64), Expr::eq("b", 2i64)]);
        assert!(e.equality_constraints().is_empty());
    }

    #[test]
    fn referenced_fields_are_collected_without_duplicates() {
        let e = Expr::And(vec![
            Expr::eq("a", 1i64),
            Expr::cmp("b", CmpOp::Lt, 2i64),
            Expr::eq("a", 3i64),
        ]);
        let mut f = Vec::new();
        e.referenced_fields(&mut f);
        assert_eq!(f, vec!["a".to_string(), "b".to_string()]);
    }
}

#[cfg(test)]
mod extended_tests {
    use super::*;

    fn rec() -> Record {
        Record::new()
            .with("balance", 100i64)
            .with(
                "fee",
                Value::Decimal {
                    units: 250,
                    scale: 2,
                },
            )
            .with("country", "NO")
            .with("email", Value::Null)
    }

    #[test]
    fn arithmetic_participates_in_comparison() {
        // balance(100) - fee(2.50) = 97.50
        let e = (Expr::field("balance") - Expr::field("fee")).arith_cmp_eq(Value::Decimal {
            units: 9750,
            scale: 2,
        });
        assert!(e.matches(&rec()));
    }

    #[test]
    fn arithmetic_on_a_missing_field_is_unknown_not_a_crash() {
        let e =
            (Expr::field("balance") + Expr::field("does_not_exist")).arith_cmp_eq(Value::I64(999));
        assert!(!e.matches(&rec()));
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
    }

    #[test]
    fn division_by_zero_is_unknown() {
        let e = (Expr::field("balance") / Expr::lit(0i64)).arith_cmp_eq(Value::I64(1));
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
    }

    #[test]
    fn in_matches_any_element() {
        assert!(Expr::field("country")
            .in_values(["SE", "NO", "DK"])
            .matches(&rec()));
        assert!(!Expr::field("country")
            .in_values(["SE", "FI", "DK"])
            .matches(&rec()));
    }

    #[test]
    fn in_on_a_null_needle_is_unknown() {
        let e = Expr::field("email").in_values(["a@b.com"]);
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
    }

    #[test]
    fn in_with_no_match_but_a_null_element_is_unknown_not_false() {
        // 5 IN (1, NULL) is UNKNOWN in SQL: NULL might have been the match.
        let e = Expr::In {
            needle: Box::new(Expr::lit(5i64)),
            list: vec![Expr::lit(1i64), Expr::lit(Value::Null)],
        };
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
    }

    #[test]
    fn like_wildcards_match_as_expected() {
        assert!(Expr::field("country").like("N%").matches(&rec()));
        assert!(Expr::field("country").like("_O").matches(&rec()));
        assert!(Expr::field("country").like("NO").matches(&rec()));
        assert!(!Expr::field("country").like("S%").matches(&rec()));
        assert!(!Expr::field("country").like("N").matches(&rec()));
    }

    #[test]
    fn like_percent_matches_the_empty_run_too() {
        assert!(Expr::lit("hello").like("%hello%").matches(&Record::new()));
        assert!(Expr::lit("").like("%").matches(&Record::new()));
    }

    #[test]
    fn like_escapes_literal_wildcards() {
        let mut r = Record::new();
        r.set("s".to_string(), Value::from("50%"));
        assert!(Expr::field("s").like("50\\%").matches(&r));
        assert!(!Expr::field("s").like("50X").matches(&r));
    }

    #[test]
    fn like_on_a_non_string_is_unknown() {
        let e = Expr::field("balance").like("1%");
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
    }

    #[test]
    fn like_on_a_missing_field_is_unknown() {
        let e = Expr::field("nope").like("%");
        assert_eq!(e.evaluate(&rec()), Truth::Unknown);
    }

    #[test]
    fn referenced_fields_covers_arith_in_and_like() {
        let mut out = Vec::new();
        (Expr::field("a") + Expr::field("b"))
            .arith_cmp_eq(Value::I64(1))
            .referenced_fields(&mut out);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);

        let mut out = Vec::new();
        Expr::field("x")
            .in_values([1i64, 2, 3])
            .referenced_fields(&mut out);
        assert_eq!(out, vec!["x".to_string()]);

        let mut out = Vec::new();
        Expr::field("y").like("z%").referenced_fields(&mut out);
        assert_eq!(out, vec!["y".to_string()]);
    }

    #[test]
    fn exact_decimal_arithmetic_never_drifts_over_many_operations() {
        // The property that matters most: chained arithmetic on money stays
        // exact, unlike the same chain in f64.
        let mut e = Expr::lit(Value::Decimal { units: 0, scale: 2 });
        for _ in 0..1000 {
            e = e + Expr::lit(Value::Decimal { units: 1, scale: 2 }); // +0.01
        }
        let result = e.arith_cmp_eq(Value::Decimal {
            units: 1000,
            scale: 2,
        }); // 10.00
        assert!(
            result.matches(&Record::new()),
            "1000 additions of 0.01 did not land on exactly 10.00"
        );
    }

    /// `self = value`, via the general `compare` builder — arithmetic results
    /// need it since `Expr::eq`/`Expr::cmp` build only the field-against-literal
    /// shape.
    trait ArithEq {
        fn arith_cmp_eq(self, v: impl Into<Value>) -> Expr;
    }
    impl ArithEq for Expr {
        fn arith_cmp_eq(self, v: impl Into<Value>) -> Expr {
            self.compare(CmpOp::Eq, Expr::lit(v))
        }
    }
}
