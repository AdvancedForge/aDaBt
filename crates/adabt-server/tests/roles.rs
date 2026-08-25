//! Roles at the socket level: what a read-only credential may and may not
//! do, and that the role belongs to the token, not the connection.
//!
//! `auth.rs` proves the authentication gate. This file proves the
//! authorization layer behind it: a second credential that authenticates but
//! buys less, and a refusal that names its kind honestly — `forbidden`, not
//! `unauthorized`, because telling a known caller to re-authenticate when no
//! credential can help is a lie.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::client::Client;
use adabt_server::server::Server;
use std::path::PathBuf;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("adabt-roles-{}-{:?}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const ADMIN: &str = "admin-token";
const READER: &str = "reader-token";

fn reader_client(addr: std::net::SocketAddr) -> Client {
    let mut c = Client::connect(addr).unwrap();
    c.auth(READER).unwrap();
    c
}

#[test]
fn a_reader_can_read_but_not_write() {
    let tmp = Tmp::new("rw");
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(0)).unwrap();
    db.create_collection("c", Schema::dynamic()).unwrap();
    db.insert("c", RecordId(1), Record::new().with("v", 1i64))
        .unwrap();
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_auth_token(ADMIN)
        .with_read_token(READER);
    let addr = server.local_addr().unwrap();
    let _stopper = server.stopper();
    std::thread::spawn(move || server.serve());

    let mut c = reader_client(addr);

    // Reads work.
    assert_eq!(c.count("c").unwrap(), 1, "a reader's count was refused");
    assert!(
        c.get("c", RecordId(1)).unwrap().is_some(),
        "a reader's get was refused"
    );

    // Writes are refused with Forbidden.
    match c.insert("c", RecordId(2), &Record::new().with("v", 2i64)) {
        Err(Error::Remote { status, message }) => {
            assert_eq!(status, "forbidden", "message was: {message}");
        }
        other => panic!("a reader's insert was not forbidden: {other:?}"),
    }
    match c.delete("c", RecordId(1)) {
        Err(Error::Remote { status, .. }) => assert_eq!(status, "forbidden"),
        other => panic!("a reader's delete was not forbidden: {other:?}"),
    }

    // The data is untouched by every refused request.
    assert_eq!(c.count("c").unwrap(), 1);
}

#[test]
fn the_admin_credential_keeps_full_access() {
    let tmp = Tmp::new("admin");
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(0)).unwrap();
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_auth_token(ADMIN)
        .with_read_token(READER);
    let addr = server.local_addr().unwrap();
    let _stopper = server.stopper();
    std::thread::spawn(move || server.serve());

    let mut c = Client::connect(addr).unwrap();
    c.auth(ADMIN).unwrap();
    c.optimize().expect("an admin's optimize was refused");
}

#[test]
fn an_unknown_role_request_is_still_just_unauthenticated() {
    // The read token authenticates; a wrong one does not. The two
    // credentials are checked independently — presenting neither, or both
    // wrong, earns Unauthorized exactly as with a single-token server.
    let tmp = Tmp::new("wrongtok");
    let db = ShardedDatabase::open(&tmp.0, 1, Policy::manual(0)).unwrap();
    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_auth_token(ADMIN)
        .with_read_token(READER);
    let addr = server.local_addr().unwrap();
    let _stopper = server.stopper();
    std::thread::spawn(move || server.serve());

    let mut c = Client::connect(addr).unwrap();
    match c.ping() {
        Err(e @ Error::Remote { status, .. }) => {
            assert_eq!(status, "unauthorized", "error was: {e}");
        }
        Ok(()) => panic!("ping answered before authentication"),
        Err(other) => panic!("unexpected error shape: {other}"),
    }
}
