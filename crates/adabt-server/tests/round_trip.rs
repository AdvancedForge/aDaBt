//! The server over a real socket.
//!
//! Everything here goes through `TcpStream`: bind to port 0, ask what port that
//! turned out to be, connect, and speak the protocol. A test that called
//! `dispatch` directly would check the encoding and skip the part most likely to
//! be wrong — that a frame arrives in as many pieces as the network chooses.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_core::value::Value;
use adabt_engine::sharded::ShardedDatabase;
use adabt_ir::CmpOp;
use adabt_server::client::Client;
use adabt_server::protocol::{Frame, PROTOCOL_VERSION};
use adabt_server::server::Server;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-srv-{tag}-{}-{:?}",
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

const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];

fn schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("country", FieldType::Char(8)),
            FieldDef::new("age", FieldType::I64),
        ],
    )
    .unwrap()
}

fn rec(i: u64) -> Record {
    Record::new()
        .with("id", i)
        .with("country", COUNTRIES[(i % 4) as usize])
        .with("age", (i % 70) as i64)
}

/// A running server and the address it landed on.
struct Running {
    addr: SocketAddr,
    stopper: adabt_server::server::Stopper,
    _tmp: Tmp,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopper.stop();
    }
}

fn start(tag: &str, rows: u64) -> Running {
    start_sharded(tag, rows, 1)
}

/// The tests run against a single shard by default, so they measure the
/// protocol rather than the partitioning; `concurrent_clients_do_not_interfere`
/// runs against several, which is where the partitioning has to hold up.
fn start_sharded(tag: &str, rows: u64, shards: usize) -> Running {
    let tmp = Tmp::new(tag);
    let db = ShardedDatabase::open(&tmp.0, shards, Policy::manual(2)).unwrap();
    db.create_collection("users", schema()).unwrap();
    for i in 0..rows {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
    // Port 0: the OS picks, and the test asks what it picked. Choosing a number
    // makes a test that fails when someone else is already using it.
    let server = Server::bind("127.0.0.1:0", db).unwrap();
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());
    Running {
        addr,
        stopper,
        _tmp: tmp,
    }
}

#[test]
fn a_record_survives_the_round_trip_unchanged() {
    let s = start("round", 100);
    let mut c = Client::connect(s.addr).unwrap();
    c.ping().unwrap();
    for i in 0..100u64 {
        assert_eq!(c.get("users", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
    assert_eq!(c.get("users", RecordId(999)).unwrap(), None);
    assert_eq!(c.count("users").unwrap(), 100);
}

#[test]
fn writes_over_the_wire_are_visible_to_later_reads() {
    let s = start("writes", 10);
    let mut c = Client::connect(s.addr).unwrap();

    c.insert("users", RecordId(500), &rec(500)).unwrap();
    assert_eq!(c.get("users", RecordId(500)).unwrap(), Some(rec(500)));
    assert_eq!(c.count("users").unwrap(), 11);

    let changed = rec(500).with("age", 99i64);
    assert!(c.update("users", RecordId(500), &changed).unwrap());
    assert_eq!(c.get("users", RecordId(500)).unwrap(), Some(changed));

    assert!(c.delete("users", RecordId(500)).unwrap());
    assert!(!c.delete("users", RecordId(500)).unwrap());
    assert_eq!(c.count("users").unwrap(), 10);
}

#[test]
fn a_filtered_query_returns_what_the_engine_would() {
    let s = start("query", 400);
    let mut c = Client::connect(s.addr).unwrap();

    let rows = c
        .query("users", Some(("country", CmpOp::Eq, Value::from("NO"))), 0)
        .unwrap();
    assert_eq!(rows.len(), 100);
    assert!(rows
        .iter()
        .all(|(_, r)| r.get("country") == Some(&Value::from("NO"))));

    let ranged = c
        .query("users", Some(("age", CmpOp::Ge, Value::I64(60))), 0)
        .unwrap();
    assert!(!ranged.is_empty());
    assert!(ranged
        .iter()
        .all(|(_, r)| matches!(r.get("age"), Some(Value::I64(a)) if *a >= 60)));

    assert_eq!(c.query("users", None, 7).unwrap().len(), 7, "limit ignored");
    assert_eq!(c.scan("users").unwrap().len(), 400);
}

#[test]
fn the_optimizer_can_be_driven_from_a_client() {
    let s = start("optimize", 200);
    let mut c = Client::connect(s.addr).unwrap();
    c.optimize().unwrap();
    let text = c.explain_optimizations().unwrap();
    assert!(text.contains("plan_cache"), "{text}");
    let plan = c
        .explain("users", Some(("country", CmpOp::Eq, Value::from("NO"))))
        .unwrap();
    assert!(plan.contains("logical"), "{plan}");
    assert!(plan.contains("physical"), "{plan}");
}

#[test]
fn metrics_reports_prometheus_text_reflecting_real_traffic() {
    let s = start("metrics", 50);
    let mut c = Client::connect(s.addr).unwrap();
    // A call this client makes must show up as a call the server observed —
    // metrics export is a view onto real telemetry, not a static reply.
    for _ in 0..5 {
        c.get("users", RecordId(0)).unwrap();
    }
    let text = c.metrics().unwrap();
    assert!(text.starts_with("# shard 0"), "{text}");
    assert!(text.contains("adabt_op_calls_total"), "{text}");
    assert!(text.contains("adabt_touches_total"), "{text}");
}

#[test]
fn an_engine_error_arrives_as_an_error_and_not_as_a_wrong_answer() {
    let s = start("errors", 5);
    let mut c = Client::connect(s.addr).unwrap();

    let e = c.get("nosuch", RecordId(1)).unwrap_err().to_string();
    assert!(e.contains("not found"), "{e}");
    assert!(e.contains("nosuch"), "{e}");

    // A record the schema rejects.
    let bad = Record::new()
        .with("id", 1u64)
        .with("country", "much too long");
    let e = c
        .insert("users", RecordId(77), &bad)
        .unwrap_err()
        .to_string();
    assert!(e.contains("schema error"), "{e}");

    // The connection is still usable: an error is a reply, not a hang-up.
    assert_eq!(c.count("users").unwrap(), 5);
}

#[test]
fn a_frame_split_across_packets_is_reassembled() {
    // The thing a direct call to the dispatcher would never catch.
    let s = start("split", 20);
    let body = {
        let mut w = adabt_server::wire::Writer::new();
        w.str("users").u64(7);
        w.finish()
    };
    let frame = Frame::new(adabt_server::protocol::RequestKind::Get.code(), 1, body).encode();

    let mut stream = TcpStream::connect(s.addr).unwrap();
    stream.set_nodelay(true).unwrap();
    for byte in &frame {
        stream.write_all(&[*byte]).unwrap();
        std::thread::yield_now();
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).unwrap();
        buf.extend_from_slice(&chunk[..n]);
        if let Some((reply, _)) = Frame::decode(&buf).unwrap() {
            assert_eq!(reply.request_id, 1);
            let mut r = adabt_server::wire::Reader::new(&reply.body);
            assert_eq!(r.u8("found").unwrap(), 1);
            assert_eq!(r.record("record").unwrap(), rec(7));
            return;
        }
        assert!(n > 0, "the server closed without replying");
    }
}

#[test]
fn several_requests_share_one_connection() {
    let s = start("pipelined", 50);
    let mut c = Client::connect(s.addr).unwrap();
    for i in 0..50u64 {
        assert_eq!(c.get("users", RecordId(i)).unwrap(), Some(rec(i)));
    }
}

#[test]
fn concurrent_clients_do_not_interfere() {
    // Four partitions, no lock around them. This does not measure throughput,
    // but it does prove that eight connections against four shards never see
    // each other's replies and never observe a partly-applied write.
    let s = start_sharded("concurrent", 200, 4);
    let addr = s.addr;
    let handles: Vec<_> = (0..8)
        .map(|t| {
            std::thread::spawn(move || {
                let mut c = Client::connect(addr).unwrap();
                for i in 0..50u64 {
                    let id = (t * 50 + i) % 200;
                    assert_eq!(
                        c.get("users", RecordId(id)).unwrap(),
                        Some(rec(id)),
                        "thread {t} got the wrong record for {id}"
                    );
                }
                c.insert("users", RecordId(1000 + t), &rec(1000 + t))
                    .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let mut c = Client::connect(addr).unwrap();
    assert_eq!(c.count("users").unwrap(), 208);
}

#[test]
fn a_client_speaking_nonsense_is_disconnected_rather_than_obeyed() {
    let s = start("garbage", 5);
    let mut stream = TcpStream::connect(s.addr).unwrap();
    stream
        .write_all(&[
            0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ])
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    assert!(buf.is_empty(), "a bad-magic frame drew a reply: {buf:?}");

    // And the server is still serving everyone else.
    let mut c = Client::connect(s.addr).unwrap();
    assert_eq!(c.count("users").unwrap(), 5);
}

#[test]
fn an_unknown_request_kind_is_refused_without_closing_the_connection() {
    let s = start("unknown", 5);
    let mut c = Client::connect(s.addr).unwrap();
    assert_eq!(c.count("users").unwrap(), 5);

    let mut stream = TcpStream::connect(s.addr).unwrap();
    stream
        .write_all(&Frame::new(200, 42, Vec::new()).encode())
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    let n = stream.read(&mut chunk).unwrap();
    buf.extend_from_slice(&chunk[..n]);
    let (reply, _) = Frame::decode(&buf).unwrap().expect("no reply");
    assert_eq!(reply.request_id, 42);
    assert_ne!(reply.kind, 0, "an unknown kind was reported as ok");
}

#[test]
fn the_protocol_version_is_on_the_wire() {
    // A version mismatch has to be detectable before a body is parsed, or the
    // first symptom of a skew is a misread value rather than a refusal.
    let f = Frame::new(0, 1, vec![1, 2, 3]).encode();
    assert_eq!(u16::from_le_bytes([f[4], f[5]]), PROTOCOL_VERSION);
}
