//! The io_uring gate: does thread-per-connection saturate before it matters?
//!
//! The substrate decision in the roadmap says the event-loop rewrite earns
//! its risk only when a connection-count bench shows saturation. This bench
//! is that measurement. Run it explicitly:
//!
//! ```text
//! cargo test -p adabt-server --test connection_scale --release -- --ignored --nocapture
//! ```
//!
//! It opens N concurrent clients (1 → 512), each hammering ping round-trips
//! for a fixed slice, and prints requests/second per rung. Read the curve:
//! flat-to-modest decline means the current substrate holds; a cliff between
//! two rungs names the connection count at which an event loop would start
//! paying for itself. Until that cliff exists in someone's real deployment,
//! `io_uring` stays a documented refusal.

use adabt_core::policy::Policy;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::{Client, Server};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Tmp(PathBuf);
impl Tmp {
    fn new() -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-conn-scale-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rung(conns: usize, slice: Duration) -> f64 {
    let tmp = Tmp::new();
    let db = ShardedDatabase::open(tmp.path(), 2, Policy::manual(4)).unwrap();
    let server = Server::bind("127.0.0.1:0", db).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || server.serve());

    // Warm one connection to confirm liveness before timing anything.
    Client::connect(addr).unwrap().ping().unwrap();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = Vec::with_capacity(conns);
    let start = Instant::now();
    for _ in 0..conns {
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut client = Client::connect(addr).unwrap();
            let mut n: u64 = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                client.ping().unwrap();
                n += 1;
            }
            n
        }));
    }
    std::thread::sleep(slice);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let secs = start.elapsed().as_secs_f64();
    total as f64 / secs.max(0.001)
}

#[test]
#[ignore = "a load measurement, not a correctness check — run with --ignored"]
fn connection_scale_curve() {
    println!("{:>8} {:>14} {:>12}", "conns", "req/s", "per-conn");
    let mut prev = 0.0;
    for conns in [1, 4, 16, 64, 128, 256, 512] {
        let rps = rung(conns, Duration::from_millis(1_500));
        println!("{:>8} {:>14.0} {:>12.0}", conns, rps, rps / conns as f64);
        // The saturation signal, stated rather than eyeballed: aggregate
        // throughput should not collapse as connections pile on. A drop of
        // more than half from one rung to the next names the cliff.
        if prev > 0.0 && conns > 4 && rps < prev * 0.5 {
            panic!(
                "throughput collapsed from {prev:.0} to {rps:.0} req/s at {conns} \
                 connections — this is the cliff where an event loop starts paying"
            );
        }
        prev = rps;
    }
}
