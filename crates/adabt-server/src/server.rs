//! The listener.
//!
//! Length-prefixed frames over TCP, one thread per connection, one engine behind
//! a mutex.
//!
//! # There is no lock around the engine
//!
//! The database is a [`ShardedDatabase`], which is *N* complete engines each
//! with its own files and its own lock. Two requests for records in different
//! shards contend for nothing, so the server holds an `Arc` and no mutex of its
//! own — a hundred connections are a hundred threads that only queue behind each
//! other when they want the same partition.
//!
//! That is genuine shared-nothing partitioning and not thread-per-core. There is
//! still no core pinning, no run-to-completion scheduler, no `io_uring` and no
//! zero-copy path; those are what the Level 9 roadmap means and they are a
//! different piece of work built on this one. Running with `--shards 1` is the
//! old behaviour exactly, which is the honest way to measure what the
//! partitioning is worth.
//!
//! # Reading a stream
//!
//! A frame arrives in as many pieces as the network feels like. The read loop
//! keeps a buffer, appends whatever arrives, and decodes as many whole frames as
//! it holds — `Frame::decode` returning `None` means "not yet", which is the
//! normal case and not an error. A connection that sends a frame header with a
//! bad magic or an impossible length is closed rather than resynchronised: the
//! stream is already meaningless and guessing where the next frame starts is how
//! a framing bug becomes a parsing bug.
//!
//! # What keeps one slow or silent client from costing everyone else
//!
//! Two things, both because this is a thread-per-connection server and a
//! thread that is stuck is a thread nothing else can use. An **idle read
//! timeout** closes a connection that opened and then sent nothing — the
//! `std::net` equivalent of "hung up on you" — so a client that connects and
//! goes silent gives its thread back after a bounded wait rather than holding
//! it forever. A **connection cap** refuses a new connection outright once
//! that many are already open, rather than queuing it behind however many
//! threads the process is willing to spawn; a caller over the cap sees a
//! closed connection immediately, not a hang.
//!
//! # Shutdown
//!
//! [`Stopper::stop`] only stops the *accept* loop. `serve` then waits — up to
//! [`DRAIN_TIMEOUT`] — for every connection already accepted to finish on its
//! own, so a request in flight when shutdown is requested gets to complete
//! rather than being cut off mid-reply. What happens after `serve` returns —
//! a final checkpoint, for instance — is the caller's decision; this module
//! only guarantees that by the time it returns, no connection thread is still
//! running.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_engine::sharded::ShardedDatabase;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::protocol::{Frame, RequestKind, StatusCode, HEADER_LEN, MAX_FRAME};
use crate::wire::{encode_rows, QuerySpec, Reader, Writer};

/// A connection that neither sends nor receives anything for this long is
/// assumed dead and closed, so a silent client cannot hold a thread forever.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Refuse a new connection once this many are already open.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;
/// How long `serve` waits, after the accept loop stops, for connections
/// already in flight to finish on their own before returning anyway.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// The engine, shared by every connection. No lock: the shards hold their own.
pub type Shared = Arc<ShardedDatabase>;

pub struct Server {
    listener: TcpListener,
    db: Shared,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    max_connections: usize,
    idle_timeout: Option<Duration>,
    auth: AuthConfig,
    /// When set, every accepted connection completes a TLS 1.3 handshake
    /// before any protocol byte is read. `None` is plaintext, the posture
    /// this server shipped with.
    tls: Option<Arc<rustls::ServerConfig>>,
}

/// What a connection is allowed to do once it has authenticated.
///
/// Two roles, because two is what the threat model justifies today: an
/// operator token that may do anything, and a read-only token for dashboards
/// and probes. Roles are a property of the *token*, not the connection —
/// a client cannot upgrade by asking, only by presenting a different
/// credential. Per-collection grants are the next slice if a deployment
/// ever needs them; nothing in the wire format assumes they will not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Every request kind.
    Admin,
    /// Reads and introspection only: Get, Count, Query, Explain,
    /// ExplainOptimizations, Metrics, Ping. Writes and optimization
    /// triggers are refused with `Forbidden`.
    Reader,
}

impl Role {
    /// Whether this role may issue `kind`.
    fn permits(self, kind: RequestKind) -> bool {
        match self {
            Role::Admin => true,
            Role::Reader => !matches!(
                kind,
                RequestKind::Insert
                    | RequestKind::Update
                    | RequestKind::Delete
                    | RequestKind::Optimize
            ),
        }
    }

    /// Is `self` at least as powerful as `floor`?
    fn at_least(self, floor: Role) -> bool {
        matches!((self, floor), (_, Role::Reader) | (Role::Admin, _))
    }
}

/// The server's credential table.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    admin_token: Option<String>,
    reader_token: Option<String>,
    /// Per-collection role floors: a collection mapped to [`Role::Admin`]
    /// refuses every request from a reader connection — reads included,
    /// which is what makes this more than the role model already says.
    /// Collections absent from the map follow the connection's role alone.
    floors: std::collections::HashMap<String, Role>,
}

impl AuthConfig {
    fn open_role(&self) -> Option<Role> {
        // With no credentials configured the listener is open: every
        // connection starts as admin, which is exactly the trusted-network
        // posture the server shipped with.
        if self.admin_token.is_none() && self.reader_token.is_none() {
            return Some(Role::Admin);
        }
        None
    }

    fn authenticate(&self, presented: &str) -> Option<Role> {
        let admin = self
            .admin_token
            .as_deref()
            .is_some_and(|t| tokens_match(t, presented));
        let reader = self
            .reader_token
            .as_deref()
            .is_some_and(|t| tokens_match(t, presented));
        match (admin, reader) {
            (true, _) => Some(Role::Admin),
            (_, true) => Some(Role::Reader),
            _ => None,
        }
    }
}

/// A PEM load failure with the offending file named — the caller's error is
/// about a file on disk, and an unnamed "decode failed" would send someone
/// hunting through the wrong directory.
fn bad_pem(
    path: &std::path::Path,
) -> impl Fn(rustls::pki_types::pem::Error) -> std::io::Error + '_ {
    move |e| std::io::Error::other(format!("reading {}: {e}", path.display()))
}

impl Server {
    pub fn bind(addr: impl ToSocketAddrs, db: ShardedDatabase) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr)?,
            db: Arc::new(db),
            stop: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
            auth: AuthConfig::default(),
            tls: None,
        })
    }

    /// Encrypt every connection with TLS, presenting the certificate chain
    /// and key from these PEM files.
    ///
    /// The handshake happens before any protocol byte is read — a client
    /// that speaks plaintext to a TLS listener sees a handshake failure, not
    /// a half-parsed frame. No client certificate is required; tokens
    /// (`with_auth_token`) remain the authentication story. Fails at bind
    /// time on unreadable files or a mismatched pair, which is the right
    /// moment to learn that: a server that came up without the encryption it
    /// was asked for would be worse than one that did not come up.
    pub fn with_tls(
        mut self,
        cert_pem: &std::path::Path,
        key_pem: &std::path::Path,
    ) -> std::io::Result<Self> {
        use rustls::pki_types::pem::PemObject;
        let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_file_iter(cert_pem)
            .map_err(bad_pem(cert_pem))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(bad_pem(cert_pem))?;
        if certs.is_empty() {
            return Err(std::io::Error::other(format!(
                "{} contained no certificates",
                cert_pem.display()
            )));
        }
        let key =
            rustls::pki_types::PrivateKeyDer::from_pem_file(key_pem).map_err(bad_pem(key_pem))?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.tls = Some(Arc::new(config));
        Ok(self)
    }

    /// Require a bearer token on every connection, granting `admin` to the
    /// connections that present it.
    ///
    /// Until a connection presents this token via an Auth request, every
    /// other request it sends is refused with `Unauthorized` — including
    /// Ping, because an unauthenticated oracle that says "the server is
    /// alive" is a small thing to leak and a smaller thing to need. `None`
    /// (the default) disables the gate entirely, which is the trusted-
    /// network posture this server shipped with.
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth.admin_token = Some(token.into());
        self
    }

    /// Issue a second credential that authenticates to the [`Role::Reader`]
    /// role: reads and introspection only. Requires an admin token to be
    /// configured — a read-only credential alongside an open listener would
    /// be a lock on a door that is already standing open.
    pub fn with_read_token(mut self, token: impl Into<String>) -> Self {
        self.auth.reader_token = Some(token.into());
        self
    }

    /// Require at least `floor` to touch `collection` — every request kind
    /// that names it, reads included.
    ///
    /// This is the per-collection grant, one rule per collection: a floor of
    /// [`Role::Admin`] walls a collection off from reader connections (a
    /// secrets table beside a public dashboard in one database), while an
    /// unmapped collection keeps following the connection's role. Floors are
    /// checked after authentication and role authorization — a reader gets
    /// `Forbidden`, not `Unauthorized`, because who they are is known; this
    /// collection is simply not for them.
    pub fn with_collection_floor(mut self, collection: impl Into<String>, floor: Role) -> Self {
        self.auth.floors.insert(collection.into(), floor);
        self
    }

    /// Refuse a connection beyond this many concurrently open. Default
    /// [`DEFAULT_MAX_CONNECTIONS`].
    pub fn with_max_connections(mut self, n: usize) -> Self {
        self.max_connections = n;
        self
    }

    /// Close a connection that has been idle this long. `None` disables the
    /// timeout — every connection this build ever shipped with one is asking
    /// for it explicitly at that point, not falling into it by omission.
    /// Default `Some(`[`DEFAULT_IDLE_TIMEOUT`]`)`.
    pub fn with_idle_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// The address actually bound.
    ///
    /// Worth having because binding to port 0 and asking afterwards is the only
    /// way to run a test server without picking a port and hoping.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn database(&self) -> Shared {
        Arc::clone(&self.db)
    }

    /// The binary's flag pair, applied as one: both flags or neither. A
    /// certificate without a key (or the reverse) is a startup error, not a
    /// half-encrypted listener.
    pub fn with_tls_flags(
        self,
        cert: Option<&std::path::Path>,
        key: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        match (cert, key) {
            (Some(cert), Some(key)) => self.with_tls(cert, key),
            (None, None) => Ok(self),
            _ => Err(std::io::Error::other(
                "--tls-cert and --tls-key must be given together",
            )),
        }
    }

    /// A handle that stops the accept loop.
    pub fn stopper(&self) -> Stopper {
        Stopper {
            flag: Arc::clone(&self.stop),
            addr: self.listener.local_addr().ok(),
        }
    }

    /// A live view of how many connections are open, readable for as long as
    /// this outlives `serve` — obtained the same way `stopper()` and
    /// `database()` are, before `serve(self)` takes ownership, since nothing
    /// could otherwise call it once that has happened.
    pub fn connection_count(&self) -> ConnectionCount {
        ConnectionCount(Arc::clone(&self.active))
    }

    /// Accept connections until stopped, then wait for the ones already
    /// accepted to finish. See the module docs for what "wait" bounds to.
    pub fn serve(self) {
        for stream in self.listener.incoming() {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else { continue };
            if self.active.load(Ordering::Relaxed) >= self.max_connections {
                // Closed by dropping it: no thread, no reply, nothing this
                // connection could do differently by waiting longer.
                continue;
            }
            self.active.fetch_add(1, Ordering::Relaxed);
            let guard = ActiveGuard(Arc::clone(&self.active));
            let db = Arc::clone(&self.db);
            let idle_timeout = self.idle_timeout;
            let auth = self.auth.clone();
            let tls = self.tls.clone();
            // A connection that fails takes nothing else down with it: the
            // thread ends, the socket closes, every other client carries on.
            std::thread::spawn(move || {
                let _guard = guard;
                // Socket tuning happens on the raw stream, before any
                // wrapping: the timeouts bound gaps between bytes at the TCP
                // layer either way.
                stream.set_nodelay(true).ok();
                stream.set_read_timeout(idle_timeout).ok();
                let conn = match &tls {
                    None => None,
                    Some(config) => match rustls::ServerConnection::new(Arc::clone(config)) {
                        Ok(c) => Some(c),
                        // A connection that cannot even be set up is closed
                        // by dropping the stream; nothing else to do.
                        Err(_) => return,
                    },
                };
                match conn {
                    Some(c) => {
                        let _ = handle(rustls::StreamOwned::new(c, stream), db, auth);
                    }
                    None => {
                        let _ = handle(stream, db, auth);
                    }
                }
            });
        }
        let deadline = std::time::Instant::now() + DRAIN_TIMEOUT;
        while self.active.load(Ordering::Relaxed) > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Decrements the shared connection count when a connection's thread ends,
/// whatever the reason — an ordinary close, a read error, or a panic inside
/// `handle`. The count is what `serve`'s drain wait and the connection cap
/// both read, so it must never be wrong for a reason as mundane as "the
/// connection's own cleanup code didn't run."
struct ActiveGuard(Arc<AtomicUsize>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Stops a running server.
pub struct Stopper {
    flag: Arc<AtomicBool>,
    addr: Option<SocketAddr>,
}

impl Stopper {
    /// Ask the accept loop to finish.
    ///
    /// Sets the flag and then connects once, because `incoming()` is blocking
    /// and a flag nobody is looking at stops nothing.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
        if let Some(addr) = self.addr {
            let _ = TcpStream::connect(addr);
        }
    }
}

/// A live count of open connections, readable from outside `serve`.
pub struct ConnectionCount(Arc<AtomicUsize>);
impl ConnectionCount {
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// Serve one connection to completion, over whatever transport it arrived
/// on — a plain `TcpStream`, or the same socket wrapped in a TLS session.
///
/// The generic is what keeps TLS a one-line change per connection: framing,
/// authentication, and dispatch are written once against `Read`/`Write` and
/// cannot drift between the encrypted and plaintext paths.
fn handle<S: Read + Write>(mut stream: S, db: Shared, auth: AuthConfig) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    // Per-connection authorization state. A connection starts unauthenticated
    // whenever any token is required; the role it earns is remembered here
    // and nowhere else, so one client's proof says nothing about any other
    // connection.
    let mut role = auth.open_role();
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(()); // peer closed
        }
        buf.extend_from_slice(&chunk[..n]);
        loop {
            match Frame::decode(&buf) {
                // Not a whole frame yet. Normal on a stream.
                Ok(None) => break,
                Ok(Some((frame, used))) => {
                    buf.drain(..used);
                    let reply = respond(&frame, &db, &auth, &mut role);
                    stream.write_all(&reply.encode())?;
                }
                // The stream is no longer frames. Nothing useful can be read
                // from here, and hunting for the next plausible header would
                // turn a framing error into a parsing error.
                Err(_) => return Ok(()),
            }
        }
        if buf.len() > MAX_FRAME as usize + HEADER_LEN {
            return Ok(());
        }
    }
}

fn respond(frame: &Frame, db: &Shared, auth: &AuthConfig, role: &mut Option<Role>) -> Frame {
    let id = frame.request_id;
    let Some(kind) = RequestKind::from_code(frame.kind) else {
        return error_frame(
            id,
            StatusCode::BadRequest,
            &format!("unknown request kind {}", frame.kind),
        );
    };
    // The gate, in two layers. First authentication: nothing but Auth is
    // answered until a credential is presented, so an unauthenticated
    // connection cannot even count rows. Then authorization: what the
    // presented credential's role permits. Both run before the engine is
    // touched.
    let Some(known) = role else {
        if kind != RequestKind::Auth {
            return error_frame(id, StatusCode::Unauthorized, "authentication required");
        }
        let token = Reader::new(&frame.body)
            .str("auth token")
            .unwrap_or_default();
        if let Some(earned) = auth.authenticate(&token) {
            *role = Some(earned);
            return Frame::new(StatusCode::Ok.code(), id, Vec::new());
        }
        return error_frame(
            id,
            StatusCode::AuthDenied,
            "authentication failed; the connection remains unauthenticated",
        );
    };
    if !known.permits(kind) {
        return error_frame(
            id,
            StatusCode::Forbidden,
            "this connection's role does not permit that request",
        );
    }
    // Per-collection floors, checked after role authorization and before the
    // engine is touched. The collection name is parsed leniently here — a
    // body too corrupt to name one fails in `dispatch` with `BadRequest`,
    // which is the honest verdict for it.
    if let Some(collection) = collection_of(kind, &frame.body) {
        if let Some(floor) = auth.floors.get(&collection) {
            if !known.at_least(*floor) {
                return error_frame(
                    id,
                    StatusCode::Forbidden,
                    "this collection requires a higher-role credential",
                );
            }
        }
    }
    match dispatch(kind, &frame.body, db) {
        Ok(body) => Frame::new(StatusCode::Ok.code(), id, body),
        Err(e) => error_frame(id, StatusCode::of(&e), &e.to_string()),
    }
}

/// The collection a request names, if its kind carries one.
///
/// Every such frame — Get, Insert, Update, Delete, Count, Query, Explain —
/// begins with a length-prefixed string, so one read answers for all of
/// them. Lenient by design: a body that will not parse as even that string
/// yields `None`, and `dispatch` gives it the `BadRequest` it deserves.
fn collection_of(kind: RequestKind, body: &[u8]) -> Option<String> {
    match kind {
        RequestKind::Get
        | RequestKind::Insert
        | RequestKind::Update
        | RequestKind::Delete
        | RequestKind::Count
        | RequestKind::Query
        | RequestKind::Explain => Reader::new(body).str("collection").ok(),
        _ => None,
    }
}

/// Token comparison that does not leak its progress through timing.
///
/// A plain `==` returns at the first differing byte, which lets a client on
/// the same network measure how far into the token it got and reconstruct
/// the secret a piece at a time. Folding every byte into one accumulator
/// costs nothing measurable and removes the channel; this is the whole
/// trick, and it is small enough to be worth doing even here.
fn tokens_match(want: &str, got: &str) -> bool {
    let (w, g) = (want.as_bytes(), got.as_bytes());
    if w.len() != g.len() {
        // Length still differs up front — unavoidable without padding — but
        // length alone narrows a secret much less than its bytes do.
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in w.iter().zip(g.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn error_frame(id: u64, status: StatusCode, message: &str) -> Frame {
    let mut w = Writer::new();
    w.str(message);
    Frame::new(status.code(), id, w.finish())
}

fn dispatch(kind: RequestKind, body: &[u8], db: &ShardedDatabase) -> Result<Vec<u8>> {
    let mut r = Reader::new(body);
    match kind {
        // Unreachable through `respond` — the auth gate handles Auth itself
        // and never forwards it here. A match arm rather than a `_` so a
        // future request kind added to the enum fails to compile here until
        // someone decides how it is served, instead of silently vanishing
        // into a catch-all.
        RequestKind::Auth => unreachable!("respond handles Auth before dispatch"),
        RequestKind::Ping => {
            r.end()?;
            Ok(Vec::new())
        }
        RequestKind::Get => {
            let (collection, id) = (r.str("collection")?, RecordId(r.u64("id")?));
            r.end()?;
            let mut w = Writer::new();
            match db.get(&collection, id)? {
                Some(rec) => {
                    w.u8(1);
                    w.record(&rec)?;
                }
                None => {
                    w.u8(0);
                }
            }
            Ok(w.finish())
        }
        RequestKind::Insert => {
            let (collection, id) = (r.str("collection")?, RecordId(r.u64("id")?));
            let rec = r.record("record")?;
            r.end()?;
            db.insert(&collection, id, rec)?;
            Ok(Vec::new())
        }
        RequestKind::Update => {
            let (collection, id) = (r.str("collection")?, RecordId(r.u64("id")?));
            let rec = r.record("record")?;
            r.end()?;
            let existed = db.update(&collection, id, rec)?;
            Ok(Writer::new().u8(existed as u8).finish())
        }
        RequestKind::Delete => {
            let (collection, id) = (r.str("collection")?, RecordId(r.u64("id")?));
            r.end()?;
            let existed = db.delete(&collection, id)?;
            Ok(Writer::new().u8(existed as u8).finish())
        }
        RequestKind::Count => {
            let collection = r.str("collection")?;
            r.end()?;
            Ok(Writer::new().u64(db.count(&collection)? as u64).finish())
        }
        RequestKind::Query => {
            let spec = QuerySpec::decode(body)?;
            let rows = db.query(&spec.to_plan())?;
            encode_rows(&rows)
        }
        RequestKind::Explain => {
            let spec = QuerySpec::decode(body)?;
            Ok(Writer::new().str(&db.explain(&spec.to_plan())).finish())
        }
        RequestKind::Optimize => {
            r.end()?;
            // Every shard runs its own cycle from its own traffic, so the reply
            // is the decision log rather than one report: there is no single
            // answer to "what changed" across shards that decide separately.
            db.optimize()?;
            Ok(Writer::new().str(&db.explain_optimizations()).finish())
        }
        RequestKind::ExplainOptimizations => {
            r.end()?;
            Ok(Writer::new().str(&db.explain_optimizations()).finish())
        }
        RequestKind::Metrics => {
            r.end()?;
            Ok(Writer::new().str(&db.metrics_text()).finish())
        }
    }
}

/// Turn an error response body back into an error.
pub(crate) fn error_from(status: StatusCode, body: &[u8]) -> Error {
    Error::Remote {
        status: status.as_str(),
        message: Reader::new(body)
            .str("error message")
            .unwrap_or_else(|_| "no detail".into()),
    }
}
