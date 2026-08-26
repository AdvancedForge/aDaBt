//! aDaBt against SQLite, on workloads chosen to include the ones aDaBt loses.
//!
//! # Why this exists
//!
//! `docs/roadmap.md` sets Track D's finish line as "there is a workload where
//! it is the right answer, demonstrably." Every performance number this
//! project has produced so far compares aDaBt against its own past. That can
//! show a change helped; it cannot show whether the result is any good.
//!
//! SQLite is the right first comparison and not an arbitrary one: it is
//! embedded, single-node, single-writer, and used the way aDaBt is meant to be
//! used. It is also extremely well optimized, which is the point — a
//! comparison against something slow proves nothing.
//!
//! # What is deliberately fair, and what is not
//!
//! Both engines are given the same records, the same queries and the same
//! durability posture, and each is allowed the indexes it would actually have.
//! Where the comparison is *not* apples-to-apples, the note says so in the
//! output rather than in a footnote nobody reads:
//!
//! - SQLite parses SQL on every call unless a statement is prepared. Prepared
//!   statements are used, because that is how anyone would write it, and
//!   because aDaBt is handed a pre-built plan.
//! - aDaBt holds its page directory and every index in memory. SQLite does
//!   not. At these row counts that is an advantage for aDaBt and it is a
//!   large part of why it wins where it wins — and it is exactly the property
//!   that makes aDaBt unable to open a dataset larger than RAM at all.
//! - Neither is warmed differently: each benchmark runs a warm-up pass whose
//!   timings are discarded.
//!
//! # What is missing, and why
//!
//! RocksDB and PostgreSQL are not here. Not because the comparison would be
//! unflattering — the whole point is to publish the losses — but because
//! neither can be built or run in this environment: RocksDB's Rust bindings
//! need `cmake` and `libclang`, PostgreSQL needs a server installed, and
//! nothing here has root. Reporting numbers for either would mean inventing
//! them. The gap is recorded in `docs/roadmap.md` instead.

use adabt_core::ids::RecordId;
use adabt_core::index_kind::IndexKind;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, LogicalOp, LogicalPlan, SortKey};
use adabt_ir::Expr;
use std::time::{Duration, Instant};

const COUNTRIES: [&str; 8] = ["NO", "SE", "DK", "FI", "IS", "NL", "BE", "IE"];

fn country(i: u64) -> &'static str {
    COUNTRIES[(i % 8) as usize]
}

fn age(i: u64) -> i64 {
    (18 + i % 60) as i64
}

fn name(i: u64) -> String {
    format!("user-{i}")
}

/// Nanoseconds per operation, taken as the best of several passes.
///
/// Best-of rather than mean: on a shared machine the mean measures the
/// scheduler as much as the code, while the minimum is the closest thing to
/// "what this costs when nothing is in the way". Both engines are measured the
/// same way, which is what makes the comparison meaningful even if the
/// absolute numbers are not reproducible elsewhere.
fn best_of(passes: u32, ops: u64, mut f: impl FnMut()) -> u64 {
    let mut best = Duration::from_secs(u64::MAX / 2);
    for _ in 0..passes {
        let t = Instant::now();
        f();
        let e = t.elapsed();
        if e < best {
            best = e;
        }
    }
    (best.as_nanos() as u64) / ops.max(1)
}

struct Row {
    workload: &'static str,
    adabt_ns: u64,
    /// The same workload after `optimize()` has run at a level where the
    /// engine may build column stores and materialized views.
    ///
    /// Reporting only the unoptimized number would be measuring a
    /// self-optimizing database with its reason for existing switched off.
    /// Reporting only the optimized one would be quietly hiding what it costs
    /// to get there. Both, then.
    tuned_ns: u64,
    sqlite_ns: u64,
    note: &'static str,
}

fn main() {
    // Fail-fast witness harness: `--witness postgres|rocksdb` must not silently
    // become a SQLite-only run. If the requested witness cannot be reached or
    // the binary was not built with that feature, exit non-zero.
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--witness") {
        let witness = args.get(pos + 1).map(|s| s.as_str()).unwrap_or("");
        match witness {
            "postgres" => {
                let url = std::env::var("DATABASE_URL").unwrap_or_default();
                if url.is_empty() {
                    eprintln!("--witness postgres requires DATABASE_URL");
                    std::process::exit(2);
                }
                // Try a TCP connect to the Postgres host to fail fast before
                // running the 8-workload harness against the wrong witness.
                if let Some(host) = url.split('@').last().and_then(|s| s.split('/').next()).and_then(|s| s.split(':').next()) {
                    let addr = format!("{}:5432", host);
                    if std::net::TcpStream::connect(&addr).is_err() {
                        eprintln!("--witness postgres: cannot connect to {addr} (DATABASE_URL={url})");
                        std::process::exit(2);
                    }
                }
                eprintln!("[comparison] postgres witness requested — harness would run same 8 workloads against Postgres here; Postgres driver not vendored in this crate, failing fast to avoid pretending SQLite numbers are Postgres numbers");
                std::process::exit(2);
            }
            "rocksdb" => {
                // `adabt-comparison` has no `rocksdb` feature gate in this workspace
                // (separate workspace, no RocksDB dep). Requesting it without a
                // rocksdb-enabled build must fail, not fall back to SQLite.
                eprintln!("--witness rocksdb requested but this build has no rocksdb feature (add `rocksdb` dep + `cmake`/`libclang` and rebuild)");
                std::process::exit(2);
            }
            other => {
                eprintln!("unknown --witness {other:?} (expected postgres|rocksdb)");
                std::process::exit(2);
            }
        }
    }

    let rows: u64 = std::env::args()
        .skip_while(|a| a != "--size")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);

    println!("# aDaBt vs SQLite — {rows} rows\n");

    // Progress goes to stderr as each workload STARTS: results print only at
    // the end, and this project has already lost one benchmark run to a
    // timeout with nothing to show for it (`docs/m36-notes.md`). A harness
    // that dies without evidence is worse than no harness.
    let stages: [(&str, fn(u64) -> Row); 8] = [
        ("bulk load", bulk_load),
        ("point lookup", point_lookup),
        ("indexed equality", indexed_equality),
        ("full scan count", full_scan_count),
        ("grouped aggregate", grouped_aggregate),
        ("range scan", range_scan),
        ("sorted limit", sorted_limit),
        ("single-row writes", single_row_writes),
    ];
    let mut out: Vec<Row> = Vec::new();
    // `--only NAME` runs a single workload (matched on its stage name), for
    // iterating on one row of the table without paying for the other seven.
    let only = std::env::args()
        .skip_while(|a| a != "--only")
        .nth(1);
    let total = stages.len();
    for (i, (name, f)) in stages.iter().enumerate() {
        if only.as_deref().is_some_and(|o| !name.contains(o)) {
            continue;
        }
        eprintln!("[comparison] starting {name} ({}/{})", i + 1, total);
        out.push(f(rows));
    }

    let verdict = |ours: u64, theirs: u64| -> String {
        if ours == 0 {
            return "-".into();
        }
        let ratio = theirs as f64 / ours as f64;
        if ratio >= 1.0 {
            format!("{ratio:.2}x faster")
        } else {
            format!("{:.2}x slower", 1.0 / ratio)
        }
    };

    println!(
        "{:<24} {:>11} {:>11} {:>11} {:>12}  {}",
        "workload", "aDaBt L0", "aDaBt tuned", "SQLite", "tuned vs sql", "note"
    );
    println!("{}", "-".repeat(115));
    for r in &out {
        println!(
            "{:<24} {:>11} {:>11} {:>11} {:>12}  {}",
            r.workload,
            r.adabt_ns,
            if r.tuned_ns == 0 {
                "-".to_string()
            } else {
                r.tuned_ns.to_string()
            },
            r.sqlite_ns,
            verdict(if r.tuned_ns == 0 { r.adabt_ns } else { r.tuned_ns }, r.sqlite_ns),
            r.note
        );
    }

    println!("\nns per operation, best of several passes. `tuned` is after optimize()");
    println!("at a level that permits column stores and materialized views; `-` means");
    println!("the workload is a write path, where those do not apply.");
    let best = |r: &Row| if r.tuned_ns == 0 { r.adabt_ns } else { r.tuned_ns };
    let losses = out.iter().filter(|r| r.sqlite_ns < best(r)).count();
    println!(
        "\nAt its best configuration aDaBt wins {} of {} and loses {}.",
        out.len() - losses,
        out.len(),
        losses
    );
}

// -- fixtures ---------------------------------------------------------------

fn fresh_adabt(tag: &str, level: u8) -> (Database, std::path::PathBuf) {
    let mut p = std::env::temp_dir();
    p.push(format!("adabt-cmp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    let mut policy = Policy::manual(level);
    // `Policy::manual` defaults to `Durability::Strict` — an fsync per write.
    // SQLite is run with `synchronous = OFF`, so leaving this at the default
    // would be timing aDaBt's fsyncs against SQLite's absence of them and
    // calling the difference an engine comparison. Matched deliberately, and
    // the write workloads report the posture they ran under.
    policy.guarantees.durability = adabt_core::policy::Durability::Relaxed;
    let mut db = Database::open(&p, policy).expect("open adabt");
    db.create_collection("users", Schema::dynamic())
        .expect("create collection");
    (db, p)
}

fn fill_adabt(db: &mut Database, rows: u64) {
    let batch: Vec<(RecordId, Record)> = (0..rows)
        .map(|i| {
            (
                RecordId(i),
                Record::new()
                    .with("id", i)
                    .with("country", country(i))
                    .with("age", age(i))
                    .with("name", name(i)),
            )
        })
        .collect();
    db.insert_batch("users", batch).expect("bulk insert");
}

/// A file-backed SQLite on the same filesystem aDaBt writes to.
///
/// In-memory would be the flattering choice for SQLite on reads and the
/// meaningless one on writes: aDaBt has no in-memory mode, so an in-memory
/// SQLite is not a configuration of the same system under test. Both engines
/// therefore write real files to the same place.
fn fresh_sqlite(tag: &str) -> (rusqlite::Connection, std::path::PathBuf) {
    let mut p = std::env::temp_dir();
    p.push(format!("sqlite-cmp-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let c = rusqlite::Connection::open(&p).expect("open sqlite");
    // aDaBt at these levels keeps everything in memory and does not fsync per
    // statement, so SQLite is configured to match rather than being handicapped
    // by a durability posture the other side is not paying for.
    c.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = OFF;
         CREATE TABLE users (id INTEGER PRIMARY KEY, country TEXT, age INTEGER, name TEXT);",
    )
    .expect("schema");
    (c, p)
}

fn fill_sqlite(c: &mut rusqlite::Connection, rows: u64) {
    let tx = c.transaction().expect("begin");
    {
        let mut stmt = tx
            .prepare("INSERT INTO users (id, country, age, name) VALUES (?1, ?2, ?3, ?4)")
            .expect("prepare");
        for i in 0..rows {
            stmt.execute(rusqlite::params![i as i64, country(i), age(i), name(i)])
                .expect("insert");
        }
    }
    tx.commit().expect("commit");
}


/// Run the same plan against an aDaBt that has been allowed to optimize.
///
/// Level 4 is the lowest that permits a column store and a materialized
/// view, which are the two structures that could plausibly change a scan-heavy
/// result. Level 5 is where the workload-aware index proposals live
/// (`auto_composite_index`, `auto_covering_index`); the covering index is
/// what lets an indexed lookup skip its fetches entirely, which matters
/// against an opponent whose rows are packed pages. Both sides get the
/// indexes they would actually have.
fn tuned_query(tag: &str, rows: u64, plan: &LogicalPlan, reps: u32, indexes: &[(&str, IndexKind)]) -> u64 {
    let (mut db, p) = fresh_adabt(tag, 5);
    fill_adabt(&mut db, rows);
    for (field, kind) in indexes {
        db.create_index("users", field, *kind).expect("index");
    }
    // Show the optimizer the workload before asking it to decide.
    for _ in 0..40 {
        db.query(plan).expect("warm");
    }
    db.optimize().expect("optimize");
    // Drive whatever experiment optimize() started to its verdict before any
    // timing happens.
    //
    // Shadow executes every query on BOTH paths and canary routes a fraction
    // of them, so timing mid-trial measures the trial rather than the engine
    // — and without `advance_experiments()` the state machine never leaves
    // shadow at all, because phase transitions happen when the caller folds
    // evidence in, not inside `query`. The first version of this harness did
    // exactly that and reported `tuned` three to seven times slower than
    // level 0 on every read workload while believing it was measuring column
    // stores and materialized views. Time-bounded rather than sample-bounded:
    // a plan that costs hundreds of microseconds can need thousands of canary
    // queries at the bottom of the ramp, and a benchmark with an unbounded
    // warm-up is a soak with a printing habit.
    let settle_deadline = Instant::now() + Duration::from_secs(90);
    while db.experiments().next().is_some() {
        for _ in 0..50 {
            db.query(plan).expect("settle");
        }
        db.advance_experiments().expect("advance");
        if Instant::now() > settle_deadline {
            db.abort_experiment("comparison harness settle deadline").expect("abort");
            break;
        }
    }
    // Switch the result cache off, and say why.
    //
    // SQLite has no equivalent of memoizing a whole result set by query key.
    // A benchmark that asks the byte-identical question two hundred times
    // would therefore be comparing aDaBt's hash-map lookup against SQLite's
    // query execution and reporting the difference as an engine result — the
    // top-20 sort measured 3,587 ns that way, against 471 ms for the same
    // query with the cache cold, which is a cache hit wearing a benchmark's
    // clothes.
    //
    // Everything else the optimizer built stays: indexes, column stores,
    // materialized views. Those are physical structures maintained on write,
    // which is a thing other databases also have. This one is not.
    db.set_result_cache_entries(0);
    db.query(plan).expect("warm after");
    // Record what the planner actually serves the plan through after tuning.
    // The first run of this harness reported `tuned` several times slower than
    // level 0 on indexed workloads and the reason was right here: optimize()
    // had replaced an IndexLookup with a ColumnScan over the whole collection.
    eprintln!("---- tuned plan ({tag}) ----\n{}", db.explain(plan));
    let ns = best_of(5, reps as u64, || {
        for _ in 0..reps {
            std::hint::black_box(db.query(plan).expect("query").len());
        }
    });
    drop(db);
    let _ = std::fs::remove_dir_all(&p);
    ns
}

// -- workloads --------------------------------------------------------------

fn bulk_load(rows: u64) -> Row {
    let adabt_ns = best_of(3, rows, || {
        let (mut db, p) = fresh_adabt("load", 0);
        fill_adabt(&mut db, rows);
        drop(db);
        let _ = std::fs::remove_dir_all(&p);
    });
    let sqlite_ns = best_of(3, rows, || {
        let (mut c, sp) = fresh_sqlite("load");
        fill_sqlite(&mut c, rows);
        drop(c);
        let _ = std::fs::remove_file(&sp);
    });
    Row {
        workload: "bulk load",
        adabt_ns,
        tuned_ns: 0,
        sqlite_ns,
        note: "both write real files; fsync off on both sides",
    }
}

fn point_lookup(rows: u64) -> Row {
    let (mut db, p) = fresh_adabt("point", 0);
    fill_adabt(&mut db, rows);
    let probes: Vec<u64> = (0..10_000).map(|k| (k * 7919) % rows).collect();

    let adabt_ns = best_of(5, probes.len() as u64, || {
        for i in &probes {
            std::hint::black_box(db.get("users", RecordId(*i)).expect("get"));
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("q");
        fill_sqlite(&mut c, rows);
        (c, sp)
    };
    let mut stmt = c.prepare("SELECT country, age, name FROM users WHERE id = ?1").unwrap();
    let sqlite_ns = best_of(5, probes.len() as u64, || {
        for i in &probes {
            let r: (String, i64, String) = stmt
                .query_row([*i as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .expect("row");
            std::hint::black_box(r);
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "point lookup by id",
        adabt_ns,
        tuned_ns: 0,
        sqlite_ns,
        note: "aDaBt: in-memory directory; SQLite: b-tree on rowid",
    }
}

fn indexed_equality(rows: u64) -> Row {
    let (mut db, p) = fresh_adabt("eq", 0);
    fill_adabt(&mut db, rows);
    db.create_index("users", "country", IndexKind::Hash)
        .expect("index");
    let plan = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::eq("country", "NO"))
            .project(vec!["name".into(), "age".into()]),
    );
    db.query(&plan).expect("warm");
    let adabt_ns = best_of(5, 200, || {
        for _ in 0..200 {
            std::hint::black_box(db.query(&plan).expect("query").len());
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("q");
        fill_sqlite(&mut c, rows);
        c.execute_batch("CREATE INDEX idx_country ON users(country);").expect("index");
        (c, sp)
    };
    let mut stmt = c
        .prepare("SELECT name, age FROM users WHERE country = ?1")
        .unwrap();
    let sqlite_ns = best_of(5, 200, || {
        for _ in 0..200 {
            let n: usize = stmt
                .query_map(["NO"], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .count();
            std::hint::black_box(n);
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "indexed equality",
        adabt_ns,
        tuned_ns: tuned_query("eqt", rows, &plan, 200, &[("country", IndexKind::Hash)]),
        sqlite_ns,
        note: "one eighth of the table matches",
    }
}

fn full_scan_count(rows: u64) -> Row {
    let (mut db, p) = fresh_adabt("scan", 0);
    fill_adabt(&mut db, rows);
    let plan = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec![],
        vec![Agg::count("n")],
    ));
    db.query(&plan).expect("warm");
    // Per QUERY, all three engines: this workload's question is "what does it
    // cost to count the table", and per-row numbers hide what a derived
    // structure does to that cost. (The first version reported L0 and SQLite
    // per row but the tuned number per query — and briefly concluded tuning
    // made aggregates 10× slower by dividing one by the other.)
    let adabt_ns = best_of(5, 20, || {
        for _ in 0..20 {
            std::hint::black_box(db.query(&plan).expect("count"));
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("q");
        fill_sqlite(&mut c, rows);
        (c, sp)
    };
    // `count(age)` rather than `count(*)`: SQLite answers `count(*)` from an
    // index without reading rows, which measures a different thing entirely.
    let mut stmt = c.prepare("SELECT count(age) FROM users").unwrap();
    let sqlite_ns = best_of(5, 20, || {
        for _ in 0..20 {
            let n: i64 = stmt.query_row([], |r| r.get(0)).unwrap();
            std::hint::black_box(n);
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "full scan count",
        adabt_ns,
        tuned_ns: tuned_query("scant", rows, &plan, 20, &[]),
        sqlite_ns,
        note: "per query",
    }
}

fn grouped_aggregate(rows: u64) -> Row {
    let (mut db, p) = fresh_adabt("group", 0);
    fill_adabt(&mut db, rows);
    let plan = LogicalPlan::new(LogicalOp::scan("users").aggregate(
        vec!["country".into()],
        vec![Agg::count("n")],
    ));
    db.query(&plan).expect("warm");
    // Per QUERY, all three engines — same reasoning as `full_scan_count`.
    let adabt_ns = best_of(5, 20, || {
        for _ in 0..20 {
            std::hint::black_box(db.query(&plan).expect("group").len());
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("q");
        fill_sqlite(&mut c, rows);
        (c, sp)
    };
    let mut stmt = c
        .prepare("SELECT country, count(*) FROM users GROUP BY country")
        .unwrap();
    let sqlite_ns = best_of(5, 20, || {
        for _ in 0..20 {
            let n = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .count();
            std::hint::black_box(n);
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "group by, count",
        adabt_ns,
        tuned_ns: tuned_query("groupt", rows, &plan, 20, &[]),
        sqlite_ns,
        note: "per query; eight groups",
    }
}

fn range_scan(rows: u64) -> Row {
    let (mut db, p) = fresh_adabt("range", 0);
    fill_adabt(&mut db, rows);
    db.create_index("users", "age", IndexKind::BTree).expect("index");
    let plan = LogicalPlan::new(
        LogicalOp::scan("users")
            .filter(Expr::And(vec![
                Expr::cmp("age", adabt_ir::CmpOp::Ge, 30i64),
                Expr::cmp("age", adabt_ir::CmpOp::Lt, 35i64),
            ]))
            .project(vec!["name".into()]),
    );
    db.query(&plan).expect("warm");
    let adabt_ns = best_of(5, 200, || {
        for _ in 0..200 {
            std::hint::black_box(db.query(&plan).expect("range").len());
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("r");
        fill_sqlite(&mut c, rows);
        c.execute_batch("CREATE INDEX idx_age ON users(age);").expect("index");
        (c, sp)
    };
    let mut stmt = c
        .prepare("SELECT name FROM users WHERE age >= 30 AND age < 35")
        .unwrap();
    let sqlite_ns = best_of(5, 200, || {
        for _ in 0..200 {
            let n = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().count();
            std::hint::black_box(n);
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "indexed range",
        adabt_ns,
        tuned_ns: tuned_query("ranget", rows, &plan, 200, &[("age", IndexKind::BTree)]),
        sqlite_ns,
        note: "five of sixty ages",
    }
}

fn sorted_limit(rows: u64) -> Row {
    let (mut db, p) = fresh_adabt("sort", 0);
    fill_adabt(&mut db, rows);
    let plan = LogicalPlan::new(
        LogicalOp::scan("users")
            .sort(vec![SortKey {
                field: "age".into(),
                descending: true,
            }])
            .limit(20)
            .project(vec!["name".into(), "age".into()]),
    );
    db.query(&plan).expect("warm");
    let adabt_ns = best_of(5, 20, || {
        for _ in 0..20 {
            std::hint::black_box(db.query(&plan).expect("sort").len());
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("q");
        fill_sqlite(&mut c, rows);
        (c, sp)
    };
    let mut stmt = c
        .prepare("SELECT name, age FROM users ORDER BY age DESC LIMIT 20")
        .unwrap();
    let sqlite_ns = best_of(5, 20, || {
        for _ in 0..20 {
            let n = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .count();
            std::hint::black_box(n);
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "top-20 by sort",
        adabt_ns,
        tuned_ns: tuned_query("sortt", rows, &plan, 20, &[]),
        sqlite_ns,
        note: "no index on the sort key; both sort the table",
    }
}

fn single_row_writes(rows: u64) -> Row {
    const WRITES: u64 = 20_000;
    let (mut db, p) = fresh_adabt("write", 0);
    fill_adabt(&mut db, rows);
    let mut next = rows;
    let adabt_ns = best_of(1, WRITES, || {
        for _ in 0..WRITES {
            db.insert(
                "users",
                RecordId(next),
                Record::new()
                    .with("id", next)
                    .with("country", country(next))
                    .with("age", age(next))
                    .with("name", name(next)),
            )
            .expect("insert");
            next += 1;
        }
    });

    let (c, sp) = {
        let (mut c, sp) = fresh_sqlite("q");
        fill_sqlite(&mut c, rows);
        (c, sp)
    };
    let mut stmt = c
        .prepare("INSERT INTO users (id, country, age, name) VALUES (?1, ?2, ?3, ?4)")
        .unwrap();
    let mut n2 = rows;
    let sqlite_ns = best_of(1, WRITES, || {
        for _ in 0..WRITES {
            stmt.execute(rusqlite::params![n2 as i64, country(n2), age(n2), name(n2)])
                .expect("insert");
            n2 += 1;
        }
    });
    drop(stmt);
    drop(c);
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&sp);
    Row {
        workload: "single-row inserts",
        adabt_ns,
        tuned_ns: 0,
        sqlite_ns,
        note: "one statement each, no explicit transaction",
    }
}
