//! The server binary.
//!
//! # Posture: trusted network only
//!
//! This server has no authentication, no authorization and no transport
//! encryption — every connection that can reach the listening port can read,
//! write and drop collections. That is not an oversight to be found later; it
//! is the honest state of a component whose auth story is a separate
//! milestone, and shipping a rate-limited-but-unauthenticated listener while
//! implying it is safe to expose would be worse than saying plainly what it
//! is: **bind this to a trusted network only** — a loopback address, a private
//! subnet behind its own perimeter, or a socket reachable only from processes
//! that already trust each other. Do not put this behind a public interface
//! without a proxy in front of it that adds the authentication this binary
//! does not have.

use adabt_core::policy::{Mode, Policy};
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::server::Server;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
adabt-server — serve an aDaBt database over TCP

usage: adabt-server --data <dir> [--listen <addr>] [--level <0-11>] [--adaptive]

  --data <dir>              where the database lives (created if absent)
  --listen <addr>            address to bind, default 127.0.0.1:7432
  --level <n>                manual optimization level, default 3
  --adaptive                 let the database optimize itself instead of using a level
  --shards <n>                partitions, each a complete engine with its own lock
  --max-connections <n>       refuse a connection beyond this many open, default 1024
  --idle-timeout-secs <n>     close a connection idle this long, default 300; 0 disables it
  --slow-query-log-ms <n>     log (to stderr) any query taking at least this long; unset by default

One thread per connection and no lock around the engine: requests contend only
when they want the same partition. This is shared-nothing partitioning, not
thread-per-core — there is no core pinning, no io_uring and no zero-copy path.
`--shards 1` is the unpartitioned behaviour exactly.

POSTURE: token auth available (--auth-token / ADABT_TOKEN) with roles
(--read-token issues a read-only credential); still no encryption. A token
stops strangers from reading; it does not stop them reading it in transit,
so bind to a trusted network regardless.

SIGINT/SIGTERM (Unix): stop accepting new connections, let connections already
open finish, checkpoint, exit.
";

fn main() {
    let mut data: Option<PathBuf> = None;
    let mut listen = "127.0.0.1:7432".to_string();
    let mut level: u8 = 3;
    let mut adaptive = false;
    let mut shards: usize = 1;
    let mut max_connections = adabt_server::server::DEFAULT_MAX_CONNECTIONS;
    let mut idle_timeout = Some(adabt_server::server::DEFAULT_IDLE_TIMEOUT);
    let mut slow_query_log_ms: Option<u64> = None;
    let mut auth_token: Option<String> = None;
    let mut read_token: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data" => data = args.next().map(PathBuf::from),
            "--listen" => {
                if let Some(v) = args.next() {
                    listen = v;
                }
            }
            "--level" => level = args.next().and_then(|v| v.parse().ok()).unwrap_or(level),
            "--adaptive" => adaptive = true,
            "--shards" => shards = args.next().and_then(|v| v.parse().ok()).unwrap_or(shards),
            "--max-connections" => {
                max_connections = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(max_connections)
            }
            "--idle-timeout-secs" => {
                let secs: u64 = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(idle_timeout.map(|d| d.as_secs()).unwrap_or(0));
                idle_timeout = if secs == 0 {
                    None
                } else {
                    Some(Duration::from_secs(secs))
                };
            }
            "--slow-query-log-ms" => {
                slow_query_log_ms = args.next().and_then(|v| v.parse().ok());
            }
            "--auth-token" => match args.next() {
                Some(t) if !t.is_empty() => auth_token = Some(t),
                _ => {
                    eprintln!("--auth-token requires a non-empty value\n\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--read-token" => match args.next() {
                Some(t) if !t.is_empty() => read_token = Some(t),
                _ => {
                    eprintln!("--read-token requires a non-empty value\n\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("unknown argument {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let Some(data) = data else {
        eprintln!("--data is required\n\n{USAGE}");
        std::process::exit(2);
    };

    let policy = if adaptive {
        Policy {
            mode: Mode::Adaptive,
            ..Policy::conventional()
        }
    } else {
        Policy::manual(level)
    };

    let db = match ShardedDatabase::open(&data, shards, policy) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("could not open {}: {e}", data.display());
            std::process::exit(1);
        }
    };
    if let Some(ms) = slow_query_log_ms {
        // Per shard, since each shard's `Database` owns its own sink — the
        // same place `ShardedDatabase::shard` already lets a caller reach in
        // for anything a shard decides on its own.
        for i in 0..db.shard_count() {
            db.shard(i)
                .unwrap()
                .lock()
                .unwrap()
                .enable_slow_query_log(Duration::from_millis(ms));
        }
    }
    // A token can come from a flag or the environment. The environment is
    // the better default for anything launched by a scheduler: process
    // listings show arguments, but not env vars.
    let auth_token = auth_token
        .or_else(|| std::env::var("ADABT_TOKEN").ok())
        .filter(|t| !t.is_empty());
    let server = match Server::bind(&listen, db) {
        Ok(s) => {
            let s = s
                .with_max_connections(max_connections)
                .with_idle_timeout(idle_timeout);
            let s = match auth_token.as_deref() {
                Some(t) => s.with_auth_token(t),
                None => s,
            };
            if read_token.is_some() && auth_token.is_none() {
                eprintln!("--read-token requires --auth-token: a read-only credential alongside an open listener locks a door that is already standing open");
                std::process::exit(2);
            }
            match read_token.as_deref() {
                Some(t) => s.with_read_token(t),
                None => s,
            }
        }
        Err(e) => {
            eprintln!("could not bind {listen}: {e}");
            std::process::exit(1);
        }
    };
    match server.local_addr() {
        Ok(addr) => eprintln!(
            "aDaBt listening on {addr} ({}, {shards} shard(s)) — {}, no encryption",
            if adaptive {
                "adaptive".to_string()
            } else {
                format!("level {level}")
            },
            if auth_token.is_some() {
                "token auth required (read-only tokens: --read-token)"
            } else {
                "trusted network only, NO AUTH"
            }
        ),
        Err(_) => eprintln!("aDaBt listening"),
    }
    // The residency ceiling is a property, not a defect; an operator who
    // learns it from a crash dump learned it too late. The measured figure
    // and its revisit triggers are in docs/scale-decision.md.
    eprintln!(
        "datasets are held resident: ~470 bytes of RAM per record — plan for roughly 2M records per GB"
    );

    // Retrieved before `serve` takes ownership of `server` — the same reason
    // `stopper()` and `database()` both take `&self`.
    let shared_db = server.database();

    #[cfg(unix)]
    {
        signal::install();
        let stopper = server.stopper();
        std::thread::spawn(move || loop {
            if signal::shutdown_requested() {
                eprintln!("aDaBt: shutdown requested, draining connections...");
                stopper.stop();
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        });
    }

    server.serve();

    eprintln!("aDaBt: checkpointing before exit...");
    if let Err(e) = shared_db.checkpoint() {
        eprintln!("aDaBt: checkpoint on shutdown failed: {e}");
    }
}

/// Minimal, dependency-free `SIGINT`/`SIGTERM` handling.
///
/// No crate for this on purpose — the workspace's one external dependency is
/// `thiserror`, and pulling in a signal-handling crate for two `signal(2)`
/// calls would be a poor trade for that property. `signal(2)` and the two
/// numbers below are POSIX, which is the only portability claim this module
/// makes; it does not build on Windows, where the server simply has no
/// graceful-shutdown path yet — the same behaviour it had before this
/// existed, not a regression on that platform.
#[cfg(unix)]
mod signal {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }

    /// The only thing a signal handler in this codebase is allowed to do: an
    /// atomic store. Everything that actually reacts to a shutdown request —
    /// stopping the accept loop, draining connections, checkpointing — runs
    /// on an ordinary thread that polls `shutdown_requested`, because none of
    /// that is async-signal-safe and a handler that did it directly would be
    /// one signal away from corrupting state mid-mutation.
    extern "C" fn on_signal(_signum: i32) {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    pub fn install() {
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }
    }

    pub fn shutdown_requested() -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }
}
