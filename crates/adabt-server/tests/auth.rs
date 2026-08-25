//! Authentication over a real socket.
//!
//! A server that requires a token must refuse everything else before it —
//! including Ping, because an unauthenticated liveness oracle is still an
//! oracle — and must remember success per connection rather than globally.
//! These tests speak the protocol the same way `round_trip.rs` does: real
//! sockets, real frames.

use adabt_core::error::Error;
use adabt_core::policy::Policy;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::client::Client;
use adabt_server::server::Server;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-auth-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Running {
    addr: SocketAddr,
    /// Held so the drop order stops the server before the directory goes
    /// away; never read directly, which is the point.
    _stopper: adabt_server::server::Stopper,
    _tmp: Tmp,
    // Keeps the drop order honest: the stopper must fire before the
    // directory goes away. Field exists purely for its position in the
    // struct; the flag is never read.
    _alive: Arc<AtomicBool>,
}

fn start(token: &str) -> Running {
    let tmp = Tmp::new("auth");
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(0)).unwrap();
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_auth_token(token);
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());
    Running {
        addr,
        _stopper: stopper,
        _tmp: tmp,
        _alive: Arc::new(AtomicBool::new(true)),
    }
}

const TOKEN: &str = "correct-horse-battery-staple";

#[test]
fn nothing_is_answered_before_authentication() {
    let run = start(TOKEN);
    let mut c = Client::connect(run.addr).unwrap();
    let err = c.ping().unwrap_err();
    match err {
        Error::Remote { status, .. } => {
            assert_eq!(status, "unauthorized", "{status}");
        }
        other => panic!("expected a remote refusal, got {other:?}"),
    }
}

#[test]
fn a_wrong_token_is_refused_and_the_connection_stays_locked() {
    let run = start(TOKEN);
    let mut c = Client::connect(run.addr).unwrap();

    let err = c.auth("wrong-token").unwrap_err();
    match err {
        Error::Remote { status, message } => {
            assert_eq!(status, "authentication failed", "{status}");
            // The token is not echoed back in the refusal.
            assert!(!message.contains("wrong-token"), "{message}");
        }
        other => panic!("expected auth denial, got {other:?}"),
    }

    // Still locked after the failure: a wrong guess does not open anything,
    // and neither does the failure itself close the connection — a typo
    // costs a retry, not a reconnect.
    let err2 = c.ping().unwrap_err();
    assert!(matches!(err2, Error::Remote { ref status, .. } if status == &"unauthorized"));
}

#[test]
fn the_right_token_unlocks_everything_for_that_connection_only() {
    let run = start(TOKEN);

    let mut c = Client::connect(run.addr).unwrap();
    c.auth(TOKEN).unwrap();
    // Authenticated now: Ping works.
    c.ping().unwrap();

    // And a second connection starts locked — one client's proof is not a
    // property of the server.
    let mut stranger = Client::connect(run.addr).unwrap();
    let err = stranger.ping().unwrap_err();
    assert!(matches!(err, Error::Remote { ref status, .. } if status == &"unauthorized"));

    // The first connection's session is unaffected by the stranger's
    // failure.
    c.ping().unwrap();
}

#[test]
fn a_server_without_a_token_answers_immediately() {
    let tmp = Tmp::new("noauth");
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(0)).unwrap();
    let server = Server::bind("127.0.0.1:0", db).unwrap();
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());

    let mut c = Client::connect(addr).unwrap();
    c.ping().unwrap();
    stopper.stop();
    drop(tmp);
}
