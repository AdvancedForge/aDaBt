//! A blocking client.
//!
//! Here because a protocol with only one implementation is a protocol nobody has
//! checked: writing the reader against the writer is how a field that is encoded
//! but never decoded goes unnoticed. It is also what the server's own tests use,
//! which means every test of the server is also a test of this.
//!
//! One request at a time, in order. The frame header carries a request id so
//! that pipelining is possible later without a format change, but nothing here
//! sends a second request before reading the first reply.

use adabt_core::error::{Error, Result};
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use adabt_ir::CmpOp;
use std::io::{Read, Write};

use crate::protocol::{Frame, RequestKind, StatusCode, HEADER_LEN};
use crate::server::error_from;
use crate::wire::{decode_rows, QuerySpec, Reader, Writer};

/// A protocol client over any byte stream.
///
/// The default is a plain TCP connection; [`Client::over_stream`] accepts
/// anything that reads and writes bytes — which is how a TLS session plugs
/// in without this type knowing certificates exist. Framing and
/// request/response correlation are written once and cannot drift between
/// the encrypted and plaintext paths, mirroring the server side.
pub struct Client<S: Read + Write = std::net::TcpStream> {
    stream: S,
    next_id: u64,
}

impl Client<std::net::TcpStream> {
    pub fn connect(addr: impl std::net::ToSocketAddrs) -> std::io::Result<Self> {
        let stream = std::net::TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream, next_id: 1 })
    }
}

impl<S: Read + Write> Client<S> {
    /// Adopt an already-connected stream — TLS, unix socket, anything.
    pub fn over_stream(stream: S) -> Self {
        Self { stream, next_id: 1 }
    }

    fn call(&mut self, kind: RequestKind, body: Vec<u8>) -> Result<Vec<u8>> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(Frame::new(kind.code(), id, body))?;
        self.read_reply(id)
    }

    /// Put one pre-built frame on the wire.
    ///
    /// The escape hatch from the typed method surface: a protocol fuzzer,
    /// a conformance test, or a proxy needs to send frames the *client*
    /// would never construct — unknown kinds, malformed bodies, hostile
    /// lengths — while still using this connection's framing and ids.
    pub fn send_raw(&mut self, frame: Frame) -> Result<()> {
        self.stream.write_all(&frame.encode()).map_err(Error::Io)
    }

    /// Read the next reply frame off this connection, requiring that it
    /// carries `id`.
    ///
    /// Unlike `Client::call`, the frame is returned as-is: status not
    /// interpreted, body not decoded. That is what an adversarial test wants
    /// — the raw shape of what a hostile request earned.
    pub fn next_reply(&mut self, id: u64) -> Result<Frame> {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if let Some((reply, _)) = Frame::decode(&buf)? {
                if reply.request_id != id {
                    return Err(Error::Corruption(format!(
                        "reply is for request {} but {id} was sent",
                        reply.request_id
                    )));
                }
                return Ok(reply);
            }
            let n = self.stream.read(&mut chunk).map_err(Error::Io)?;
            if n == 0 {
                return Err(Error::Corruption(format!(
                    "the server closed the connection with {} of at least {HEADER_LEN} \
                     reply bytes received",
                    buf.len()
                )));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// The typed path: `next_reply` plus status interpretation — an ok
    /// status yields the body, anything else becomes the mapped error.
    fn read_reply(&mut self, id: u64) -> Result<Vec<u8>> {
        let reply = self.next_reply(id)?;
        let status = StatusCode::from_code(reply.kind)
            .ok_or_else(|| Error::Corruption(format!("unknown status code {}", reply.kind)))?;
        if status.is_ok() {
            Ok(reply.body)
        } else {
            Err(error_from(status, &reply.body))
        }
    }

    pub fn ping(&mut self) -> Result<()> {
        self.call(RequestKind::Ping, Vec::new()).map(|_| ())
    }

    /// Present a bearer token to a server that requires one.
    ///
    /// Must be the first thing a client does against an auth-enabled server;
    /// every other request is refused with `Unauthorized` until this
    /// succeeds. A wrong token is an `Error::Remote` whose message says so,
    /// and the connection may simply try again — a typo costs a retry, not
    /// a reconnect.
    pub fn auth(&mut self, token: &str) -> Result<()> {
        let mut w = Writer::new();
        w.str(token);
        self.call(RequestKind::Auth, w.finish()).map(|_| ())
    }

    pub fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        let body = self.call(
            RequestKind::Get,
            Writer::new().str(collection).u64(id.0).finish(),
        )?;
        let mut r = Reader::new(&body);
        let out = match r.u8("found flag")? {
            0 => None,
            _ => Some(r.record("record")?),
        };
        r.end()?;
        Ok(out)
    }

    pub fn insert(&mut self, collection: &str, id: RecordId, rec: &Record) -> Result<()> {
        let mut w = Writer::new();
        w.str(collection).u64(id.0).record(rec)?;
        self.call(RequestKind::Insert, w.finish()).map(|_| ())
    }

    pub fn update(&mut self, collection: &str, id: RecordId, rec: &Record) -> Result<bool> {
        let mut w = Writer::new();
        w.str(collection).u64(id.0).record(rec)?;
        let body = self.call(RequestKind::Update, w.finish())?;
        Ok(Reader::new(&body).u8("existed")? != 0)
    }

    pub fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let body = self.call(
            RequestKind::Delete,
            Writer::new().str(collection).u64(id.0).finish(),
        )?;
        Ok(Reader::new(&body).u8("existed")? != 0)
    }

    pub fn count(&mut self, collection: &str) -> Result<u64> {
        let body = self.call(RequestKind::Count, Writer::new().str(collection).finish())?;
        Reader::new(&body).u64("count")
    }

    pub fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        self.query(collection, None, 0)
    }

    /// A filtered scan — the query shape the wire format commits to.
    pub fn query(
        &mut self,
        collection: &str,
        filter: Option<(&str, CmpOp, Value)>,
        limit: u32,
    ) -> Result<Vec<(RecordId, Record)>> {
        let spec = QuerySpec {
            collection: collection.to_string(),
            filter: filter.map(|(f, op, v)| (f.to_string(), op, v)),
            limit,
        };
        let body = self.call(RequestKind::Query, spec.encode())?;
        decode_rows(&body)
    }

    pub fn explain(
        &mut self,
        collection: &str,
        filter: Option<(&str, CmpOp, Value)>,
    ) -> Result<String> {
        let spec = QuerySpec {
            collection: collection.to_string(),
            filter: filter.map(|(f, op, v)| (f.to_string(), op, v)),
            limit: 0,
        };
        let body = self.call(RequestKind::Explain, spec.encode())?;
        Reader::new(&body).str("explanation")
    }

    pub fn optimize(&mut self) -> Result<String> {
        let body = self.call(RequestKind::Optimize, Vec::new())?;
        Reader::new(&body).str("report")
    }

    pub fn explain_optimizations(&mut self) -> Result<String> {
        let body = self.call(RequestKind::ExplainOptimizations, Vec::new())?;
        Reader::new(&body).str("explanation")
    }

    /// Prometheus exposition text for everything the server has observed.
    pub fn metrics(&mut self) -> Result<String> {
        let body = self.call(RequestKind::Metrics, Vec::new())?;
        Reader::new(&body).str("metrics")
    }
}
