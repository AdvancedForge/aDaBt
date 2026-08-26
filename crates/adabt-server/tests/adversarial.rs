//! The hostile-client suite.
//!
//! Everything a client can do wrong on the wire, done deliberately: garbage
//! bytes, impossible lengths, unknown request kinds, bodies that parse as
//! nothing at all, frames cut off mid-header. What each test proves is the
//! same three-part contract: the misbehaving connection ends up closed (or
//! refused) rather than half-served; the server does not crash, panic, or
//! wedge; and every *other* connection carries on as if nothing happened.
//! A protocol that only works when everyone follows it is not finished.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::client::Client;
use adabt_server::protocol::{Frame, StatusCode, PROTOCOL_VERSION};
use adabt_server::server::Server;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-adv-{tag}-{}-{:?}",
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
    stopper: adabt_server::server::Stopper,
    _tmp: Tmp,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopper.stop();
    }
}

fn start(tag: &str, rows: u64) -> Running {
    let tmp = Tmp::new(tag);
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(2)).unwrap();
    db.create_collection("users", adabt_core::schema::Schema::dynamic())
        .unwrap();
    for i in 0..rows {
        db.insert("users", RecordId(i), rec(i)).unwrap();
    }
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

fn rec(i: u64) -> Record {
    Record::new().with("id", i).with("n", (i as i64) * 3)
}

/// A raw connection with short timeouts: hostile traffic should never need
/// to wait long for its answer, and neither should a test that asserts it
/// gets none.
fn raw(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    s.set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    s
}

/// Read until EOF or timeout. Returns how many bytes arrived before then.
fn drain(mut stream: TcpStream) -> usize {
    let mut total = 0;
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return total,
            Ok(n) => total += n,
            Err(_) => return total,
        }
    }
}

/// The benign control: a full round trip through the real client. Run after
/// each act of vandalism — if this fails, the server did not survive.
fn still_serving(s: &Running, expected: u64) {
    let mut c = Client::connect(s.addr).unwrap();
    c.ping().unwrap();
    assert_eq!(c.count("users").unwrap(), expected);
}

#[test]
fn garbage_bytes_are_closed_and_the_server_carries_on() {
    let s = start("garbage", 10);

    // Pure noise, no framing anywhere in it.
    let mut g1 = raw(s.addr);
    g1.write_all(&[0xDE, 0xAD, 0xBE, 0xEF].repeat(64)).unwrap();
    assert_eq!(drain(g1), 0, "the server answered garbage with something");

    // Valid magic, then nonsense: same verdict.
    let mut g2 = raw(s.addr);
    let mut junk = Frame::new(0, 1, Vec::new()).encode()[..8].to_vec();
    junk.extend_from_slice(&[0x42; 64]);
    g2.write_all(&junk).unwrap();
    assert_eq!(drain(g2), 0);

    still_serving(&s, 10);
}

#[test]
fn an_impossible_length_is_refused_without_reading_a_gigabyte() {
    let s = start("huge", 10);
    // A well-formed header claiming a four-gigabyte body must be rejected on
    // the length field alone — before any body byte is awaited or buffered.
    let mut f = raw(s.addr);
    let mut header = Frame::new(0, 1, Vec::new()).encode();
    header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    f.write_all(&header).unwrap();
    assert_eq!(drain(f), 0, "a 4 GiB frame claim was not shut out");

    // One past the limit is equally unwelcome.
    let mut f2 = raw(s.addr);
    let mut header2 = Frame::new(0, 1, Vec::new()).encode();
    header2[16..20].copy_from_slice(&(u32::MAX - 3).to_le_bytes());
    f2.write_all(&header2).unwrap();
    assert_eq!(drain(f2), 0);

    still_serving(&s, 10);
}

#[test]
fn bad_magic_and_wrong_version_close_the_connection() {
    let s = start("framing", 10);

    // Wrong magic.
    let mut m = raw(s.addr);
    let mut h = Frame::new(0, 1, Vec::new()).encode();
    h[0] = 0x00;
    m.write_all(&h).unwrap();
    assert_eq!(drain(m), 0);

    // Wrong version.
    let mut v = raw(s.addr);
    let mut h2 = Frame::new(0, 1, Vec::new()).encode();
    h2[4..6].copy_from_slice(&999u16.to_le_bytes());
    v.write_all(&h2).unwrap();
    assert_eq!(drain(v), 0);

    still_serving(&s, 10);
}

#[test]
fn a_truncated_header_then_fin_ends_quietly() {
    let s = start("trunc", 10);
    let mut t = raw(s.addr);
    // Ten bytes of a twenty-byte header, then the writer's side goes away.
    t.write_all(&Frame::new(0, 1, Vec::new()).encode()[..10])
        .unwrap();
    t.shutdown(std::net::Shutdown::Write).unwrap();
    assert_eq!(drain(t), 0, "half a header earned no reply");
    still_serving(&s, 10);
}

#[test]
fn an_unknown_request_kind_earns_bad_request_and_the_connection_lives_on() {
    let s = start("kind", 10);
    let mut c = Client::connect(s.addr).unwrap();

    // Kind 200 is not in the protocol. The reply is an error frame carrying
    // the offending request's id — not silence, and not a closed socket.
    c.send_raw(Frame::new(200, 77, Vec::new())).unwrap();
    let reply = c.next_reply(77).unwrap();
    assert_eq!(reply.request_id, 77);
    assert_eq!(
        StatusCode::from_code(reply.kind),
        Some(StatusCode::BadRequest)
    );

    // The same connection continues to work afterwards.
    c.ping().unwrap();
    assert_eq!(c.count("users").unwrap(), 10);
}

#[test]
fn malformed_bodies_earn_error_replies_and_the_connection_survives() {
    let s = start("body", 10);
    let mut c = Client::connect(s.addr).unwrap();

    // Get with an empty body: no collection, no id.
    c.send_raw(Frame::new(
        adabt_server::protocol::RequestKind::Get.code(),
        101,
        Vec::new(),
    ))
    .unwrap();
    let reply = c.next_reply(101).unwrap();
    assert_ne!(
        StatusCode::from_code(reply.kind),
        Some(StatusCode::Ok),
        "an empty Get body was answered as success"
    );
    assert_eq!(reply.request_id, 101);

    // Insert whose body is a string where a record should be.
    use adabt_server::wire::Writer;
    let mut w = Writer::new();
    w.str("users");
    w.u64(50);
    w.str("this is not a record");
    c.send_raw(Frame::new(
        adabt_server::protocol::RequestKind::Insert.code(),
        102,
        w.finish(),
    ))
    .unwrap();
    let reply = c.next_reply(102).unwrap();
    assert_ne!(StatusCode::from_code(reply.kind), Some(StatusCode::Ok));

    // Nothing landed, and the connection is fine.
    assert_eq!(c.count("users").unwrap(), 10);
    assert_eq!(c.get("users", RecordId(50)).unwrap(), None);
    c.ping().unwrap();
}

#[test]
fn a_hostile_client_cannot_disturb_a_benign_one() {
    let s = start("concurrent", 100);

    // One thread hammering garbage while a real client works.
    let addr = s.addr;
    let spammer = std::thread::spawn(move || {
        for i in 0..50u32 {
            let Ok(mut st) = TcpStream::connect(addr) else {
                continue;
            };
            let _ = st.set_read_timeout(Some(std::time::Duration::from_secs(2)));
            let seed = i.to_le_bytes();
            let _ = st.write_all(&[seed[0]; 512]);
            let _ = drain(st);
        }
    });

    let mut c = Client::connect(s.addr).unwrap();
    for i in 0..100u64 {
        assert_eq!(c.get("users", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
    c.insert("users", RecordId(500), &rec(500)).unwrap();
    assert_eq!(c.count("users").unwrap(), 101);
    spammer.join().unwrap();

    still_serving(&s, 101);
}

#[test]
fn nonzero_flags_do_not_confuse_the_dispatcher() {
    // `flags` is carried but unused; a client setting it must get normal
    // service rather than undefined behaviour.
    let s = start("flags", 5);
    let mut stream = raw(s.addr);
    let mut f = Frame::new(
        adabt_server::protocol::RequestKind::Ping.code(),
        9,
        Vec::new(),
    )
    .encode();
    f[7] = 0xFF;
    stream.write_all(&f).unwrap();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some((reply, _)) = Frame::decode(&buf).unwrap() {
            assert_eq!(reply.request_id, 9);
            assert_eq!(
                StatusCode::from_code(reply.kind),
                Some(StatusCode::Ok),
                "unused flag bits changed the answer"
            );
            break;
        }
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed instead of answering");
        buf.extend_from_slice(&chunk[..n]);
    }
    let _ = PROTOCOL_VERSION;
}
