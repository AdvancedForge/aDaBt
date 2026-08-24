//! Query-path benchmarking.
//!
//! The operation-level harness drives `LogicalStore` directly — get, update,
//! scan — and that surface is deliberately *not* where most optimizations live.
//! Plan caching, result caching and automatic indexing all sit on `query()`, so
//! a matrix built only on the operation path shows every level costing a little
//! and gaining nothing.
//!
//! That is a real finding about the harness rather than about the levels, and
//! the fix is to measure the surface the optimizations actually serve.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::store::LogicalStore;
use adabt_engine::Database;
use adabt_ir::plan::{Agg, AggKind, LogicalOp, LogicalPlan, SortKey};
use adabt_ir::{CmpOp, Expr};
use adabt_telemetry::Histogram;
use adabt_testkit::rng::Rng;

use crate::resources::ResourceSample;

const COUNTRIES: [&str; 8] = ["NO", "SE", "DK", "FI", "IS", "EE", "LV", "LT"];

pub fn schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("age", FieldType::I64),
            FieldDef::new("balance", FieldType::I64),
        ],
    )
    .unwrap()
}

pub fn record(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("country", COUNTRIES[(i % 8) as usize])
        .with("age", (i % 70) as i64)
        .with("balance", (i * 37 % 100_000) as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMix {
    /// Repeated equality filters with varying literals. What `auto_index` is for.
    PointFilter,
    /// The same handful of queries over and over. What a result cache is for.
    RepeatedQuery,
    /// Range predicates. Needs an ordered index to help.
    RangeFilter,
    /// Grouped aggregation over the whole collection.
    Aggregate,
    /// Fetch by identity through the query path.
    ByIdentity,
}

impl QueryMix {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "point_filter" => Self::PointFilter,
            "repeated_query" => Self::RepeatedQuery,
            "range_filter" => Self::RangeFilter,
            "aggregate" => Self::Aggregate,
            "by_identity" => Self::ByIdentity,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PointFilter => "point_filter",
            Self::RepeatedQuery => "repeated_query",
            Self::RangeFilter => "range_filter",
            Self::Aggregate => "aggregate",
            Self::ByIdentity => "by_identity",
        }
    }
    pub const ALL: [QueryMix; 5] = [
        Self::PointFilter,
        Self::RepeatedQuery,
        Self::RangeFilter,
        Self::Aggregate,
        Self::ByIdentity,
    ];

    pub fn next_plan(self, rng: &mut Rng, size: u64) -> LogicalPlan {
        let root = match self {
            Self::PointFilter => {
                let c = COUNTRIES[rng.below_usize(COUNTRIES.len())];
                LogicalOp::scan("users").filter(Expr::eq("country", c))
            }
            Self::RepeatedQuery => {
                // A tiny set of distinct queries, so a result cache can work.
                let c = COUNTRIES[rng.below_usize(2)];
                LogicalOp::scan("users").filter(Expr::eq("country", c))
            }
            Self::RangeFilter => {
                let lo = rng.below(60) as i64;
                LogicalOp::scan("users")
                    .filter(Expr::And(vec![
                        Expr::cmp("age", CmpOp::Ge, lo),
                        Expr::cmp("age", CmpOp::Lt, lo + 5),
                    ]))
                    .limit(50)
            }
            // Varied on purpose. An aggregate issued identically every time is
            // answered entirely by the result cache, which masks whatever the
            // access path underneath is doing — the first version of this mix
            // measured caching and reported it as columnar performance.
            Self::Aggregate => {
                let lo = rng.below(60) as i64;
                LogicalOp::scan("users")
                    .filter(Expr::cmp("age", CmpOp::Ge, lo))
                    .aggregate(
                        vec!["country".into()],
                        vec![Agg::count("n"), Agg::over(AggKind::Avg, "balance", "mean")],
                    )
            }
            Self::ByIdentity => LogicalOp::get("users", RecordId(rng.below(size))),
        };
        // A sort on the filtered mixes, so the plans are not trivial.
        let root = if matches!(self, Self::PointFilter) {
            root.sort(vec![SortKey {
                field: "balance".into(),
                descending: true,
            }])
            .limit(20)
        } else {
            root
        };
        LogicalPlan::new(root)
    }
}

pub struct QueryReport {
    pub mix: QueryMix,
    pub level: u8,
    pub queries: u64,
    pub rows: u64,
    pub wall_secs: f64,
    pub latency: Histogram,
    pub resources: ResourceSample,
    pub indexes: usize,
    pub plan_cache_hit_rate: Option<f64>,
    pub result_cache_hit_rate: Option<f64>,
    pub derived_bytes: usize,
}

impl QueryReport {
    pub fn throughput(&self) -> f64 {
        if self.wall_secs <= 0.0 {
            0.0
        } else {
            self.queries as f64 / self.wall_secs
        }
    }
}

pub struct QueryRunConfig {
    pub mix: QueryMix,
    pub level: u8,
    pub dataset_size: u64,
    pub queries: u64,
    pub seed: u64,
    pub max_secs: Option<f64>,
}

pub fn run_queries(
    db: &mut Database,
    cfg: &QueryRunConfig,
) -> adabt_core::error::Result<QueryReport> {
    db.create_collection("users", schema())?;
    for i in 0..cfg.dataset_size {
        db.insert("users", RecordId(i), record(i))?;
    }

    // Warm up, then re-optimize: usage-driven optimizations need evidence
    // before they can fire, so measuring without this would measure a level
    // that had not taken effect yet.
    let mut warm = Rng::new(cfg.seed ^ 0x5EED);
    for _ in 0..200 {
        let q = cfg.mix.next_plan(&mut warm, cfg.dataset_size);
        db.query(&q)?;
    }
    db.optimize()?;
    for _ in 0..200 {
        let q = cfg.mix.next_plan(&mut warm, cfg.dataset_size);
        db.query(&q)?;
    }

    let mut rng = Rng::new(cfg.seed);
    let mut latency = Histogram::new();
    let mut rows = 0u64;
    let res_start = ResourceSample::now();
    let start = std::time::Instant::now();
    let mut executed = 0u64;

    for i in 0..cfg.queries {
        if let Some(limit) = cfg.max_secs {
            if i % 64 == 0 && start.elapsed().as_secs_f64() >= limit {
                break;
            }
        }
        let q = cfg.mix.next_plan(&mut rng, cfg.dataset_size);
        let t = std::time::Instant::now();
        let out = db.query(&q)?;
        latency.record(t.elapsed().as_nanos() as u64);
        rows += out.len() as u64;
        executed += 1;
    }

    Ok(QueryReport {
        mix: cfg.mix,
        level: cfg.level,
        queries: executed,
        rows,
        wall_secs: start.elapsed().as_secs_f64(),
        latency,
        resources: ResourceSample::now().since(&res_start),
        indexes: db.index_specs().len(),
        plan_cache_hit_rate: db.plan_cache_stats().hit_rate(),
        result_cache_hit_rate: db.result_cache_stats().hit_rate(),
        derived_bytes: db.derived_memory_bytes(),
    })
}

pub fn format_query_table(reports: &[QueryReport]) -> String {
    let mut out = format!(
        "{:<16} {:>3} {:>9} {:>11} {:>10} {:>10} {:>4} {:>7} {:>7} {:>10}\n",
        "query", "lvl", "queries", "q/sec", "p50 ns", "p99 ns", "idx", "plan%", "res%", "derived"
    );
    out.push_str(&"-".repeat(96));
    out.push('\n');
    for r in reports {
        let pct = |v: Option<f64>| match v {
            Some(v) => format!("{:.0}", v * 100.0),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "{:<16} {:>3} {:>9} {:>11.0} {:>10} {:>10} {:>4} {:>7} {:>7} {:>9.1}M\n",
            r.mix.as_str(),
            r.level,
            r.queries,
            r.throughput(),
            r.latency.percentile(50.0),
            r.latency.percentile(99.0),
            r.indexes,
            pct(r.plan_cache_hit_rate),
            pct(r.result_cache_hit_rate),
            r.derived_bytes as f64 / 1e6,
        ));
    }
    out
}

pub fn to_json(r: &QueryReport) -> String {
    format!(
        concat!(
            "{{\"query\":\"{q}\",\"level\":{lv},\"queries\":{n},\"rows\":{rows},",
            "\"wall_secs\":{wall:.6},\"queries_per_sec\":{tput:.2},",
            "\"latency_ns\":{{\"p50\":{p50},\"p99\":{p99},\"max\":{max}}},",
            "\"indexes\":{idx},\"derived_bytes\":{db},\"peak_rss_bytes\":{rss}}}"
        ),
        q = r.mix.as_str(),
        lv = r.level,
        n = r.queries,
        rows = r.rows,
        wall = r.wall_secs,
        tput = r.throughput(),
        p50 = r.latency.percentile(50.0),
        p99 = r.latency.percentile(99.0),
        max = r.latency.max(),
        idx = r.indexes,
        db = r.derived_bytes,
        rss = r.resources.peak_rss_bytes,
    )
}

/// Measure the compiled path against the general one for identity lookups.
///
/// Reported rather than assumed: "removing generality from the hot path" is the
/// project's central claim about Level 10-11, and a specialisation that is not
/// measurably faster is just more code to maintain.
pub fn compiled_path_comparison(
    db: &mut Database,
    dataset: u64,
    samples: u64,
) -> adabt_core::error::Result<(Histogram, Histogram, usize)> {
    let mut rng = Rng::new(0xC0FFEE);
    let plans: Vec<LogicalPlan> = (0..samples)
        .map(|_| LogicalPlan::new(LogicalOp::get("users", RecordId(rng.below(dataset)))))
        .collect();

    // Phase one: below the specialisation threshold, so the general path runs.
    let mut general = Histogram::new();
    for p in plans.iter().take(200) {
        let t = std::time::Instant::now();
        db.query(p)?;
        general.record(t.elapsed().as_nanos() as u64);
    }

    // Warm past the threshold.
    for p in plans.iter() {
        db.query(p)?;
    }

    let mut compiled = Histogram::new();
    for p in plans.iter() {
        let t = std::time::Instant::now();
        db.query(p)?;
        compiled.record(t.elapsed().as_nanos() as u64);
    }
    Ok((general, compiled, db.compiled_paths()))
}

/// Whole-record decode against a single-field read from a computed address.
///
/// The Level 11 claim reduced to a measurement: how much of a lookup's cost is
/// generality — decoding fields nobody asked for, allocating a record to hold
/// them — rather than the work of finding the data.
pub fn field_read_comparison(
    db: &mut Database,
    dataset: u64,
    samples: u64,
) -> (Histogram, Histogram) {
    let mut rng = Rng::new(0x11FE1D);
    let ids: Vec<RecordId> = (0..samples).map(|_| RecordId(rng.below(dataset))).collect();

    let mut whole = Histogram::new();
    for id in &ids {
        let t = std::time::Instant::now();
        let r = db.get("users", *id).unwrap();
        std::hint::black_box(r.and_then(|r| r.get("balance").cloned()));
        whole.record(t.elapsed().as_nanos() as u64);
    }

    let mut single = Histogram::new();
    for id in &ids {
        let t = std::time::Instant::now();
        std::hint::black_box(db.field_of("users", *id, "balance").unwrap());
        single.record(t.elapsed().as_nanos() as u64);
    }
    (whole, single)
}
