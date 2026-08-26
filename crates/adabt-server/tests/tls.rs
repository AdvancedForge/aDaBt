//! TLS, end to end: a real handshake and real queries over the encrypted
//! stream.
//!
//! The server side is exercised through [`Server::with_tls`] with a freshly
//! generated self-signed certificate; the client side through rustls with
//! that same certificate pinned as its trust root — which is exactly how a
//! private deployment would pin it. What this proves is not that "rustls
//! works" but that *our* wiring does: the handshake happens before any
//! protocol byte, frames survive the encrypted stream intact, plaintext
//! against a TLS listener fails rather than half-speaking, and a listener
//! asked for TLS without both files refuses to come up.

use adabt_core::ids::RecordId;
use adabt_core::policy::Policy;
use adabt_core::record::Record;
use adabt_engine::sharded::ShardedDatabase;
use adabt_server::client::Client;
use adabt_server::protocol::{Frame, StatusCode};
use adabt_server::server::Server;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "adabt-tls-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running TLS server: its address, its stopper, its certificate (for the
/// client to pin), and the temp dir keeping the PEM files alive.
struct Running {
    addr: SocketAddr,
    stopper: adabt_server::server::Stopper,
    cert_pem: PathBuf,
    _tmp: Tmp,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopper.stop();
    }
}

fn start(tag: &str, rows: u64) -> Running {
    let tmp = Tmp::new(tag);

    // A fresh self-signed pair per run. `localhost` is the name the client
    // will claim to be connecting to; the SAN must match or rustls rightly
    // refuses.
    let certified_key =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("key generation");
    let cert_pem = tmp.path().join("cert.pem");
    let key_pem = tmp.path().join("key.pem");
    std::fs::write(&cert_pem, certified_key.cert.pem()).unwrap();
    std::fs::write(&key_pem, certified_key.signing_key.serialize_pem()).unwrap();

    let db = ShardedDatabase::open(tmp.path(), 1, Policy::manual(2)).unwrap();
    db.create_collection("users", adabt_core::schema::Schema::dynamic())
        .unwrap();
    for i in 0..rows {
        db.insert(
            "users",
            RecordId(i),
            Record::new().with("id", i).with("n", (i as i64) * 3),
        )
        .unwrap();
    }

    let server = Server::bind("127.0.0.1:0", db)
        .unwrap()
        .with_tls(&cert_pem, &key_pem)
        .expect("valid certificate and key");
    let addr = server.local_addr().unwrap();
    let stopper = server.stopper();
    std::thread::spawn(move || server.serve());
    Running {
        addr,
        stopper,
        cert_pem,
        _tmp: tmp,
    }
}

/// A protocol client over a TLS session whose only trusted root is the
/// server's own certificate — private-pinning, not a public CA.
fn tls_client(running: &Running) -> Client<impl std::io::Read + std::io::Write> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::ServerName;

    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_file_iter(&running.cert_pem)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for c in certs {
        roots.add(c).unwrap();
    }
    let config = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let name = ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(config, name).unwrap();
    let stream = TcpStream::connect(running.addr).unwrap();
    stream.set_nodelay(true).ok();
    Client::over_stream(rustls::StreamOwned::new(conn, stream))
}

#[test]
fn queries_and_writes_survive_the_encrypted_stream_unchanged() {
    let s = start("round", 50);
    let mut c = tls_client(&s);
    c.ping().unwrap();
    assert_eq!(c.count("users").unwrap(), 50);
    for i in [0u64, 17, 49] {
        assert_eq!(c.get("users", RecordId(i)).unwrap(), Some(rec(i)), "{i}");
    }
    assert_eq!(c.get("users", RecordId(999)).unwrap(), None);

    let extra = rec(500);
    c.insert("users", RecordId(500), &extra).unwrap();
    assert_eq!(c.get("users", RecordId(500)).unwrap(), Some(extra));
    assert_eq!(c.count("users").unwrap(), 51);
    assert!(c.delete("users", RecordId(500)).unwrap());
    assert_eq!(c.count("users").unwrap(), 50);
}

fn rec(i: u64) -> Record {
    Record::new().with("id", i).with("n", (i as i64) * 3)
}

#[test]
fn plaintext_against_a_tls_listener_is_refused_not_half_spoken() {
    let s = start("plain", 5);
    // A raw TCP connection speaking our frame protocol must get nowhere:
    // the server waits for a handshake the client never sends, so the ping
    // cannot be answered — and the read times out rather than returning an
    // HTTP-style plaintext error we would have to be careful about.
    let mut stream = TcpStream::connect(s.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let ping = Frame::new(
        adabt_server::protocol::RequestKind::Ping.code(),
        1,
        Vec::new(),
    );
    stream.write_all(&ping.encode()).unwrap();
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        // A TLS alert record: content type 0x15, one two-byte alert inside.
        // That is the handshake layer refusing, before any protocol code ran
        // — the correct shape of "this listener speaks TLS only".
        Ok(n) => {
            // A TLS alert record: content type 0x15, one two-byte alert
            // inside. That is the handshake layer refusing, before any
            // protocol code ran. Whatever the bytes parse as at our framing
            // layer, they must not constitute an answer to the ping.
            assert_eq!(buf[0], 0x15, "expected a TLS alert record, got {n} bytes");
            match adabt_server::protocol::Frame::decode(&buf[..n]) {
                Err(_) | Ok(None) => {}
                Ok(Some((frame, _))) => {
                    assert_ne!(
                        frame.request_id, 1,
                        "the alert was mistaken for a reply to the ping"
                    );
                    assert_ne!(
                        StatusCode::from_code(frame.kind),
                        Some(StatusCode::Ok),
                        "the alert carried an ok status"
                    );
                }
            }
        }
        Err(e) => panic!("the server closed without even an alert: {e}"),
    }
}

#[test]
fn a_certificate_without_a_key_is_a_startup_error() {
    let tmp = Tmp::new("half");
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = tmp.path().join("cert.pem");
    std::fs::write(&cert, ck.cert.pem()).unwrap();

    let db = ShardedDatabase::open(tmp.path(), 1, Policy::manual(2)).unwrap();
    let server = Server::bind("127.0.0.1:0", db).unwrap();
    let err = server
        .with_tls_flags(Some(cert.as_path()), None)
        .err()
        .expect("half a TLS configuration must be refused");
    assert!(err.to_string().contains("together"), "{err}");
}

#[test]
fn an_unreadable_certificate_fails_at_startup_with_the_file_named() {
    let tmp = Tmp::new("badfile");
    let missing = tmp.path().join("nope.pem");

    let db = ShardedDatabase::open(tmp.path(), 1, Policy::manual(2)).unwrap();
    let server = Server::bind("127.0.0.1:0", db).unwrap();
    let key = tmp.path().join("k.pem");
    let err = server
        .with_tls(missing.as_path(), key.as_path())
        .err()
        .expect("a missing certificate must be refused");
    assert!(err.to_string().contains("nope.pem"), "{err}");
}
