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
        })
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
            // A connection that fails takes nothing else down with it: the
            // thread ends, the socket closes, every other client carries on.
            std::thread::spawn(move || {
                let _guard = guard;
                let _ = handle(stream, db, idle_timeout);
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

fn handle(
    mut stream: TcpStream,
    db: Shared,
    idle_timeout: Option<Duration>,
) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    // Re-armed by the OS on every `read`, so this bounds the gap between
    // bytes, not the connection's total lifetime — a client making steady
    // progress is never cut off by it, only one that has gone silent.
    stream.set_read_timeout(idle_timeout).ok();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
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
                    let reply = respond(&frame, &db);
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

fn respond(frame: &Frame, db: &Shared) -> Frame {
    let id = frame.request_id;
    let Some(kind) = RequestKind::from_code(frame.kind) else {
        return error_frame(
            id,
            StatusCode::BadRequest,
            &format!("unknown request kind {}", frame.kind),
        );
    };
    match dispatch(kind, &frame.body, db) {
        Ok(body) => Frame::new(StatusCode::Ok.code(), id, body),
        Err(e) => error_frame(id, StatusCode::of(&e), &e.to_string()),
    }
}

fn error_frame(id: u64, status: StatusCode, message: &str) -> Frame {
    let mut w = Writer::new();
    w.str(message);
    Frame::new(status.code(), id, w.finish())
}

fn dispatch(kind: RequestKind, body: &[u8], db: &ShardedDatabase) -> Result<Vec<u8>> {
    let mut r = Reader::new(body);
    match kind {
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
