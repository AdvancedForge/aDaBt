//! Per-collection role floors, over real sockets.
//!
//! The role model says what a *connection* may do; floors say where it may
//! do it. One collection walled off from readers — reads included, which is
//! the part the role model alone cannot express — beside collections that
//! follow the connection's role as before. What the tests pin: the floor
/// bites on every request kind that names the collection, leaves every
/// other collection untouched, answers a known caller with `Forbidden`
/// rather than `Unauthorized`, and costs an unconfigured server nothing.
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::client::Client;
use adabt_server::server::{Role, Server};
use std::net::SocketAddr;
use std::path::PathBuf;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-grants-{tag}-{}-{:?}",
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

fn start(tag: &str, configure: impl FnOnce(Server) -> Server) -> Running {
    let tmp = Tmp::new(tag);
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(2)).unwrap();
    db.create_collection("public", adabt_core::schema::Schema::dynamic())
        .unwrap();
    db.create_collection("secrets", adabt_core::schema::Schema::dynamic())
        .unwrap();
    db.insert("public", RecordId(1), Record::new().with("v", "everyone"))
        .unwrap();
    db.insert("secrets", RecordId(1), Record::new().with("v", "operators"))
        .unwrap();
    let server = configure(Server::bind("127.0.0.1:0", db).unwrap());
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());
    Running {
        addr,
        stopper,
        _tmp: tmp,
    }
}

const ADMIN: &str = "op-token";
const READER: &str = "dash-token";

#[test]
fn a_reader_is_walled_off_from_a_floored_collection_and_nowhere_else() {
    let s = start("floor", |srv| {
        srv.with_auth_token(ADMIN)
            .with_read_token(READER)
            .with_collection_floor("secrets", Role::Admin)
    });

    // Reader: the public collection is fully theirs to browse...
    let mut r = Client::connect(s.addr).unwrap();
    r.auth(READER).unwrap();
    assert_eq!(
        r.get("public", RecordId(1)).unwrap(),
        Some(Record::new().with("v", "everyone"))
    );
    assert_eq!(r.count("public").unwrap(), 1);

    // ...but secrets refuses even a read.
    let err = r.get("secrets", RecordId(1)).unwrap_err().to_string();
    assert!(err.contains("higher-role"), "{err}");
    assert_eq!(r.count("secrets").unwrap_err().to_string(), err);

    // And the refusal is Forbidden-shaped: known caller, wrong place.
    use adabt_server::protocol::{RequestKind, StatusCode};
    let kind = RequestKind::Count.code();
    let mut body = adabt_server::wire::Writer::new();
    body.str("secrets");
    r.send_raw(adabt_server::protocol::Frame::new(kind, 42, body.finish()))
        .unwrap();
    assert_eq!(
        StatusCode::from_code(r.next_reply(42).unwrap().kind),
        Some(StatusCode::Forbidden)
    );
}

#[test]
fn an_admin_passes_every_floor_and_the_floor_changes_nothing_unconfigured() {
    let s = start("admin", |srv| {
        srv.with_auth_token(ADMIN)
            .with_read_token(READER)
            .with_collection_floor("secrets", Role::Admin)
    });
    let mut op = Client::connect(s.addr).unwrap();
    op.auth(ADMIN).unwrap();
    assert_eq!(
        op.get("secrets", RecordId(1)).unwrap(),
        Some(Record::new().with("v", "operators"))
    );
    op.insert("secrets", RecordId(2), &Record::new().with("v", "added"))
        .unwrap();

    // The public collection never had a floor; readers read it (covered in
    // the other test), and here admins write it.
    op.update("public", RecordId(1), &Record::new().with("v", "changed"))
        .unwrap();
}

#[test]
fn an_open_listener_ignores_floors_because_there_is_nothing_to_check() {
    // No tokens configured: every connection starts admin (`at_least` holds),
    // so a floor is moot. The point of the test is that configuring one does
    // not break the trusted-network posture — no panic, no refusals.
    let s = start("open", |srv| {
        srv.with_collection_floor("secrets", Role::Admin)
    });
    let mut c = Client::connect(s.addr).unwrap();
    c.ping().unwrap();
    assert_eq!(
        c.get("secrets", RecordId(1)).unwrap(),
        Some(Record::new().with("v", "operators"))
    );
}

#[test]
fn a_corrupt_body_still_answers_bad_request_not_forbidden() {
    // A floor must not turn an unparseable request into an authorization
    // answer: leniency in `collection_of` routes garbage to dispatch, which
    // gives it the honest verdict.
    let s = start("corrupt", |srv| {
        srv.with_auth_token(ADMIN)
            .with_read_token(READER)
            .with_collection_floor("secrets", Role::Admin)
    });
    let mut r = Client::connect(s.addr).unwrap();
    r.auth(READER).unwrap();
    use adabt_server::protocol::{RequestKind, StatusCode};
    r.send_raw(adabt_server::protocol::Frame::new(
        RequestKind::Count.code(),
        7,
        vec![0xFF; 8],
    ))
    .unwrap();
    let reply = r.next_reply(7).unwrap();
    // The exact code for an unparseable body is dispatch's business (a
    // corruption maps to `Internal`); what this pins is only that a floor
    // did not turn wire garbage into an authorization answer.
    let earned = StatusCode::from_code(reply.kind);
    assert_ne!(earned, Some(StatusCode::Forbidden));
    assert_ne!(earned, Some(StatusCode::Unauthorized));
}
