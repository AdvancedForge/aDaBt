//! Scale evidence.
//!
//! Every performance claim in `docs/` up to this point rests on a 5,000-row
//! soak. This measures them where they might actually break: at the largest
//! row counts this machine can hold, with resident memory tracked so the
//! *ceiling itself* is a measurement rather than a guess.
//!
//! # What is being tested, and what would refute it
//!
//! - **Scale ceiling.** `HeapStore` keeps a `BTreeMap<RecordId, VersionChain>`
//!   entry per record and every index lives entirely in memory. So the
//!   maximum row count is bounded by RAM, not disk, and the interesting
//!   number is bytes of resident memory per row. If that is large, the
//!   plan's "100M+ records" target is unreachable without a paged directory,
//!   and saying so is worth more than a benchmark that quietly tests 100k.
//! - **Bitmap vs hash index (M25).** The claim is that a bitmap index is
//!   cheaper on a low-cardinality field. That was measured at 5,000 rows over
//!   4 values. Refuted if the advantage vanishes — or inverts — at scale.
//! - **Query latency vs row count.** An indexed point lookup should stay
//!   roughly flat as rows grow; a full scan should grow linearly. If the
//!   indexed path grows with the collection, an index is not doing what every
//!   cost estimate in `adabt-opt` assumes it does.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore as _;
use adabt_engine::Database;
use adabt_index::IndexKind;
use adabt_ir::plan::{LogicalOp, LogicalPlan};
use adabt_ir::Expr;
use std::time::Instant;

/// Resident set size in bytes, from `/proc/self/statm`.
///
/// Read rather than estimated: the whole point of this harness is to find
/// the real ceiling, and a computed estimate of memory use would be assuming
/// the answer.
pub fn rss_bytes() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let pages: u64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Strict,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            // Four distinct values: the low-cardinality shape a bitmap index
            // exists for.
            FieldDef::new("status", FieldType::Str { max_len: Some(12) }),
            FieldDef::new("bucket", FieldType::I64),
        ],
    )
    .unwrap()
}

const STATUSES: [&str; 4] = ["active", "closed", "pending", "archived"];

fn record(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("status", STATUSES[(i % 4) as usize])
        .with("bucket", (i % 1000) as i64)
}

/// Load `n` records, reporting load rate and the memory it cost.
fn load(db: &mut Database, n: u64, batch: u64) -> (f64, u64) {
    let before = rss_bytes();
    let start = Instant::now();
    let mut i = 0u64;
    while i < n {
        let upper = (i + batch).min(n);
        let rows: Vec<(RecordId, Record)> = (i..upper).map(|k| (RecordId(k), record(k))).collect();
        db.insert_batch("users", rows).expect("batch insert");
        i = upper;
    }
    let secs = start.elapsed().as_secs_f64();
    let after = rss_bytes();
    (n as f64 / secs, after.saturating_sub(before))
}

fn time_query(db: &mut Database, plan: &LogicalPlan, reps: u32) -> u64 {
    // Warm once so the first call's cache misses are not the measurement.
    let _ = db.query(plan);
    let start = Instant::now();
    for _ in 0..reps {
        db.query(plan).expect("query");
    }
    (start.elapsed().as_nanos() / reps.max(1) as u128) as u64
}

/// Run the scale ladder, doubling until `max_rows` or memory runs short.
pub fn run(data_dir: &std::path::Path, max_rows: u64, budget_mb: u64, pool_pages: usize) {
    println!(
        "{:<12} {:>10} {:>12} {:>12} {:>12} {:>12}",
        "rows", "load/s", "rss MB", "bytes/row", "point ns", "scan ns"
    );
    println!("{}", "-".repeat(76));

    // Start small and double: the per-row cost is the thing being measured,
    // so the ladder has to have several rungs below the ceiling to show it is
    // flat rather than assumed.
    let mut rows = 100_000u64.min(max_rows);
    let mut per_row_seen = 0u64;
    let mut prev_rows = 0u64;
    let mut prev_rss = 0u64;
    loop {
        let dir = data_dir.join(format!("scale-{rows}"));
        let _ = std::fs::remove_dir_all(&dir);
        // NOTE: not a per-rung baseline. The allocator does not return memory
        // to the OS between rungs, so RSS-before-open is already inflated by
        // the previous rung and subtracting it reports zero. Marginal cost is
        // computed against the previous rung's total instead, which is the
        // honest measurement of what each additional row costs.
        let mut db = match Database::open(&dir, Policy::manual(0)) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("open failed at {rows} rows: {e}");
                return;
            }
        };
        if pool_pages > 0 {
            db.set_pool_capacity(pool_pages).expect("pool");
        }
        db.create_collection("users", schema()).expect("collection");

        let (rate, _) = load(&mut db, rows, 10_000);
        let rss = rss_bytes();
        let per_row = if prev_rows == 0 {
            rss / rows.max(1)
        } else {
            rss.saturating_sub(prev_rss) / rows.saturating_sub(prev_rows).max(1)
        };

        // An indexed point lookup: should stay roughly flat as rows grow.
        db.create_index("users", "id", IndexKind::Hash)
            .expect("index");
        let point = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("id", rows / 2)));
        let point_ns = time_query(&mut db, &point, 20);

        // A full scan with a filter that matches a quarter of rows: should
        // grow linearly. This is the control that proves the point lookup's
        // flatness is the index working, not the measurement failing.
        let scan = LogicalPlan::new(LogicalOp::scan("users").filter(Expr::eq("status", "active")));
        let scan_ns = time_query(&mut db, &scan, 3);

        println!(
            "{:<12} {:>10.0} {:>12.1} {:>12} {:>12} {:>12}",
            rows,
            rate,
            rss as f64 / 1e6,
            per_row,
            point_ns,
            scan_ns
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);

        per_row_seen = per_row.max(per_row_seen);
        prev_rows = rows;
        prev_rss = rss;
        if rows >= max_rows {
            break;
        }
        let next = (rows * 2).min(max_rows);
        // Projected *before* attempting it, not checked afterwards. Checking
        // after means the run that exhausts memory is the one that has
        // already happened — the machine swaps, and the measurement becomes
        // one of the kernel rather than the database. This harness got that
        // wrong first and was killed at 30 minutes with nothing to show.
        let projected_mb = (next.saturating_mul(per_row_seen)) / 1_000_000;
        if projected_mb > budget_mb {
            println!(
                "\nstopped before {next} rows: projected {projected_mb} MB exceeds the \
                 {budget_mb} MB budget at {per_row_seen} bytes/row"
            );
            break;
        }
        rows = next;
    }
}

/// Bitmap vs hash index on a low-cardinality field, at a given row count.
///
/// M25 claimed bitmap is cheaper here and measured it at 5,000 rows over 4
/// values. This is the same claim at whatever scale is asked for.
pub fn index_comparison(rows: u64) {
    use adabt_index::{BitmapIndex, HashIndex, Index};

    let mut hash = HashIndex::new("status");
    let mut bitmap = BitmapIndex::new("status");
    for i in 0..rows {
        let r = record(i);
        hash.index_record(RecordId(i), &r);
        bitmap.index_record(RecordId(i), &r);
    }
    let h = hash.memory_bytes();
    let b = bitmap.memory_bytes();

    let key = adabt_core::value::Value::Str("active".into());
    let t0 = Instant::now();
    let hn = hash.lookup(&key).len();
    let h_ns = t0.elapsed().as_nanos();
    let t1 = Instant::now();
    let bn = bitmap.lookup(&key).len();
    let b_ns = t1.elapsed().as_nanos();

    println!("rows: {rows}, distinct values: {}", STATUSES.len());
    println!(
        "{:<10} {:>14} {:>14} {:>10}",
        "index", "bytes", "lookup ns", "matched"
    );
    println!("{}", "-".repeat(52));
    println!("{:<10} {:>14} {:>14} {:>10}", "hash", h, h_ns, hn);
    println!("{:<10} {:>14} {:>14} {:>10}", "bitmap", b, b_ns, bn);
    println!(
        "\nbitmap uses {:.2}x the memory of hash, and returns the same {} rows",
        b as f64 / h as f64,
        if hn == bn { "identical" } else { "DIFFERENT" }
    );
    assert_eq!(hn, bn, "the two index kinds disagreed about what matches");
}

/// Where the per-row fetch cost actually goes.
///
/// M36 measured a full scan at roughly 6 µs/row and showed it was *not*
/// I/O-bound: a 64× larger buffer pool changed nothing. That refuted one
/// hypothesis but did not produce another, and "CPU-side somewhere in the
/// fetch path" is not a target you can optimize against.
///
/// So this decomposes the path into the layers it is actually made of, each
/// timed on the same records, so the cost lands on a named layer:
///
/// | layer | adds |
/// |---|---|
/// | `decode` | `RecordCodec::decode` alone — no storage at all |
/// | `decompress+decode` | the payload copy `read_at` makes before decoding |
/// | `Source::fetch` | what the executor calls per row: name resolution, masking, directory, pool |
/// | `LogicalStore::get` | the same read plus the engine's telemetry — not on the scan path |
/// | `scan` | the whole query path: batching, physical plan, `Vec` building |
///
/// Each row is the *cumulative* cost, so the difference between two adjacent
/// rows is what that layer costs. A layer that dominates is the thing to fix;
/// a layer that is nearly free is one to leave alone regardless of how
/// suspicious it looks.
pub fn fetch_profile(rows: u64, reps: u32) {
    use adabt_storage::codec::RecordCodec;
    use adabt_storage::compress::{decompress, Encoding};

    let schema = schema();
    let codec = RecordCodec::new(schema.clone());

    // One representative record, encoded the way the heap stores it.
    let sample = record(7);
    let encoded = codec.encode(&sample).expect("encode");

    let n = rows.max(1);
    let mut out: Vec<(&str, u64)> = Vec::new();

    // 1. Decode alone. No file, no pool, no directory: the pure CPU cost of
    //    turning bytes into a `Record`.
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..reps {
        for _ in 0..n {
            let r = codec.decode(&encoded).expect("decode");
            sink += r.len();
        }
    }
    out.push(("decode", t.elapsed().as_nanos() as u64 / (n * reps as u64)));

    // 2. The copy `read_at` makes on every read. With compression off — the
    //    default — this is `Encoding::Raw`, which still allocates and memcpys
    //    the whole payload before anything looks at it.
    let t = Instant::now();
    for _ in 0..reps {
        for _ in 0..n {
            let bytes = decompress(Encoding::Raw, &encoded).expect("decompress");
            let r = codec.decode(&bytes).expect("decode");
            sink += r.len();
        }
    }
    out.push((
        "decompress+decode",
        t.elapsed().as_nanos() as u64 / (n * reps as u64),
    ));
    std::hint::black_box(sink);

    // 3. Through the store, one record at a time.
    let dir = std::env::temp_dir().join(format!("adabt-fetch-profile-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = Database::open(&dir, Policy::manual(0)).expect("open");
    db.create_collection("users", schema).expect("create");
    load(&mut db, n, 20_000);

    let ids: Vec<RecordId> = (0..n).map(RecordId).collect();

    // 3a. `Source::fetch` — what the executor actually calls, once per row.
    //     Resolves the collection by name, checks candidate masking, checks
    //     for a direct array, then descends the page directory and reads.
    let t = Instant::now();
    let mut found = 0u64;
    for _ in 0..reps {
        for id in &ids {
            if adabt_exec::exec::Source::fetch(&mut db, "users", *id)
                .expect("fetch")
                .is_some()
            {
                found += 1;
            }
        }
    }
    out.push((
        "Source::fetch",
        t.elapsed().as_nanos() as u64 / (n * reps as u64),
    ));

    // 3b. `LogicalStore::get` — the same read plus the engine's own
    //     bookkeeping: two `Instant::now()` calls and a telemetry event. Not
    //     on the scan path; here to keep 3a honest about what it excludes.
    let t = Instant::now();
    for _ in 0..reps {
        for id in &ids {
            if db.get("users", *id).expect("get").is_some() {
                found += 1;
            }
        }
    }
    out.push((
        "LogicalStore::get",
        t.elapsed().as_nanos() as u64 / (n * reps as u64),
    ));
    std::hint::black_box(found);

    // 4. The whole query path, which is what an application actually pays.
    let plan = LogicalPlan::new(LogicalOp::Scan {
        collection: "users".into(),
    });
    let t = Instant::now();
    for _ in 0..reps {
        let r = db.query(&plan).expect("scan");
        std::hint::black_box(r.len());
    }
    out.push(("scan", t.elapsed().as_nanos() as u64 / (n * reps as u64)));

    println!("{n} rows, {reps} reps — nanoseconds per row, cumulative\n");
    println!("{:<22} {:>10} {:>10}", "layer", "ns/row", "adds");
    println!("{}", "-".repeat(44));
    let mut prev = 0u64;
    for (name, ns) in &out {
        println!("{:<22} {:>10} {:>10}", name, ns, ns.saturating_sub(prev));
        prev = *ns;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Is `Record`'s representation the cost, or is value parsing?
///
/// `decode` costs ~250 ns/row and there are two candidates inside it: parsing
/// bytes into `Value`s, and building the `BTreeMap<String, Value>` that holds
/// them — which allocates a `String` per field per row and a tree node per
/// field per row. Item 1 of the roadmap rewrites the second and leaves the
/// first alone, so it is worth knowing which one it is before rewriting
/// anything. Guessing this wrong is how the last two optimization attempts
/// went.
///
/// The comparison is the map against the shape that replaces it — field names
/// shared from the schema rather than cloned, and a sorted `Vec` rather than a
/// tree — with identical field names and identical values, so the only
/// difference measured is the container.
pub fn record_repr(rows: u64, reps: u32) {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let names: Vec<&str> = vec!["id", "status", "bucket"];
    let shared: Vec<Arc<str>> = names.iter().map(|n| Arc::from(*n)).collect();
    let vals: Vec<adabt_core::value::Value> = vec![
        adabt_core::value::Value::U64(42),
        adabt_core::value::Value::Str("active".into()),
        adabt_core::value::Value::I64(7),
    ];

    let n = rows.max(1) * reps as u64;

    // What decode does today: clone the name, insert into a tree.
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..n {
        let mut m: BTreeMap<String, adabt_core::value::Value> = BTreeMap::new();
        for (k, name) in names.iter().enumerate() {
            m.insert((*name).to_string(), vals[k].clone());
        }
        sink += m.len();
    }
    let owned = t.elapsed().as_nanos() as u64 / n;

    // What it would do instead: bump a refcount, push onto one Vec.
    let t = Instant::now();
    for _ in 0..n {
        let mut v: Vec<(Arc<str>, adabt_core::value::Value)> = Vec::with_capacity(names.len());
        for (k, name) in shared.iter().enumerate() {
            v.push((Arc::clone(name), vals[k].clone()));
        }
        sink += v.len();
    }
    let arced = t.elapsed().as_nanos() as u64 / n;
    std::hint::black_box(sink);

    println!("{n} constructions of a {}-field record\n", names.len());
    println!("{:<34} {:>10}", "representation", "ns/record");
    println!("{}", "-".repeat(46));
    println!("{:<34} {:>10}", "BTreeMap<String, Value>", owned);
    println!("{:<34} {:>10}", "Vec<(Arc<str>, Value)>", arced);
    println!(
        "\nthe container is {} ns/record of the ~250 ns decode costs",
        owned.saturating_sub(arced)
    );
}
