//! The server binary.
//!
//! Three steps, in an order that matters: parse the command line into a
//! [`Config`] (rejecting anything malformed), validate it, and only then
//! touch the disk or the network. A bad invocation exits with status 2 and
//! leaves nothing behind; a runtime failure exits with status 1.
//!
//! # Posture: encrypt and authenticate for anything but a trusted network
//!
//! Token auth (`--auth-token`, roles via `--read-token`) stops strangers
//! from *using* the server. TLS (`--tls-cert` / `--tls-key`) stops them from
//! reading it in transit. Neither substitutes for the other — a token over
//! plaintext is a password on an open wire; TLS without tokens lets anyone
//! with a connection issue writes. Bind to a trusted network only when
//! running with neither.

use adabt_core::policy::{Mode, Policy};
use adabt_engine::sharded::ShardedDatabase;
use adabt_opt::levels::MAX_LEVEL;
use adabt_server::server::{Server, DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_CONNECTIONS};
use std::fmt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

const USAGE: &str = "\
adabt-server — serve an aDaBt database over TCP

usage: adabt-server --data <dir> [options]

  --data <dir>               where the database lives (created if absent)
  --listen <addr>            address to bind, default 127.0.0.1:7432
  --level <0-11>             manual optimization level, default 3
  --adaptive                 let the database optimize itself instead of using a level
  --shards <n>               partitions, each a complete engine with its own lock, default 1
  --max-connections <n>      refuse a connection beyond this many open, default 1024
  --idle-timeout-secs <n>    close a connection idle this long, default 300; 0 disables it
  --slow-query-log-ms <n>    log (to stderr) any query taking at least this long
  --auth-token <token>       require this bearer token (or set ADABT_TOKEN)
  --read-token <token>       also accept this read-only token; requires an auth token
  --tls-cert <pem>           certificate chain (PEM); requires --tls-key
  --tls-key <pem>            private key (PEM) for --tls-cert
  --version, -h/--help

One thread per connection and no lock around the engine: requests contend only
when they want the same partition. `--shards 1` is the unpartitioned behaviour.

Token auth and TLS are independent layers — use both outside a trusted network.

SIGINT/SIGTERM (Unix): stop accepting new connections, let connections already
open finish, checkpoint, exit.
";

const DEFAULT_LISTEN: &str = "127.0.0.1:7432";
const DEFAULT_LEVEL: u8 = 3;

/// Everything the binary needs to run, fully validated.
#[derive(Debug, PartialEq)]
struct Config {
    data: PathBuf,
    listen: String,
    optimization: Optimization,
    shards: usize,
    max_connections: usize,
    idle_timeout: Option<Duration>,
    slow_query_log: Option<Duration>,
    auth: Option<Auth>,
    tls: Option<Tls>,
}

#[derive(Debug, PartialEq)]
enum Optimization {
    Level(u8),
    Adaptive,
}

/// Credentials. A read-only token exists only alongside a full one: a
/// read-only credential on an open listener locks a door already standing
/// open, so that combination is unrepresentable here.
#[derive(Debug, PartialEq)]
struct Auth {
    token: String,
    read_token: Option<String>,
}

/// Certificate and key always travel together; half-encrypted is worse than down.
#[derive(Debug, PartialEq)]
struct Tls {
    cert: PathBuf,
    key: PathBuf,
}

/// What the command line asked for.
#[derive(Debug, PartialEq)]
enum Invocation {
    Serve(Config),
    Help,
    Version,
}

/// A command-line mistake, reported with the usage text and exit status 2.
#[derive(Debug, PartialEq)]
struct UsageError(String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse arguments (without the program name). `env_token` is the value of
/// `ADABT_TOKEN`, passed in so parsing stays a pure function.
fn parse_args(
    args: impl IntoIterator<Item = String>,
    env_token: Option<String>,
) -> Result<Invocation, UsageError> {
    let mut data = None;
    let mut listen = DEFAULT_LISTEN.to_string();
    let mut level = DEFAULT_LEVEL;
    let mut adaptive = false;
    let mut shards = 1;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS;
    let mut idle_timeout = Some(DEFAULT_IDLE_TIMEOUT);
    let mut slow_query_log = None;
    let mut auth_token = None;
    let mut read_token = None;
    let mut tls_cert = None;
    let mut tls_key = None;

    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| UsageError(format!("{flag} requires a value")))
        };
        match flag.as_str() {
            "--data" => data = Some(PathBuf::from(value()?)),
            "--listen" => listen = value()?,
            "--level" => level = number(&flag, value()?)?,
            "--adaptive" => adaptive = true,
            "--shards" => shards = number(&flag, value()?)?,
            "--max-connections" => max_connections = number(&flag, value()?)?,
            "--idle-timeout-secs" => idle_timeout = seconds(number(&flag, value()?)?),
            "--slow-query-log-ms" => {
                slow_query_log = Some(Duration::from_millis(number(&flag, value()?)?))
            }
            "--auth-token" => auth_token = Some(value()?),
            "--read-token" => read_token = Some(value()?),
            "--tls-cert" => tls_cert = Some(PathBuf::from(value()?)),
            "--tls-key" => tls_key = Some(PathBuf::from(value()?)),
            "--version" => return Ok(Invocation::Version),
            "-h" | "--help" => return Ok(Invocation::Help),
            other => return Err(UsageError(format!("unknown argument {other}"))),
        }
    }

    let data = data.ok_or_else(|| UsageError("--data is required".into()))?;
    if level > MAX_LEVEL {
        return Err(UsageError(format!("--level must be 0-{MAX_LEVEL}")));
    }
    if shards == 0 {
        return Err(UsageError("--shards must be at least 1".into()));
    }

    // The environment is the better home for a token launched by a
    // scheduler: process listings show arguments, not environment variables.
    let auth = match (
        auth_token.or(env_token.filter(|t| !t.is_empty())),
        read_token,
    ) {
        (Some(token), read_token) => Some(Auth { token, read_token }),
        (None, None) => None,
        (None, Some(_)) => {
            return Err(UsageError(
                "--read-token requires --auth-token (or ADABT_TOKEN)".into(),
            ))
        }
    };
    let tls = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => Some(Tls { cert, key }),
        (None, None) => None,
        _ => {
            return Err(UsageError(
                "--tls-cert and --tls-key must be given together".into(),
            ))
        }
    };

    Ok(Invocation::Serve(Config {
        data,
        listen,
        optimization: if adaptive {
            Optimization::Adaptive
        } else {
            Optimization::Level(level)
        },
        shards,
        max_connections,
        idle_timeout,
        slow_query_log,
        auth,
        tls,
    }))
}

fn number<T: FromStr>(flag: &str, value: String) -> Result<T, UsageError> {
    value
        .parse()
        .map_err(|_| UsageError(format!("{flag} expects a number, got {value:?}")))
}

/// `0` means "never".
fn seconds(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

impl Config {
    fn policy(&self) -> Policy {
        match self.optimization {
            Optimization::Level(level) => Policy::manual(level),
            Optimization::Adaptive => Policy {
                mode: Mode::Adaptive,
                ..Policy::conventional()
            },
        }
    }
}

/// Open, bind and serve until a shutdown signal; then checkpoint.
fn serve(config: Config) -> Result<(), String> {
    let db = ShardedDatabase::open(&config.data, config.shards, config.policy())
        .map_err(|e| format!("could not open {}: {e}", config.data.display()))?;
    if let Some(threshold) = config.slow_query_log {
        // Each shard's `Database` owns its own sink.
        for i in 0..db.shard_count() {
            db.shard(i)
                .expect("index below shard_count")
                .lock()
                .expect("shard lock poisoned before serving")
                .enable_slow_query_log(threshold);
        }
    }

    let mut server = Server::bind(&config.listen, db)
        .map_err(|e| format!("could not bind {}: {e}", config.listen))?
        .with_max_connections(config.max_connections)
        .with_idle_timeout(config.idle_timeout);
    if let Some(auth) = &config.auth {
        server = server.with_auth_token(&auth.token);
        if let Some(read_token) = &auth.read_token {
            server = server.with_read_token(read_token);
        }
    }
    if let Some(tls) = &config.tls {
        server = server
            .with_tls(&tls.cert, &tls.key)
            .map_err(|e| format!("could not configure TLS: {e}"))?;
    }

    announce(&config, &server);

    // Taken before `serve` consumes the server.
    let db = server.database();
    #[cfg(unix)]
    signal::stop_on_shutdown_request(server.stopper());

    server.serve();

    eprintln!("aDaBt: checkpointing before exit...");
    db.checkpoint()
        .map_err(|e| format!("checkpoint on shutdown failed: {e}"))
}

fn announce(config: &Config, server: &Server) {
    let mode = match config.optimization {
        Optimization::Level(level) => format!("level {level}"),
        Optimization::Adaptive => "adaptive".into(),
    };
    let auth = match config.auth {
        Some(_) => "token auth required",
        None => "trusted network only, NO AUTH",
    };
    let encryption = match config.tls {
        Some(_) => "TLS",
        None => "no encryption",
    };
    let addr = server
        .local_addr()
        .map_or_else(|_| config.listen.clone(), |a| a.to_string());
    eprintln!(
        "aDaBt listening on {addr} ({mode}, {} shard(s)) — {auth}, {encryption}",
        config.shards
    );
    // The residency ceiling is a property, not a defect; an operator who
    // learns it from a crash dump learned it too late. The measured figure
    // and its revisit triggers are in docs/benchmarks/scale-decision.md.
    eprintln!(
        "datasets are held resident: ~470 bytes of RAM per record — plan for roughly 2M records per GB"
    );
}

fn main() -> ExitCode {
    let env_token = std::env::var("ADABT_TOKEN").ok();
    match parse_args(std::env::args().skip(1), env_token) {
        Ok(Invocation::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Invocation::Version) => {
            println!("adabt-server {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Invocation::Serve(config)) => match serve(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("aDaBt: {message}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Minimal, dependency-free `SIGINT`/`SIGTERM` handling.
///
/// Two `signal(2)` calls do not justify a dependency. `signal(2)` and the two
/// numbers below are POSIX, which is the only portability claim this module
/// makes; on Windows the server has no graceful-shutdown path yet.
#[cfg(unix)]
mod signal {
    use adabt_server::server::Stopper;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }

    /// The only thing a signal handler here may do: an atomic store.
    /// Stopping, draining and checkpointing are not async-signal-safe, so
    /// they happen on an ordinary thread that polls the flag.
    extern "C" fn on_signal(_signum: i32) {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    /// Install the handlers and stop `stopper` once a signal arrives.
    pub fn stop_on_shutdown_request(stopper: Stopper) {
        // SAFETY: `on_signal` is async-signal-safe (a single atomic store),
        // and SIGINT/SIGTERM are valid signal numbers on every POSIX system.
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }
        std::thread::spawn(move || {
            while !REQUESTED.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            eprintln!("aDaBt: shutdown requested, draining connections...");
            stopper.stop();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, UsageError> {
        parse_args(args.iter().map(|s| s.to_string()), None)
    }

    fn config(args: &[&str]) -> Config {
        match parse(args) {
            Ok(Invocation::Serve(c)) => c,
            other => panic!("expected a config, got {other:?}"),
        }
    }

    fn error(args: &[&str]) -> String {
        parse(args).expect_err("expected a usage error").0
    }

    #[test]
    fn defaults_need_only_a_data_directory() {
        assert_eq!(
            config(&["--data", "d"]),
            Config {
                data: "d".into(),
                listen: DEFAULT_LISTEN.into(),
                optimization: Optimization::Level(DEFAULT_LEVEL),
                shards: 1,
                max_connections: DEFAULT_MAX_CONNECTIONS,
                idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
                slow_query_log: None,
                auth: None,
                tls: None,
            }
        );
    }

    #[test]
    fn malformed_numbers_are_refused_not_defaulted() {
        assert!(error(&["--data", "d", "--level", "abc"]).contains("--level"));
        assert!(error(&["--data", "d", "--shards", "x"]).contains("--shards"));
        assert!(error(&["--data", "d", "--max-connections", "-1"]).contains("--max-connections"));
    }

    #[test]
    fn out_of_range_values_are_refused() {
        assert!(error(&["--data", "d", "--level", "12"]).contains("0-11"));
        assert!(error(&["--data", "d", "--shards", "0"]).contains("at least 1"));
    }

    #[test]
    fn a_flag_missing_its_value_is_refused() {
        assert_eq!(error(&["--data"]), "--data requires a value");
        assert_eq!(
            error(&["--data", "d", "--auth-token", ""]),
            "--auth-token requires a value"
        );
    }

    #[test]
    fn data_is_required_and_unknown_flags_are_refused() {
        assert_eq!(error(&[]), "--data is required");
        assert_eq!(
            error(&["--data", "d", "--port", "1"]),
            "unknown argument --port"
        );
    }

    #[test]
    fn zero_idle_timeout_disables_it() {
        assert_eq!(
            config(&["--data", "d", "--idle-timeout-secs", "0"]).idle_timeout,
            None
        );
    }

    #[test]
    fn tls_files_come_as_a_pair() {
        assert!(error(&["--data", "d", "--tls-cert", "c"]).contains("together"));
        assert!(error(&["--data", "d", "--tls-key", "k"]).contains("together"));
        assert_eq!(
            config(&["--data", "d", "--tls-cert", "c", "--tls-key", "k"]).tls,
            Some(Tls {
                cert: "c".into(),
                key: "k".into()
            })
        );
    }

    #[test]
    fn a_read_token_needs_a_full_token_from_flag_or_environment() {
        assert!(error(&["--data", "d", "--read-token", "r"]).contains("--auth-token"));
        let from_env = parse_args(
            ["--data", "d", "--read-token", "r"].map(String::from),
            Some("t".into()),
        );
        let Ok(Invocation::Serve(c)) = from_env else {
            panic!("{from_env:?}")
        };
        assert_eq!(
            c.auth,
            Some(Auth {
                token: "t".into(),
                read_token: Some("r".into())
            })
        );
    }

    #[test]
    fn the_flag_token_wins_over_the_environment_and_empty_env_is_ignored() {
        let args = ["--data", "d", "--auth-token", "flag"].map(String::from);
        let Ok(Invocation::Serve(c)) = parse_args(args, Some("env".into())) else {
            panic!()
        };
        assert_eq!(c.auth.unwrap().token, "flag");
        let Ok(Invocation::Serve(c)) =
            parse_args(["--data", "d"].map(String::from), Some(String::new()))
        else {
            panic!()
        };
        assert_eq!(c.auth, None);
    }

    #[test]
    fn adaptive_overrides_the_level() {
        let c = config(&["--data", "d", "--level", "5", "--adaptive"]);
        assert_eq!(c.optimization, Optimization::Adaptive);
        assert!(matches!(c.policy().mode, Mode::Adaptive));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--version"]), Ok(Invocation::Version));
    }
}
