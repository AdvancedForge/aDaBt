//! The SQL shell.
//!
//! M37 built the parser and stopped there; this is the other half — the thing
//! a person types into. It is deliberately thin: every execution rule lives in
//! the IR and the engine, and the shell only maps text onto them, because a
//! front-end with its own evaluation rules would be a second engine that could
//! disagree with the first.
//!
//! The parser refuses far more than it accepts, by name. That discipline is
//! inherited here unchanged: a statement the front-end does not implement
//! produces an error naming the construct, never something *near* what was
//! asked.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use adabt_core::policy::{Durability, Policy};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = match args.next() {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!("usage: adabt-cli <data-dir> [--level N] [--strict]");
            std::process::exit(2);
        }
    };
    let mut level = 4u8;
    let mut strict = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--level" => {
                level = args.next().and_then(|v| v.parse().ok()).unwrap_or(4);
            }
            "--version" => {
                println!("adabt-cli {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--strict" => strict = true,
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }

    let mut policy = Policy::manual(level);
    policy.guarantees.durability = if strict {
        Durability::Strict
    } else {
        Durability::Relaxed
    };

    let mut db = match Database::open(&dir, policy) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("could not open {}: {e}", dir.display());
            std::process::exit(1);
        }
    };

    println!(
        "aDaBt shell — {} — level {level}, {} durability",
        dir.display(),
        if strict { "strict" } else { "relaxed" }
    );
    println!("SELECT only. .explain <sql>, .tables, .indexes, .help, .quit");

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("adabt> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next() else { break };
        let line = line.unwrap_or_default();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match trimmed {
            ".quit" | ".exit" => break,
            ".help" => println!("{}", HELP),
            ".tables" => {
                for t in db.collection_names() {
                    println!("{t}");
                }
            }
            ".indexes" => {
                for s in db.index_specs() {
                    println!("{}.{} ({})", s.collection, s.field, s.kind.as_str());
                }
            }
            ".explain" => {
                // The rest of the line after ".explain" is SQL to plan, not run.
                match sql_of(trimmed, ".explain") {
                    Some(sql) => match run_line(&mut db, &format!(".explain {sql}")) {
                        Ok(out) => print!("{out}"),
                        Err(e) => println!("error: {e}"),
                    },
                    None => println!("usage: .explain <select statement>"),
                }
            }
            _ => match run_line(&mut db, trimmed) {
                Ok(out) => print!("{out}"),
                Err(e) => println!("error: {e}"),
            },
        }
    }
}

fn sql_of<'a>(line: &'a str, cmd: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(cmd)?;
    rest.split_whitespace().next().map(|_| rest.trim_start())
}

/// One line of input, evaluated. Separated from the REPL so tests can drive
/// it without a TTY.
pub fn run_line(db: &mut Database, line: &str) -> Result<String, String> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix(".explain") {
        let sql = rest.trim_start();
        let plan = adabt_ir::sql::parse_select(sql).map_err(|e| e.to_string())?;
        return Ok(format!("{}\n", db.explain(&plan)));
    }
    let plan = adabt_ir::sql::parse_select(trimmed).map_err(|e| e.to_string())?;
    let rows = db.query(&plan).map_err(|e| e.to_string())?;
    Ok(render(&rows))
}

/// Rows as a fixed-width table. Column order comes from the first row — the
/// IR keeps record fields sorted, so the same shape always prints the same
/// way — and an empty result prints its emptiness rather than nothing.
fn render(rows: &[(adabt_core::ids::RecordId, adabt_core::record::Record)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let Some((_, first)) = rows.first() else {
        return String::from("(0 rows)\n");
    };
    let cols: Vec<String> = first.iter().map(|(k, _)| k.to_string()).collect();
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for (_, r) in rows {
        cells.push(r.iter().map(|(_, v)| format_value(v)).collect());
    }
    let mut widths = cols.iter().map(|c| c.len()).collect::<Vec<_>>();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.len());
        }
    }
    for (w, c) in widths.iter().zip(&cols) {
        let _ = write!(out, "{c:>w$}  ");
    }
    out.push('\n');
    for w in &widths {
        let _ = write!(out, "{}  ", "-".repeat(*w));
    }
    out.push('\n');
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            let _ = write!(out, "{c:>width$}  ", width = widths[i]);
        }
        out.push('\n');
    }
    let _ = writeln!(out, "({} rows)", rows.len());
    out
}

const HELP: &str = "\
.quit            leave the shell
.tables          list collections
.indexes         list index definitions
.explain <sql>   show the logical and physical plan for a SELECT
<sql>            any SELECT the front-end accepts (WHERE, GROUP BY,
                 ORDER BY, LIMIT, one JOIN). Writes are refused by the
                 parser, by name.";

/// The shell's own value rendering. Deliberately plain — a reading aid, not a
/// serialization format; anything a program needs should travel through the
/// wire protocol, where encoding is exact and versioned.
fn format_value(v: &adabt_core::value::Value) -> String {
    use adabt_core::value::Value;
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Decimal { units, scale } => {
            // Exact by construction: integer digits with the point placed
            // back in. No floating point anywhere near it — this project's
            // whole position on money is that it stays exact in the last
            // place, including on screen.
            let negative = *units < 0;
            let digits = units.unsigned_abs().to_string();
            if *scale == 0 {
                let _ = negative;
                return digits;
            }
            let point = digits.len().saturating_sub(*scale as usize);
            let body = if point == 0 {
                format!("0.{:0>width$}", digits, width = *scale as usize)
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            };
            if negative {
                format!("-{body}")
            } else {
                body
            }
        }
        Value::Timestamp(ns) => {
            // Milliseconds are what a shell is for; full precision belongs to
            // the wire protocol.
            let secs = ns.div_euclid(1_000_000_000);
            let millis = ns.rem_euclid(1_000_000_000) / 1_000_000;
            format!("<ts {secs}.{millis:03}>")
        }
        Value::Str(s) => s.clone(),
        Value::Bytes(_) => "<bytes>".into(),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(m) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k}: {}", format_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_core::ids::RecordId;
    use adabt_core::record::Record;
    use adabt_core::schema::Schema;
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn db() -> (Tmp, Database) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-cli-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        let mut db = Database::open(&p, Policy::manual(0)).unwrap();
        db.create_collection("users", Schema::dynamic()).unwrap();
        for i in 0..3u64 {
            db.insert(
                "users",
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("name", format!("u{i}"))
                    .with("age", 20 + i as i64),
            )
            .unwrap();
        }
        (Tmp(p), db)
    }

    #[test]
    fn a_select_renders_a_table() {
        let (_t, mut db) = db();
        let out = run_line(&mut db, "SELECT id, name, age FROM users ORDER BY age").unwrap();
        assert!(out.contains("id"), "{out}");
        assert!(out.contains("(3 rows)"), "{out}");
        // Ordered by age means u0 first among the data lines.
        let first_data = out.lines().find(|l| l.contains('u')).unwrap();
        assert!(first_data.contains("u0"), "{out}");
    }

    #[test]
    fn an_empty_result_says_so() {
        let (_t, mut db) = db();
        let out = run_line(&mut db, "SELECT name FROM users WHERE age > 999").unwrap();
        assert!(out.contains("(0 rows)"), "{out}");
    }

    #[test]
    fn writes_are_refused_by_name() {
        let (_t, mut db) = db();
        let err = run_line(&mut db, "INSERT INTO users VALUES (1)").unwrap_err();
        assert!(
            err.to_lowercase().contains("insert"),
            "the error should name the construct: {err}"
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_shrug() {
        let (_t, mut db) = db();
        assert!(run_line(&mut db, "SELECT FROM WHERE").is_err());
        assert!(run_line(&mut db, "totally not sql").is_err());
    }

    #[test]
    fn explain_shows_the_physical_plan() {
        let (_t, mut db) = db();
        let out = run_line(&mut db, ".explain SELECT name FROM users WHERE id = 1").unwrap();
        assert!(out.contains("physical:"), "{out}");
        assert!(out.contains("rationale:"), "{out}");
    }
}
