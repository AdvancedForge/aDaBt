//! One function per subcommand. Each takes the shared [`Options`] and parses
//! its own flags from the raw arguments.

use crate::harness::{self, format_per_op, format_table, RunConfig, RunReport};
use crate::workload::WorkloadKind;
use crate::{arg, flag, parsed, parsed_list, queries, scale as scale_bench, soak as soak_bench};
use crate::{usage, Options};
use adabt_core::policy::{Durability, Mode, Override, Policy, Priorities};
use adabt_core::store::LogicalStore as _;
use adabt_engine::Database;
use adabt_storage::heap::HeapStore;
use adabt_telemetry::Histogram;
use adabt_testkit::reference::ReferenceStore;
use std::path::{Path, PathBuf};

/// A long adaptive run against traffic that changes underneath it, checked
/// against a level-0 reference the whole way.
pub(crate) fn soak(opts: &Options, args: &[String]) {
    let cfg = soak_config(opts, args);
    let r = match soak_bench::run_soak(&opts.data_dir, &cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("soak failed: {e}");
            std::process::exit(1);
        }
    };
    print!("{}", soak_bench::format_soak(&r));
    if flag(args, "--log") {
        print_soak_log(&r);
    }
    if let Some(out) = arg(args, "--out") {
        let _ = std::fs::write(&out, soak_bench::format_soak(&r));
        eprintln!("wrote {out}");
    }
    // A divergence is a failed run, not a reported statistic.
    if r.divergence.is_some() {
        std::process::exit(1);
    }
}

/// The soak configuration: shared options plus the soak-only flags.
fn soak_config(opts: &Options, args: &[String]) -> soak_bench::SoakConfig {
    soak_bench::SoakConfig {
        size: opts.size,
        ops_per_phase: parsed(args, "--ops-per-phase", 20_000),
        seed: opts.seed,
        verify: !flag(args, "--no-verify"),
        verify_every: parsed(args, "--verify-every", 64),
        cycle_every: parsed(args, "--cycle-every", 500),
        priorities: Priorities {
            speed: parsed(args, "--speed", 9),
            resources: parsed(args, "--resources", 3),
            freedom: parsed(args, "--freedom", 5),
        },
    }
}

/// The adaptive decisions and experiments a soak made, for `--log`.
fn print_soak_log(r: &soak_bench::SoakReport) {
    println!("\n--- decisions ---\n{}", r.decision_log);
    println!("--- experiments ---");
    for e in &r.experiments {
        println!("{e}");
    }
}

/// Print every workload name, one per line.
pub(crate) fn list() {
    for w in WorkloadKind::ALL {
        println!("{}", w.as_str());
    }
}

/// One workload against one engine, printed as a table or as JSON.
pub(crate) fn run(opts: &Options, args: &[String]) {
    let workload = arg(args, "--workload")
        .map(|w| WorkloadKind::parse(&w).unwrap_or_else(|| usage()))
        .unwrap_or(WorkloadKind::PointLookup);
    let r = execute(opts, &run_config(opts, workload, 0));
    if flag(args, "--json") {
        println!("{}", r.to_json());
    } else {
        print!("{}", format_table(std::slice::from_ref(&r)));
        print!("{}", format_per_op(&r));
    }
}

/// Every chosen workload at every chosen optimization level, as one table.
pub(crate) fn matrix(opts: &Options, args: &[String]) {
    let workloads = parse_workloads(args);
    let levels = matrix_levels(opts, args);

    let mut reports = Vec::new();
    for w in workloads {
        for &level in &levels {
            reports.push(execute(opts, &run_config(opts, w, level)));
        }
    }
    print!("{}", format_table(&reports));

    if let Some(path) = arg(args, "--out") {
        write_json_array(&path, reports.iter().map(|r| r.to_json()));
    }
}

/// The `--workloads` list, defaulting to every workload.
fn parse_workloads(args: &[String]) -> Vec<WorkloadKind> {
    match arg(args, "--workloads") {
        None => WorkloadKind::ALL.to_vec(),
        Some(list) => list
            .split(',')
            .map(|s| WorkloadKind::parse(s.trim()).unwrap_or_else(|| usage()))
            .collect(),
    }
}

/// The `--levels` list for a matrix, refused unless the engine has levels.
///
/// Levels mean something only for the `engine` backend, the one the
/// optimization framework drives. Reject anything but 0 for the others
/// rather than emitting identical rows under different labels, which would
/// read as "optimization made no difference" instead of "does not apply".
fn matrix_levels(opts: &Options, args: &[String]) -> Vec<u8> {
    let levels: Vec<u8> = parsed_list(args, "--levels", vec![0]);
    if opts.engine != "engine" && levels.iter().any(|&l| l != 0) {
        eprintln!(
            "only the `engine` backend has optimization levels; \
             reference and heap are always level 0"
        );
        std::process::exit(2);
    }
    levels
}

/// A harness run of `workload` at `level`, sized by the shared options.
fn run_config(opts: &Options, workload: WorkloadKind, level: u8) -> RunConfig {
    RunConfig {
        workload,
        dataset_size: opts.size,
        ops: opts.ops,
        seed: opts.seed,
        level,
        max_secs: opts.duration,
        engine: opts.engine.clone(),
        ..Default::default()
    }
}

/// Every chosen query mix at every chosen level against the full engine.
pub(crate) fn query_matrix(opts: &Options, args: &[String]) {
    let mixes = parse_query_mixes(args);
    let levels: Vec<u8> = parsed_list(args, "--levels", vec![0, 1, 2, 3, 10]);
    // Optimizations to force off, whatever the level says.
    let disabled: Vec<String> = arg(args, "--disable")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();

    let mut reports = Vec::new();
    for mix in mixes {
        for &level in &levels {
            reports.push(run_query_cell(opts, mix, level, &disabled));
        }
    }
    print!("{}", queries::format_query_table(&reports));
    if let Some(path) = arg(args, "--out") {
        write_json_array(&path, reports.iter().map(queries::to_json));
    }
}

/// The `--queries` list, defaulting to every query mix.
fn parse_query_mixes(args: &[String]) -> Vec<queries::QueryMix> {
    match arg(args, "--queries") {
        None => queries::QueryMix::ALL.to_vec(),
        Some(list) => list
            .split(',')
            .map(|s| queries::QueryMix::parse(s.trim()).unwrap_or_else(|| usage()))
            .collect(),
    }
}

/// One query mix at one level, in a fresh database, exiting 1 on failure.
fn run_query_cell(
    opts: &Options,
    mix: queries::QueryMix,
    level: u8,
    disabled: &[String],
) -> queries::QueryReport {
    let scratch = Scratch::new(&opts.data_dir, &format!("q-{}-l{level}", mix.as_str()));
    let mut db = open_database(&scratch.0, query_policy(level, opts.durability, disabled));
    let qcfg = queries::QueryRunConfig {
        mix,
        level,
        dataset_size: opts.size,
        queries: opts.ops,
        seed: opts.seed,
        max_secs: opts.duration,
    };
    match queries::run_queries(&mut db, &qcfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: {e}", mix.as_str());
            std::process::exit(1);
        }
    }
}

/// A manual policy at `level` with `disabled` optimizations forced off.
///
/// Explicit overrides beat the level, which is what lets a benchmark isolate
/// one optimization from another that would otherwise mask it.
fn query_policy(level: u8, durability: Durability, disabled: &[String]) -> Policy {
    let mut policy = Policy::manual(level);
    policy.guarantees.durability = durability;
    if let Mode::Manual { overrides, .. } = &mut policy.mode {
        overrides.extend(disabled.iter().map(|n| Override::toggle(n.clone(), false)));
    }
    policy
}

/// Per-row cost as the dataset doubles towards `--max-rows`.
pub(crate) fn scale(opts: &Options, args: &[String]) {
    let max = parsed(args, "--max-rows", 8_000_000u64);
    let budget = parsed(args, "--budget-mb", 1_500u64);
    let pool_pages: usize = parsed(args, "--pool-pages", 0);
    scale_bench::run(&opts.data_dir, max, budget, pool_pages);
}

/// Where the time goes when fetching one stored record.
pub(crate) fn fetch_profile(args: &[String]) {
    let rows = parsed(args, "--size", 50_000u64);
    let reps: u32 = parsed(args, "--reps", 3);
    scale_bench::fetch_profile(rows, reps);
}

/// The cost of alternative in-memory record representations.
pub(crate) fn record_repr(args: &[String]) {
    let rows = parsed(args, "--size", 200_000u64);
    let reps: u32 = parsed(args, "--reps", 5);
    scale_bench::record_repr(rows, reps);
}

/// Memory of a hash index against a bitmap index over the same column.
pub(crate) fn index_scale(args: &[String]) {
    let rows = parsed(args, "--size", 1_000_000u64);
    scale_bench::index_comparison(rows);
}

/// The compiled identity-lookup path against the general one, then whole
/// record reads against single-field reads.
pub(crate) fn compiled(opts: &Options) {
    let scratch = Scratch::new(&opts.data_dir, "compiled");
    let mut policy = Policy::manual(10);
    policy.guarantees.durability = opts.durability;
    let mut db = open_database(&scratch.0, policy);
    load_users(&mut db, opts.size);
    db.optimize().unwrap();
    match queries::compiled_path_comparison(&mut db, opts.size, opts.ops) {
        Ok((general, compiled, paths)) => {
            print_path_table(&general, &compiled);
            println!(
                "\n{paths} shape(s) specialised, direct array: {}",
                db.has_direct_array("users")
            );
            let (whole, single) = queries::field_read_comparison(&mut db, opts.size, opts.ops);
            print_field_read_table(&whole, &single);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// Create the `users` collection and insert `size` records into it.
fn load_users(db: &mut Database, size: u64) {
    if let Err(e) = db.create_collection("users", queries::schema()) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    for i in 0..size {
        db.insert("users", adabt_core::ids::RecordId(i), queries::record(i))
            .unwrap();
    }
}

/// p50/p99/sample-count rows for the general and compiled lookup paths.
fn print_path_table(general: &Histogram, compiled: &Histogram) {
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "path", "p50 ns", "p99 ns", "samples"
    );
    println!("{}", "-".repeat(46));
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "general",
        general.percentile(50.0),
        general.percentile(99.0),
        general.count()
    );
    println!(
        "{:<12} {:>10} {:>10} {:>10}",
        "compiled",
        compiled.percentile(50.0),
        compiled.percentile(99.0),
        compiled.count()
    );
}

/// p50/p99 rows for whole-record reads against single-field reads.
fn print_field_read_table(whole: &Histogram, single: &Histogram) {
    println!("\n{:<12} {:>10} {:>10}", "read", "p50 ns", "p99 ns");
    println!("{}", "-".repeat(34));
    println!(
        "{:<12} {:>10} {:>10}",
        "whole record",
        whole.percentile(50.0),
        whole.percentile(99.0)
    );
    println!(
        "{:<12} {:>10} {:>10}",
        "one field",
        single.percentile(50.0),
        single.percentile(99.0)
    );
}

/// Write `items` (already JSON) to `path` as one JSON array, exiting 1 on failure.
fn write_json_array(path: &str, items: impl Iterator<Item = String>) {
    let body: Vec<String> = items.collect();
    if let Err(e) = std::fs::write(path, format!("[{}]", body.join(","))) {
        eprintln!("could not write {path}: {e}");
        std::process::exit(1);
    }
    eprintln!("wrote {path}");
}

/// Open the full engine at `dir` under `policy`, exiting 1 if it cannot.
fn open_database(dir: &Path, policy: Policy) -> Database {
    match Database::open(dir, policy) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("could not open database: {e}");
            std::process::exit(1);
        }
    }
}

/// Scratch directory for a heap run, removed when the run finishes.
///
/// Deliberately *not* `std::env::temp_dir()`. On most Linux systems /tmp is
/// tmpfs, where fsync never reaches a disk — so a durability benchmark run
/// there measures memory and reports that strict durability is nearly free.
struct Scratch(PathBuf);
impl Scratch {
    /// A fresh, empty directory named after `tag` under `base`.
    fn new(base: &Path, tag: &str) -> Self {
        let mut p = base.to_path_buf();
        p.push(format!("run-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Scratch(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run one workload against the `--engine` backend, exiting on any failure.
fn execute(opts: &Options, cfg: &RunConfig) -> RunReport {
    let outcome = match opts.engine.as_str() {
        "reference" => harness::run(&mut ReferenceStore::new(), cfg),
        "heap" => {
            let scratch = Scratch::new(&opts.data_dir, cfg.workload.as_str());
            let mut store = match HeapStore::open(&scratch.0, opts.durability, opts.pool) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("could not open heap store: {e}");
                    std::process::exit(1);
                }
            };
            harness::run(&mut store, cfg)
        }
        // The full engine, at whatever optimization level was asked for. This
        // is the row that makes a level x workload matrix mean anything.
        "engine" => {
            let scratch = Scratch::new(
                &opts.data_dir,
                &format!("{}-l{}", cfg.workload.as_str(), cfg.level),
            );
            let mut policy = Policy::manual(cfg.level);
            policy.guarantees.durability = opts.durability;
            let mut db = open_database(&scratch.0, policy);
            harness::run(&mut db, cfg)
        }
        _ => usage(),
    };
    match outcome {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: {e}", cfg.workload.as_str());
            std::process::exit(1);
        }
    }
}
