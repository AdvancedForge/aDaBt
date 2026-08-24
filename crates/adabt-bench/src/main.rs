//! Benchmark driver.
//!
//! Two engines are measurable: the reference model (an in-memory `BTreeMap`
//! with no durability at all) and the heap store (pages, buffer pool,
//! write-ahead log). The reference is not a performance target — it is a
//! *correctness* floor that cheats, and the gap between the two is the price of
//! not losing data when the power fails.

mod harness;
mod queries;
mod resources;
mod scale;
mod soak;
mod workload;

use adabt_core::policy::Durability;
use adabt_core::policy::Policy;
use adabt_core::store::LogicalStore as _;
use adabt_engine::Database;
use adabt_storage::heap::HeapStore;
use adabt_testkit::reference::ReferenceStore;
use harness::{format_per_op, format_table, run, RunConfig, RunReport};
use workload::WorkloadKind;

fn usage() -> ! {
    eprintln!(
        "usage:
  adabt-bench run    [--engine E] [--workload W] [--size N] [--ops N] [--seed N]
                     [--data-dir PATH]
                     [--duration SECS] [--durability D] [--pool N] [--json]
  adabt-bench matrix [--engine E] [--workloads a,b,c] [--levels 0] [--size N] [--ops N]
                     [--duration SECS] [--durability D] [--pool N] [--out FILE]
  adabt-bench query-matrix [--queries a,b] [--levels 0,1,2,3,10] [--size N] [--ops N]
                           [--duration SECS] [--data-dir PATH] [--disable a,b] [--out FILE]
  adabt-bench soak   [--size N] [--ops-per-phase N] [--seed N] [--data-dir PATH]
                     [--cycle-every N] [--verify-every N] [--no-verify] [--log] [--out FILE]
                     [--speed N] [--resources N] [--freedom N]
  adabt-bench list

engines:      reference, heap, engine
durability:   strict, group, relaxed
workloads:    {}",
        WorkloadKind::ALL
            .iter()
            .map(|w| w.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    std::process::exit(2)
}

fn arg(args: &[String], key: &str) -> Option<String> {
    let i = args.iter().position(|a| a == key)?;
    args.get(i + 1).cloned()
}

fn flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

fn parse_durability(s: &str) -> Durability {
    match s {
        "strict" => Durability::Strict,
        "group" => Durability::GroupCommit,
        "relaxed" => Durability::Relaxed,
        _ => usage(),
    }
}

/// Scratch directory for a heap run, removed when the run finishes.
///
/// Deliberately *not* `std::env::temp_dir()`. On most Linux systems /tmp is
/// tmpfs, where fsync never reaches a disk — so a durability benchmark run
/// there measures memory and reports that strict durability is nearly free.
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new(base: &std::path::Path, tag: &str) -> Self {
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

fn execute(
    engine: &str,
    cfg: &RunConfig,
    durability: Durability,
    pool: usize,
    data_dir: &std::path::Path,
) -> RunReport {
    let outcome = match engine {
        "reference" => run(&mut ReferenceStore::new(), cfg),
        "heap" => {
            let scratch = Scratch::new(data_dir, cfg.workload.as_str());
            let mut store = match HeapStore::open(&scratch.0, durability, pool) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("could not open heap store: {e}");
                    std::process::exit(1);
                }
            };
            run(&mut store, cfg)
        }
        // The full engine, at whatever optimization level was asked for. This
        // is the row that makes a level x workload matrix mean anything.
        "engine" => {
            let scratch = Scratch::new(
                data_dir,
                &format!("{}-l{}", cfg.workload.as_str(), cfg.level),
            );
            let mut policy = Policy::manual(cfg.level);
            policy.guarantees.durability = durability;
            let mut db = match Database::open(&scratch.0, policy) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("could not open database: {e}");
                    std::process::exit(1);
                }
            };
            run(&mut db, cfg)
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");

    let size: u64 = arg(&args, "--size")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let ops: u64 = arg(&args, "--ops")
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let seed: u64 = arg(&args, "--seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xADAB7);
    // A matrix spans workloads whose per-op cost differs by orders of
    // magnitude, so every run is bounded by wall-clock as well as op count.
    let duration: Option<f64> = arg(&args, "--duration")
        .and_then(|s| s.parse().ok())
        .or(Some(10.0));
    let engine = arg(&args, "--engine").unwrap_or_else(|| "reference".into());
    let durability =
        parse_durability(&arg(&args, "--durability").unwrap_or_else(|| "strict".into()));
    // Optimizations to force off, whatever the level says.
    let disabled: Vec<String> = arg(&args, "--disable")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();
    let pool: usize = arg(&args, "--pool")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);

    // Default to a real disk. /tmp is tmpfs on most systems, where fsync is a
    // no-op and every durability number is meaningless.
    let data_dir = arg(&args, "--data-dir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let mut p = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            p.push(".cache/adabt-bench");
            p
        });
    if engine == "heap" || cmd == "query-matrix" {
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            eprintln!("could not create {}: {e}", data_dir.display());
            std::process::exit(1);
        }
        if resources::is_memory_backed(&data_dir) {
            eprintln!(
                "WARNING: {} is on {} (memory-backed). fsync does not reach a disk there,\n\
                 so durability numbers from this run are not real. Pass --data-dir <path on disk>.",
                data_dir.display(),
                resources::filesystem_type(&data_dir).unwrap_or_else(|| "?".into())
            );
        }
    }

    match cmd {
        // A long adaptive run against traffic that changes underneath it,
        // checked against a level-0 reference the whole way.
        "soak" => {
            let cfg = soak::SoakConfig {
                size,
                ops_per_phase: arg(&args, "--ops-per-phase")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(20_000),
                seed,
                verify: !flag(&args, "--no-verify"),
                verify_every: arg(&args, "--verify-every")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(64),
                cycle_every: arg(&args, "--cycle-every")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(500),
                priorities: adabt_core::policy::Priorities {
                    speed: arg(&args, "--speed")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(9),
                    resources: arg(&args, "--resources")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(3),
                    freedom: arg(&args, "--freedom")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(5),
                },
            };
            let r = match soak::run_soak(&data_dir, &cfg) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("soak failed: {e}");
                    std::process::exit(1);
                }
            };
            print!("{}", soak::format_soak(&r));
            if flag(&args, "--log") {
                println!("\n--- decisions ---\n{}", r.decision_log);
                println!("--- experiments ---");
                for e in &r.experiments {
                    println!("{e}");
                }
            }
            if let Some(out) = arg(&args, "--out") {
                let _ = std::fs::write(&out, soak::format_soak(&r));
                eprintln!("wrote {out}");
            }
            // A divergence is a failed run, not a reported statistic.
            if r.divergence.is_some() {
                std::process::exit(1);
            }
        }

        "list" => {
            for w in WorkloadKind::ALL {
                println!("{}", w.as_str());
            }
        }

        "run" => {
            let workload = arg(&args, "--workload")
                .map(|w| WorkloadKind::parse(&w).unwrap_or_else(|| usage()))
                .unwrap_or(WorkloadKind::PointLookup);
            let cfg = RunConfig {
                workload,
                dataset_size: size,
                ops,
                seed,
                max_secs: duration,
                engine: engine.clone(),
                ..Default::default()
            };
            let r = execute(&engine, &cfg, durability, pool, &data_dir);
            if flag(&args, "--json") {
                println!("{}", r.to_json());
            } else {
                print!("{}", format_table(std::slice::from_ref(&r)));
                print!("{}", format_per_op(&r));
            }
        }

        "matrix" => {
            let workloads: Vec<WorkloadKind> = match arg(&args, "--workloads") {
                None => WorkloadKind::ALL.to_vec(),
                Some(list) => list
                    .split(',')
                    .map(|s| WorkloadKind::parse(s.trim()).unwrap_or_else(|| usage()))
                    .collect(),
            };
            // Levels are accepted but not yet meaningful: the optimization
            // framework lands in M3. Reject anything but 0 rather than emitting
            // identical rows under different labels, which would read as
            // "optimization made no difference" instead of "not built yet".
            let levels: Vec<u8> = arg(&args, "--levels")
                .map(|l| l.split(',').filter_map(|s| s.trim().parse().ok()).collect())
                .unwrap_or_else(|| vec![0]);
            if engine != "engine" && levels.iter().any(|&l| l != 0) {
                eprintln!(
                    "only the `engine` backend has optimization levels; \
                     reference and heap are always level 0"
                );
                std::process::exit(2);
            }

            let mut reports = Vec::new();
            for w in workloads {
                for &level in &levels {
                    let cfg = RunConfig {
                        workload: w,
                        dataset_size: size,
                        ops,
                        seed,
                        level,
                        max_secs: duration,
                        engine: engine.clone(),
                        ..Default::default()
                    };
                    reports.push(execute(&engine, &cfg, durability, pool, &data_dir));
                }
            }
            print!("{}", format_table(&reports));

            if let Some(path) = arg(&args, "--out") {
                let body: Vec<String> = reports.iter().map(|r| r.to_json()).collect();
                if let Err(e) = std::fs::write(&path, format!("[{}]", body.join(","))) {
                    eprintln!("could not write {path}: {e}");
                    std::process::exit(1);
                }
                eprintln!("wrote {path}");
            }
        }

        "query-matrix" => {
            let mixes: Vec<queries::QueryMix> = match arg(&args, "--queries") {
                None => queries::QueryMix::ALL.to_vec(),
                Some(list) => list
                    .split(',')
                    .map(|s| queries::QueryMix::parse(s.trim()).unwrap_or_else(|| usage()))
                    .collect(),
            };
            let levels: Vec<u8> = arg(&args, "--levels")
                .map(|l| l.split(',').filter_map(|s| s.trim().parse().ok()).collect())
                .unwrap_or_else(|| vec![0, 1, 2, 3, 10]);

            let mut reports = Vec::new();
            for mix in mixes {
                for &level in &levels {
                    let scratch = Scratch::new(&data_dir, &format!("q-{}-l{level}", mix.as_str()));
                    let mut policy = Policy::manual(level);
                    policy.guarantees.durability = durability;
                    // Explicit overrides beat the level, which is what lets a
                    // benchmark isolate one optimization from another that
                    // would otherwise mask it.
                    if let adabt_core::policy::Mode::Manual { overrides, .. } = &mut policy.mode {
                        overrides.extend(
                            disabled
                                .iter()
                                .map(|n| adabt_core::policy::Override::toggle(n.clone(), false)),
                        );
                    }
                    let mut db = match Database::open(&scratch.0, policy) {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!("could not open database: {e}");
                            std::process::exit(1);
                        }
                    };
                    let qcfg = queries::QueryRunConfig {
                        mix,
                        level,
                        dataset_size: size,
                        queries: ops,
                        seed,
                        max_secs: duration,
                    };
                    match queries::run_queries(&mut db, &qcfg) {
                        Ok(r) => reports.push(r),
                        Err(e) => {
                            eprintln!("{}: {e}", mix.as_str());
                            std::process::exit(1);
                        }
                    }
                }
            }
            print!("{}", queries::format_query_table(&reports));
            if let Some(path) = arg(&args, "--out") {
                let body: Vec<String> = reports.iter().map(queries::to_json).collect();
                if let Err(e) = std::fs::write(&path, format!("[{}]", body.join(","))) {
                    eprintln!("could not write {path}: {e}");
                    std::process::exit(1);
                }
                eprintln!("wrote {path}");
            }
        }

        "scale" => {
            let max = arg(&args, "--max-rows")
                .and_then(|v| v.parse().ok())
                .unwrap_or(8_000_000u64);
            let budget = arg(&args, "--budget-mb")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_500u64);
            let pool_pages: usize = arg(&args, "--pool-pages")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            scale::run(&data_dir, max, budget, pool_pages);
        }
        "fetch-profile" => {
            let rows = arg(&args, "--size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(50_000u64);
            let reps: u32 = arg(&args, "--reps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(3);
            scale::fetch_profile(rows, reps);
        }
        "record-repr" => {
            let rows = arg(&args, "--size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(200_000u64);
            let reps: u32 = arg(&args, "--reps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(5);
            scale::record_repr(rows, reps);
        }
        "index-scale" => {
            let rows = arg(&args, "--size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_000_000u64);
            scale::index_comparison(rows);
        }
        "compiled" => {
            let scratch = Scratch::new(&data_dir, "compiled");
            let mut policy = Policy::manual(10);
            policy.guarantees.durability = durability;
            let mut db = match Database::open(&scratch.0, policy) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("could not open database: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = db.create_collection("users", queries::schema()) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            for i in 0..size {
                db.insert("users", adabt_core::ids::RecordId(i), queries::record(i))
                    .unwrap();
            }
            db.optimize().unwrap();
            match queries::compiled_path_comparison(&mut db, size, ops) {
                Ok((general, compiled, paths)) => {
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
                    println!(
                        "\n{paths} shape(s) specialised, direct array: {}",
                        db.has_direct_array("users")
                    );
                    let (whole, single) = queries::field_read_comparison(&mut db, size, ops);
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
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }

        _ => usage(),
    }
}
