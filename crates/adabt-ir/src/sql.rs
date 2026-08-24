//! A SQL front-end: a parser onto the logical IR.
//!
//! # Why this is last rather than first
//!
//! The plan is explicit about it, and the reason is worth restating: built
//! earlier, this would have been a parser onto a moving target. The IR has
//! since grown a tree shape (M20), joins (M23), arithmetic, `IN` and `LIKE`
//! (M20), and a compiled evaluator (M26). A SQL layer written before any of
//! that would have been rewritten three times, and each rewrite is a chance
//! to introduce a difference between what SQL means and what the IR does.
//!
//! # What this is not
//!
//! **Not a SQL implementation.** It is a front-end onto exactly the
//! capabilities the engine already has, and it refuses everything else
//! rather than approximating it. `SELECT` only: no `INSERT`, `UPDATE`,
//! `DELETE`, `CREATE`, subqueries, `UNION`, `HAVING`, window functions, or
//! `OR` inside a join condition. Every one of those is rejected with a
//! message naming what was unsupported, because a SQL front-end that
//! silently does something *near* what was asked is worse than one that
//! declines — the caller cannot tell the difference until the answer is
//! wrong.
//!
//! One join per query, matching `Database::query_join`'s own limit; a second
//! is refused here rather than accepted and refused later.
//!
//! # Ownership of semantics
//!
//! Where SQL and this IR already agree — three-valued logic, `NULL` never
//! equalling `NULL`, `LIKE` wildcards — the IR's existing behaviour is the
//! definition and this layer only maps syntax onto it. Nothing here
//! reimplements an evaluation rule.

use crate::expr::{CmpOp, Expr};
use crate::plan::{Agg, AggKind, JoinKind, LogicalOp, LogicalPlan, SortKey};
use adabt_core::value::Value;

/// Why a statement could not be turned into a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    pub message: String,
    /// Byte offset into the input where the problem was noticed, when known.
    pub at: Option<usize>,
}

impl SqlError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            at: None,
        }
    }
    fn at(message: impl Into<String>, at: usize) -> Self {
        Self {
            message: message.into(),
            at: Some(at),
        }
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.at {
            Some(at) => write!(f, "{} (at byte {at})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for SqlError {}

type R<T> = Result<T, SqlError>;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Num(f64),
    Int(i64),
    Sym(char),
    /// `<=`, `>=`, `<>`, `!=`
    Op2(String),
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else if c == b'-' && self.bytes.get(self.pos + 1) == Some(&b'-') {
                // A line comment runs to the newline.
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn tokens(mut self) -> R<Vec<(Tok, usize)>> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            if self.pos >= self.bytes.len() {
                return Ok(out);
            }
            let start = self.pos;
            let c = self.bytes[self.pos];
            let tok = if c.is_ascii_alphabetic() || c == b'_' {
                while self.pos < self.bytes.len()
                    && (self.bytes[self.pos].is_ascii_alphanumeric()
                        || self.bytes[self.pos] == b'_')
                {
                    self.pos += 1;
                }
                Tok::Word(self.src[start..self.pos].to_string())
            } else if c.is_ascii_digit() {
                let mut seen_dot = false;
                while self.pos < self.bytes.len() {
                    let d = self.bytes[self.pos];
                    if d.is_ascii_digit() {
                        self.pos += 1;
                    } else if d == b'.' && !seen_dot {
                        seen_dot = true;
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let text = &self.src[start..self.pos];
                if seen_dot {
                    Tok::Num(
                        text.parse().map_err(|_| {
                            SqlError::at(format!("`{text}` is not a number"), start)
                        })?,
                    )
                } else {
                    Tok::Int(text.parse().map_err(|_| {
                        SqlError::at(format!("`{text}` does not fit an integer"), start)
                    })?)
                }
            } else if c == b'\'' {
                self.pos += 1;
                let mut s = String::new();
                loop {
                    let Some(&d) = self.bytes.get(self.pos) else {
                        return Err(SqlError::at("unterminated string literal", start));
                    };
                    self.pos += 1;
                    if d == b'\'' {
                        // '' inside a string is an escaped quote, per SQL.
                        if self.bytes.get(self.pos) == Some(&b'\'') {
                            s.push('\'');
                            self.pos += 1;
                            continue;
                        }
                        break;
                    }
                    s.push(d as char);
                }
                Tok::Str(s)
            } else if matches!(c, b'<' | b'>' | b'!') && self.bytes.get(self.pos + 1) == Some(&b'=')
            {
                self.pos += 2;
                Tok::Op2(self.src[start..self.pos].to_string())
            } else if c == b'<' && self.bytes.get(self.pos + 1) == Some(&b'>') {
                self.pos += 2;
                Tok::Op2("<>".to_string())
            } else if matches!(c, b'(' | b')' | b',' | b'*' | b'=' | b'<' | b'>' | b'.') {
                self.pos += 1;
                Tok::Sym(c as char)
            } else {
                return Err(SqlError::at(
                    format!("unexpected character `{}`", c as char),
                    start,
                ));
            };
            out.push((tok, start));
        }
    }
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }
    fn offset(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|(_, o)| *o)
            .unwrap_or_else(|| self.toks.last().map(|(_, o)| *o).unwrap_or(0))
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    /// Consume `word` if it is next, case-insensitively.
    fn eat_word(&mut self, word: &str) -> bool {
        if let Some(Tok::Word(w)) = self.peek() {
            if w.eq_ignore_ascii_case(word) {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn peek_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
    }
    fn eat_sym(&mut self, c: char) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(s)) if *s == c) {
            self.pos += 1;
            return true;
        }
        false
    }
    fn expect_word(&mut self, word: &str) -> R<()> {
        if self.eat_word(word) {
            Ok(())
        } else {
            Err(SqlError::at(
                format!("expected `{}`", word.to_uppercase()),
                self.offset(),
            ))
        }
    }
    fn expect_sym(&mut self, c: char) -> R<()> {
        if self.eat_sym(c) {
            Ok(())
        } else {
            Err(SqlError::at(format!("expected `{c}`"), self.offset()))
        }
    }
    fn identifier(&mut self) -> R<String> {
        let at = self.offset();
        match self.next() {
            Some(Tok::Word(w)) => Ok(w),
            _ => Err(SqlError::at("expected a name", at)),
        }
    }

    /// A possibly-qualified column: `age` or `users.age`.
    ///
    /// A qualified name is kept joined, because that is exactly the form a
    /// join's output rows use (`merge_joined_fields` prefixes every field
    /// with its collection). So `users.id` in SQL and `users.id` in a joined
    /// row are the same string, with no translation layer to disagree.
    fn column(&mut self) -> R<String> {
        let first = self.identifier()?;
        if self.eat_sym('.') {
            let second = self.identifier()?;
            return Ok(format!("{first}.{second}"));
        }
        Ok(first)
    }
}

fn reserved(w: &str) -> bool {
    const WORDS: [&str; 16] = [
        "from", "where", "group", "order", "by", "limit", "join", "on", "left", "inner", "and",
        "or", "not", "is", "null", "as",
    ];
    WORDS.iter().any(|k| w.eq_ignore_ascii_case(k))
}

/// Parse a `SELECT` statement into a logical plan.
///
/// See the module docs for exactly what is and is not supported. Anything
/// outside that set is an error naming the construct, never a silent
/// approximation.
pub fn parse_select(sql: &str) -> R<LogicalPlan> {
    let toks = Lexer::new(sql).tokens()?;
    if toks.is_empty() {
        return Err(SqlError::new("empty statement"));
    }
    let mut p = Parser { toks, pos: 0 };

    // Reject the statements this front-end deliberately does not implement,
    // by name, before anything else confuses the error message.
    for kw in [
        "insert", "update", "delete", "create", "drop", "alter", "with",
    ] {
        if p.peek_word(kw) {
            return Err(SqlError::at(
                format!(
                    "{} is not supported; this front-end reads with SELECT only",
                    kw.to_uppercase()
                ),
                p.offset(),
            ));
        }
    }
    p.expect_word("select")?;

    // -- projection / aggregates
    let mut projection: Vec<String> = Vec::new();
    let mut aggs: Vec<Agg> = Vec::new();
    let mut star = false;
    loop {
        if p.eat_sym('*') {
            star = true;
        } else if let Some(agg) = parse_aggregate(&mut p)? {
            aggs.push(agg);
        } else {
            projection.push(p.column()?);
        }
        if !p.eat_sym(',') {
            break;
        }
    }

    p.expect_word("from")?;
    let base = p.identifier()?;
    let mut root = LogicalOp::scan(&base);

    // -- join (at most one, matching the engine's own limit)
    let mut joined = false;
    loop {
        let kind = if p.eat_word("left") {
            let _ = p.eat_word("outer");
            p.expect_word("join")?;
            Some(JoinKind::Left)
        } else if p.eat_word("inner") || p.peek_word("join") {
            // `INNER JOIN` and a bare `JOIN` mean the same thing; the
            // optional INNER has already been consumed if it was there.
            p.expect_word("join")?;
            Some(JoinKind::Inner)
        } else {
            None
        };
        let Some(kind) = kind else { break };
        if joined {
            return Err(SqlError::at(
                "only one JOIN per query is supported",
                p.offset(),
            ));
        }
        joined = true;
        let right = p.identifier()?;
        p.expect_word("on")?;
        let l = p.column()?;
        p.expect_sym('=')?;
        let r = p.column()?;
        // The engine's join takes bare field names per side; a qualified
        // name in SQL names the same field.
        let strip = |s: &str| s.rsplit('.').next().unwrap_or(s).to_string();
        root = root.join(LogicalOp::scan(&right), kind, (strip(&l), strip(&r)));
    }

    // -- where
    if p.eat_word("where") {
        let pred = parse_or(&mut p)?;
        root = root.filter(pred);
    }

    // -- group by
    let mut group_by: Vec<String> = Vec::new();
    if p.eat_word("group") {
        p.expect_word("by")?;
        loop {
            group_by.push(p.column()?);
            if !p.eat_sym(',') {
                break;
            }
        }
    }

    if !aggs.is_empty() || !group_by.is_empty() {
        if aggs.is_empty() {
            return Err(SqlError::new(
                "GROUP BY without an aggregate has no meaning here; add COUNT/SUM/MIN/MAX/AVG",
            ));
        }
        root = root.aggregate(group_by.clone(), aggs);
    } else if !star && !projection.is_empty() {
        root = root.project(projection.clone());
    }

    // -- order by
    if p.eat_word("order") {
        p.expect_word("by")?;
        let mut keys = Vec::new();
        loop {
            let field = p.column()?;
            let descending = if p.eat_word("desc") {
                true
            } else {
                let _ = p.eat_word("asc");
                false
            };
            keys.push(SortKey { field, descending });
            if !p.eat_sym(',') {
                break;
            }
        }
        root = root.sort(keys);
    }

    // -- limit
    if p.eat_word("limit") {
        let at = p.offset();
        match p.next() {
            Some(Tok::Int(n)) if n >= 0 => root = root.limit(n as usize),
            _ => return Err(SqlError::at("LIMIT needs a non-negative integer", at)),
        }
    }

    if p.pos < p.toks.len() {
        return Err(SqlError::at("unexpected trailing input", p.offset()));
    }
    Ok(LogicalPlan::new(root))
}

fn parse_aggregate(p: &mut Parser) -> R<Option<Agg>> {
    let Some(Tok::Word(w)) = p.peek().cloned() else {
        return Ok(None);
    };
    let kind = match w.to_ascii_lowercase().as_str() {
        "count" => AggKind::Count,
        "sum" => AggKind::Sum,
        "min" => AggKind::Min,
        "max" => AggKind::Max,
        "avg" => AggKind::Avg,
        _ => return Ok(None),
    };
    // Only an aggregate if a `(` actually follows; `count` could be a column.
    if !matches!(p.toks.get(p.pos + 1).map(|(t, _)| t), Some(Tok::Sym('('))) {
        return Ok(None);
    }
    p.pos += 1;
    p.expect_sym('(')?;
    let field = if p.eat_sym('*') {
        None
    } else {
        Some(p.column()?)
    };
    p.expect_sym(')')?;
    if kind != AggKind::Count && field.is_none() {
        return Err(SqlError::at(
            format!("{}(*) is not meaningful; name a column", w.to_uppercase()),
            p.offset(),
        ));
    }
    // `AS name`, else a default derived from the aggregate itself.
    let output = if p.eat_word("as") {
        p.identifier()?
    } else if let Some(Tok::Word(next)) = p.peek() {
        if reserved(next) {
            default_output(kind, field.as_deref())
        } else {
            p.identifier()?
        }
    } else {
        default_output(kind, field.as_deref())
    };
    Ok(Some(match field {
        None => Agg::count(output),
        Some(f) => Agg::over(kind, f, output),
    }))
}

fn default_output(kind: AggKind, field: Option<&str>) -> String {
    match field {
        None => format!("{}_star", kind.as_str()),
        Some(f) => format!("{}_{}", kind.as_str(), f.replace('.', "_")),
    }
}

fn parse_or(p: &mut Parser) -> R<Expr> {
    let mut parts = vec![parse_and(p)?];
    while p.eat_word("or") {
        parts.push(parse_and(p)?);
    }
    Ok(if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        Expr::Or(parts)
    })
}

fn parse_and(p: &mut Parser) -> R<Expr> {
    let mut parts = vec![parse_not(p)?];
    while p.eat_word("and") {
        parts.push(parse_not(p)?);
    }
    Ok(if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        Expr::And(parts)
    })
}

fn parse_not(p: &mut Parser) -> R<Expr> {
    if p.eat_word("not") {
        return Ok(Expr::Not(Box::new(parse_not(p)?)));
    }
    parse_predicate(p)
}

fn parse_predicate(p: &mut Parser) -> R<Expr> {
    if p.eat_sym('(') {
        let inner = parse_or(p)?;
        p.expect_sym(')')?;
        return Ok(inner);
    }
    let at = p.offset();
    let lhs = parse_value(p)?;

    // IS [NOT] NULL
    if p.eat_word("is") {
        let negated = p.eat_word("not");
        p.expect_word("null")?;
        return Ok(if negated {
            Expr::IsNotNull(Box::new(lhs))
        } else {
            Expr::IsNull(Box::new(lhs))
        });
    }

    // [NOT] IN (...) / [NOT] LIKE '...'
    let negated = p.eat_word("not");
    if p.eat_word("in") {
        p.expect_sym('(')?;
        let mut list = Vec::new();
        loop {
            list.push(parse_value(p)?);
            if !p.eat_sym(',') {
                break;
            }
        }
        p.expect_sym(')')?;
        let e = Expr::In {
            needle: Box::new(lhs),
            list,
        };
        return Ok(if negated { Expr::Not(Box::new(e)) } else { e });
    }
    if p.eat_word("like") {
        let at = p.offset();
        let pattern = match p.next() {
            Some(Tok::Str(s)) => s,
            _ => return Err(SqlError::at("LIKE needs a string pattern", at)),
        };
        let e = Expr::Like {
            text: Box::new(lhs),
            pattern,
        };
        return Ok(if negated { Expr::Not(Box::new(e)) } else { e });
    }
    if negated {
        return Err(SqlError::at("NOT must be followed by IN or LIKE here", at));
    }

    // Comparison
    let op = match p.next() {
        Some(Tok::Sym('=')) => CmpOp::Eq,
        Some(Tok::Sym('<')) => CmpOp::Lt,
        Some(Tok::Sym('>')) => CmpOp::Gt,
        Some(Tok::Op2(s)) => match s.as_str() {
            "<=" => CmpOp::Le,
            ">=" => CmpOp::Ge,
            "<>" | "!=" => CmpOp::Ne,
            _ => return Err(SqlError::at(format!("unknown operator `{s}`"), at)),
        },
        _ => {
            return Err(SqlError::at(
                "expected a comparison, IS NULL, IN or LIKE",
                at,
            ))
        }
    };
    let rhs = parse_value(p)?;
    Ok(Expr::Compare {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    })
}

/// A value-producing term: a literal or a column. Arithmetic is deliberately
/// not parsed — the IR supports it, but `a + 1 > 2` needs precedence rules
/// this front-end does not yet have, and half-implemented precedence is how
/// a query silently means something other than it says.
fn parse_value(p: &mut Parser) -> R<Expr> {
    let at = p.offset();
    match p.peek().cloned() {
        Some(Tok::Str(s)) => {
            p.pos += 1;
            Ok(Expr::Literal(Value::Str(s)))
        }
        Some(Tok::Int(n)) => {
            p.pos += 1;
            Ok(Expr::Literal(Value::I64(n)))
        }
        Some(Tok::Num(f)) => {
            p.pos += 1;
            Ok(Expr::Literal(Value::F64(f)))
        }
        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("null") => {
            p.pos += 1;
            Ok(Expr::Literal(Value::Null))
        }
        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("true") => {
            p.pos += 1;
            Ok(Expr::Literal(Value::Bool(true)))
        }
        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("false") => {
            p.pos += 1;
            Ok(Expr::Literal(Value::Bool(false)))
        }
        Some(Tok::Word(_)) => Ok(Expr::Field(p.column()?)),
        _ => Err(SqlError::at("expected a value or column", at)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(sql: &str) -> LogicalPlan {
        parse_select(sql).unwrap_or_else(|e| panic!("{sql}\n  -> {e}"))
    }
    fn err(sql: &str) -> SqlError {
        parse_select(sql).unwrap_err()
    }

    #[test]
    fn a_bare_select_becomes_a_scan() {
        assert_eq!(plan("SELECT * FROM users").root, LogicalOp::scan("users"));
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(plan("select * from users"), plan("SELECT * FROM users"));
        assert_eq!(plan("SeLeCt * FrOm users"), plan("SELECT * FROM users"));
    }

    #[test]
    fn a_where_clause_becomes_the_same_filter_the_builder_makes() {
        assert_eq!(
            plan("SELECT * FROM users WHERE country = 'NO'").root,
            LogicalOp::scan("users").filter(Expr::eq("country", "NO"))
        );
    }

    #[test]
    fn and_or_not_nest_correctly() {
        // Precedence: OR is loosest, then AND, then NOT.
        let got = plan("SELECT * FROM u WHERE a = 1 OR b = 2 AND NOT c = 3");
        let expected = LogicalOp::scan("u").filter(Expr::Or(vec![
            Expr::eq("a", 1i64),
            Expr::And(vec![
                Expr::eq("b", 2i64),
                Expr::Not(Box::new(Expr::eq("c", 3i64))),
            ]),
        ]));
        assert_eq!(got.root, expected);
    }

    #[test]
    fn parentheses_override_precedence() {
        let got = plan("SELECT * FROM u WHERE (a = 1 OR b = 2) AND c = 3");
        let expected = LogicalOp::scan("u").filter(Expr::And(vec![
            Expr::Or(vec![Expr::eq("a", 1i64), Expr::eq("b", 2i64)]),
            Expr::eq("c", 3i64),
        ]));
        assert_eq!(got.root, expected);
    }

    #[test]
    fn every_comparison_operator_maps_to_its_cmpop() {
        for (sql, op) in [
            ("=", CmpOp::Eq),
            ("<", CmpOp::Lt),
            (">", CmpOp::Gt),
            ("<=", CmpOp::Le),
            (">=", CmpOp::Ge),
            ("<>", CmpOp::Ne),
            ("!=", CmpOp::Ne),
        ] {
            let got = plan(&format!("SELECT * FROM u WHERE a {sql} 1"));
            assert_eq!(
                got.root,
                LogicalOp::scan("u").filter(Expr::cmp("a", op, 1i64)),
                "operator {sql}"
            );
        }
    }

    #[test]
    fn is_null_and_is_not_null_parse() {
        assert_eq!(
            plan("SELECT * FROM u WHERE a IS NULL").root,
            LogicalOp::scan("u").filter(Expr::IsNull(Box::new(Expr::field("a"))))
        );
        assert_eq!(
            plan("SELECT * FROM u WHERE a IS NOT NULL").root,
            LogicalOp::scan("u").filter(Expr::IsNotNull(Box::new(Expr::field("a"))))
        );
    }

    #[test]
    fn in_and_like_map_onto_the_irs_own_variants() {
        assert_eq!(
            plan("SELECT * FROM u WHERE c IN ('a', 'b')").root,
            LogicalOp::scan("u").filter(Expr::field("c").in_values(["a", "b"]))
        );
        assert_eq!(
            plan("SELECT * FROM u WHERE c LIKE 'a%'").root,
            LogicalOp::scan("u").filter(Expr::field("c").like("a%"))
        );
        // NOT IN / NOT LIKE wrap rather than inventing a second variant.
        assert!(matches!(
            plan("SELECT * FROM u WHERE c NOT IN ('a')").root,
            LogicalOp::Filter {
                predicate: Expr::Not(_),
                ..
            }
        ));
    }

    #[test]
    fn projection_order_by_and_limit_compose() {
        let got = plan("SELECT name, age FROM u WHERE age > 18 ORDER BY age DESC LIMIT 5");
        let expected = LogicalOp::scan("u")
            .filter(Expr::cmp("age", CmpOp::Gt, 18i64))
            .project(vec!["name".into(), "age".into()])
            .sort(vec![SortKey {
                field: "age".into(),
                descending: true,
            }])
            .limit(5);
        assert_eq!(got.root, expected);
    }

    #[test]
    fn group_by_with_count_becomes_an_aggregate() {
        let got = plan("SELECT COUNT(*) AS n FROM u GROUP BY country");
        assert_eq!(
            got.root,
            LogicalOp::scan("u").aggregate(vec!["country".into()], vec![Agg::count("n")])
        );
    }

    #[test]
    fn an_aggregate_without_as_gets_a_derived_name() {
        let got = plan("SELECT AVG(age) FROM u GROUP BY country");
        match got.root {
            LogicalOp::Aggregate { aggs, .. } => {
                assert_eq!(aggs[0].output, "avg_age");
                assert_eq!(aggs[0].kind, AggKind::Avg);
            }
            other => panic!("expected an aggregate, got {}", other.name()),
        }
    }

    #[test]
    fn a_join_maps_onto_the_irs_join() {
        let got = plan("SELECT * FROM users JOIN orders ON users.id = orders.user_id");
        assert_eq!(
            got.root,
            LogicalOp::scan("users").join(
                LogicalOp::scan("orders"),
                JoinKind::Inner,
                ("id", "user_id")
            )
        );
        // LEFT JOIN picks the other kind.
        let left = plan("SELECT * FROM users LEFT JOIN orders ON users.id = orders.user_id");
        assert!(matches!(
            left.root,
            LogicalOp::Join {
                kind: JoinKind::Left,
                ..
            }
        ));
    }

    #[test]
    fn a_qualified_column_keeps_the_form_a_joined_row_uses() {
        // `merge_joined_fields` prefixes every field with its collection, so
        // `users.id` in SQL is the same string as in the row — no translation
        // layer that could disagree.
        let got =
            plan("SELECT * FROM users JOIN orders ON users.id = orders.user_id WHERE users.id = 1");
        match got.root {
            LogicalOp::Filter { predicate, .. } => match predicate {
                Expr::Compare { lhs, .. } => {
                    assert_eq!(*lhs, Expr::Field("users.id".into()));
                }
                other => panic!("unexpected predicate {other:?}"),
            },
            other => panic!("expected a filter, got {}", other.name()),
        }
    }

    #[test]
    fn string_literals_handle_escaped_quotes() {
        assert_eq!(
            plan("SELECT * FROM u WHERE s = 'it''s'").root,
            LogicalOp::scan("u").filter(Expr::eq("s", "it's"))
        );
    }

    #[test]
    fn comments_are_ignored() {
        assert_eq!(
            plan("SELECT * FROM u -- trailing comment\n WHERE a = 1").root,
            LogicalOp::scan("u").filter(Expr::eq("a", 1i64))
        );
    }

    // -- what it refuses, and why that matters more than what it accepts

    #[test]
    fn write_statements_are_refused_by_name() {
        for kw in [
            "INSERT INTO u VALUES (1)",
            "UPDATE u SET a = 1",
            "DELETE FROM u",
        ] {
            let e = err(kw);
            assert!(e.message.contains("not supported"), "{kw} -> {}", e.message);
        }
    }

    #[test]
    fn a_second_join_is_refused_rather_than_silently_dropped() {
        let e = err("SELECT * FROM a JOIN b ON a.x = b.x JOIN c ON a.y = c.y");
        assert!(e.message.contains("one JOIN"), "{}", e.message);
    }

    #[test]
    fn group_by_without_an_aggregate_is_refused() {
        let e = err("SELECT * FROM u GROUP BY country");
        assert!(e.message.contains("GROUP BY"), "{}", e.message);
    }

    #[test]
    fn malformed_input_reports_where_it_went_wrong() {
        let e = err("SELECT * FROM");
        assert!(e.at.is_some(), "no position reported");
        let e2 = err("SELECT * FROM u WHERE");
        assert!(e2.at.is_some());
        let e3 = err("SELECT * FROM u WHERE a = 'unterminated");
        assert!(e3.message.contains("unterminated"), "{}", e3.message);
    }

    #[test]
    fn trailing_input_is_an_error_not_silently_ignored() {
        let e = err("SELECT * FROM u WHERE a = 1 garbage");
        assert!(e.message.contains("trailing"), "{}", e.message);
    }

    #[test]
    fn an_empty_statement_is_an_error() {
        assert!(parse_select("   ").is_err());
    }
}
