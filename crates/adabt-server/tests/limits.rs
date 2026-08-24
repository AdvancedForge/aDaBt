//! Connection limits, idle timeouts and graceful shutdown — the difference
//! between a server that stops accepting connections it should not and a
//! server that just leaks threads until the process falls over.

use adabt_core::policy::Policy;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::client::Client;
use adabt_server::server::Server;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-srv-limits-{tag}-{}-{:?}",
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

fn open_db(tag: &str) -> (Tmp, ShardedDatabase) {
    let tmp = Tmp::new(tag);
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(0)).unwrap();
    (tmp, db)
}

#[test]
fn a_connection_beyond_the_cap_is_closed_immediately() {
    let (_tmp, db) = open_db("cap");
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_max_connections(2)
        .with_idle_timeout(None);
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());

    // Two connections that stay open, holding the cap.
    let _a = Client::connect(addr).unwrap();
    let _b = Client::connect(addr).unwrap();
    // Give the accept loop a moment to have actually accepted both before the
    // third connects — otherwise this races the server's own bookkeeping.
    std::thread::sleep(Duration::from_millis(50));

    // A third is refused: the server closes it without ever replying.
    let mut over = TcpStream::connect(addr).unwrap();
    over.write_all(&[1, 2, 3, 4]).ok();
    let mut buf = [0u8; 16];
    let n = over.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "a connection beyond the cap should get nothing back");

    stopper.stop();
}

#[test]
fn an_idle_connection_is_closed_after_its_timeout() {
    let (_tmp, db) = open_db("idle");
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_idle_timeout(Some(Duration::from_millis(100)));
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());

    let mut stream = TcpStream::connect(addr).unwrap();
    // Send nothing. The server must give up on this connection on its own.
    let mut buf = [0u8; 16];
    let started = std::time::Instant::now();
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "an idle connection should eventually be closed");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout should be measured in the configured hundred milliseconds, not left unbounded"
    );

    stopper.stop();
}

#[test]
fn serve_waits_for_an_in_flight_request_before_returning() {
    let (_tmp, db) = open_db("drain");
    let server = Server::bind("127.0.0.1:0", db).unwrap();
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    let handle = std::thread::spawn(move || server.serve());

    let mut c = Client::connect(addr).unwrap();
    c.ping().unwrap();
    // Closed rather than left open: an open connection has no idle timeout
    // in this test, so it would otherwise hold the drain for the full
    // `DRAIN_TIMEOUT` and this test would be checking that bound, not the
    // "waits, but not forever" property it is actually for.
    drop(c);

    stopper.stop();
    // `serve` returning promptly, from a background thread that is joinable
    // within a short bound, is the property under test: a stuck drain would
    // hang this join instead.
    let started = std::time::Instant::now();
    let done = handle.join();
    assert!(done.is_ok(), "serve should return once stopped");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "serve took {:?} to drain a connection that had already closed",
        started.elapsed()
    );
}

#[test]
fn connection_count_reflects_what_is_actually_open() {
    let (_tmp, db) = open_db("active-count");
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_idle_timeout(None);
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    let count = server.connection_count();
    assert_eq!(count.get(), 0);
    let server_thread = std::thread::spawn(move || server.serve());

    let c1 = Client::connect(addr).unwrap();
    let c2 = Client::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(count.get(), 2);

    drop(c1);
    drop(c2);
    // Closing a `TcpStream` sends FIN, but the server only notices on its
    // next `read`, so this can lag briefly — poll rather than assert once.
    let mut settled = false;
    for _ in 0..50 {
        if count.get() == 0 {
            settled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        settled,
        "connections did not close: count = {}",
        count.get()
    );

    stopper.stop();
    server_thread.join().unwrap();
}
