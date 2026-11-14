//! Benchmark driver.
//!
//! Two engines are measurable: the reference model (an in-memory `BTreeMap`
//! with no durability at all) and the heap store (pages, buffer pool,
//! write-ahead log). The reference is not a performance target — it is a
//! *correctness* floor that cheats, and the gap between the two is the price of
//! not losing data when the power fails.

mod commands;
mod harness;
mod queries;
mod resources;
mod scale;
mod soak;
mod workload;

use adabt_core::policy::Durability;
use std::path::{Path, PathBuf};
use workload::WorkloadKind;

/// Print the command-line synopsis and exit with status 2.
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

/// The raw value following `key`, if the flag is present.
fn arg(args: &[String], key: &str) -> Option<String> {
    let i = args.iter().position(|a| a == key)?;
    args.get(i + 1).cloned()
}

/// The value of `key` parsed as `T`, or `default` when the flag is absent.
/// A present but malformed value is a usage error, never a silent default:
/// a benchmark that quietly ran with other settings reports wrong numbers.
fn parsed<T: std::str::FromStr>(args: &[String], key: &str, default: T) -> T {
    match arg(args, key) {
        None => default,
        Some(v) => v.parse().unwrap_or_else(|_| {
            eprintln!("{key} expects a number, got {v:?}");
            std::process::exit(2)
        }),
    }
}

/// A comma-separated list of `T`, with the same refusal rule as [`parsed`].
fn parsed_list<T: std::str::FromStr>(args: &[String], key: &str, default: Vec<T>) -> Vec<T> {
    match arg(args, key) {
        None => default,
        Some(list) => list
            .split(',')
            .map(|item| {
                item.trim().parse().unwrap_or_else(|_| {
                    eprintln!("{key} expects comma-separated numbers, got {item:?}");
                    std::process::exit(2)
                })
            })
            .collect(),
    }
}

/// Whether the boolean flag `key` is present.
fn flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

/// The durability level named `s`, or a usage error.
fn parse_durability(s: &str) -> Durability {
    match s {
        "strict" => Durability::Strict,
        "group" => Durability::GroupCommit,
        "relaxed" => Durability::Relaxed,
        _ => usage(),
    }
}

/// Flags shared by the subcommands, parsed (and validated) before dispatch.
///
/// Every one is checked whichever subcommand runs, so a malformed shared
/// flag is a usage error even where that subcommand would ignore it.
struct Options {
    size: u64,
    ops: u64,
    seed: u64,
    /// Wall-clock bound per run, alongside the op count.
    duration: Option<f64>,
    engine: String,
    durability: Durability,
    pool: usize,
    data_dir: PathBuf,
}

impl Options {
    /// Parse the shared flags from the full argument list.
    fn parse(args: &[String]) -> Self {
        Options {
            size: parsed(args, "--size", 100_000),
            ops: parsed(args, "--ops", 200_000),
            seed: parsed(args, "--seed", 0xADAB7),
            // A matrix spans workloads whose per-op cost differs by orders of
            // magnitude, so every run is bounded by wall-clock as well as op count.
            duration: Some(parsed(args, "--duration", 10.0)),
            engine: arg(args, "--engine").unwrap_or_else(|| "reference".into()),
            durability: parse_durability(
                &arg(args, "--durability").unwrap_or_else(|| "strict".into()),
            ),
            pool: parsed(args, "--pool", 1024),
            data_dir: arg(args, "--data-dir")
                .map(PathBuf::from)
                .unwrap_or_else(default_data_dir),
        }
    }
}

/// `~/.cache/adabt-bench`, or `./.cache/adabt-bench` when `HOME` is unset.
///
/// Default to a real disk. /tmp is tmpfs on most systems, where fsync is a
/// no-op and every durability number is meaningless.
fn default_data_dir() -> PathBuf {
    let mut p = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    p.push(".cache/adabt-bench");
    p
}

/// Create `data_dir`, warning when it is memory-backed and so cannot measure durability.
fn prepare_data_dir(data_dir: &Path) {
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        eprintln!("could not create {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    if resources::is_memory_backed(data_dir) {
        eprintln!(
            "WARNING: {} is on {} (memory-backed). fsync does not reach a disk there,\n\
             so durability numbers from this run are not real. Pass --data-dir <path on disk>.",
            data_dir.display(),
            resources::filesystem_type(data_dir).unwrap_or_else(|| "?".into())
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let opts = Options::parse(&args);
    if opts.engine == "heap" || cmd == "query-matrix" {
        prepare_data_dir(&opts.data_dir);
    }

    match cmd {
        "soak" => commands::soak(&opts, &args),
        "list" => commands::list(),
        "run" => commands::run(&opts, &args),
        "matrix" => commands::matrix(&opts, &args),
        "query-matrix" => commands::query_matrix(&opts, &args),
        "scale" => commands::scale(&opts, &args),
        "fetch-profile" => commands::fetch_profile(&args),
        "record-repr" => commands::record_repr(&args),
        "index-scale" => commands::index_scale(&args),
        "compiled" => commands::compiled(&opts),
        _ => usage(),
    }
}
