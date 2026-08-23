//! Benchmark driver.
//!
//! At M0 the only `LogicalStore` in existence is the reference model, so every
//! run is labelled `reference` and every level is 0. The harness is wired for
//! the matrix now so that the first real engine is measurable the day it lands
//! rather than after a retrofit; the reference numbers also serve as the
//! deliberately-unoptimised floor that Level 0 must not fall below.

mod harness;
mod resources;
mod workload;

use adabt_testkit::reference::ReferenceStore;
use harness::{format_per_op, format_table, run, RunConfig};
use workload::WorkloadKind;

fn usage() -> ! {
    eprintln!(
        "usage:
  adabt-bench run    [--workload NAME] [--size N] [--ops N] [--seed N] [--duration SECS] [--json]
  adabt-bench matrix [--workloads a,b,c] [--levels 0,1] [--size N] [--ops N] [--duration SECS] [--out FILE]
  adabt-bench list

workloads: {}",
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

fn parse_workloads(args: &[String]) -> Vec<WorkloadKind> {
    match arg(args, "--workloads") {
        None => WorkloadKind::ALL.to_vec(),
        Some(list) => list
            .split(',')
            .map(|s| {
                WorkloadKind::parse(s.trim()).unwrap_or_else(|| {
                    eprintln!("unknown workload: {s}");
                    usage()
                })
            })
            .collect(),
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

    match cmd {
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
                ..Default::default()
            };
            let mut store = ReferenceStore::new();
            match run(&mut store, &cfg) {
                Ok(r) if flag(&args, "--json") => println!("{}", r.to_json()),
                Ok(r) => {
                    print!("{}", format_table(std::slice::from_ref(&r)));
                    print!("{}", format_per_op(&r));
                }
                Err(e) => {
                    eprintln!("run failed: {e}");
                    std::process::exit(1);
                }
            }
        }

        "matrix" => {
            let workloads = parse_workloads(&args);
            // Levels are accepted but not yet meaningful: the optimization
            // framework lands in M3. Reject anything but 0 rather than emitting
            // identical rows under different labels, which would read as
            // "optimization made no difference" instead of "not built yet".
            let levels: Vec<u8> = arg(&args, "--levels")
                .map(|l| l.split(',').filter_map(|s| s.trim().parse().ok()).collect())
                .unwrap_or_else(|| vec![0]);
            if levels.iter().any(|&l| l != 0) {
                eprintln!(
                    "only level 0 exists at this milestone; the optimization framework lands in M3"
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
                        ..Default::default()
                    };
                    let mut store = ReferenceStore::new();
                    match run(&mut store, &cfg) {
                        Ok(r) => reports.push(r),
                        Err(e) => {
                            eprintln!("{}: {e}", w.as_str());
                            std::process::exit(1);
                        }
                    }
                }
            }
            print!("{}", format_table(&reports));

            if let Some(path) = arg(&args, "--out") {
                let body: Vec<String> = reports.iter().map(|r| r.to_json()).collect();
                let json = format!("[{}]", body.join(","));
                if let Err(e) = std::fs::write(&path, json) {
                    eprintln!("could not write {path}: {e}");
                    std::process::exit(1);
                }
                eprintln!("wrote {path}");
            }
        }

        _ => usage(),
    }
}
