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
use std::net::{TcpStream, ToSocketAddrs};

use crate::protocol::{Frame, RequestKind, StatusCode, HEADER_LEN};
use crate::server::error_from;
use crate::wire::{decode_rows, QuerySpec, Reader, Writer};

pub struct Client {
    stream: TcpStream,
    next_id: u64,
}

impl Client {
    pub fn connect(addr: impl ToSocketAddrs) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        Ok(Self { stream, next_id: 1 })
    }

    fn call(&mut self, kind: RequestKind, body: Vec<u8>) -> Result<Vec<u8>> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = Frame::new(kind.code(), id, body);
        self.stream.write_all(&frame.encode()).map_err(Error::Io)?;

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
                let status = StatusCode::from_code(reply.kind).ok_or_else(|| {
                    Error::Corruption(format!("unknown status code {}", reply.kind))
                })?;
                return if status.is_ok() {
                    Ok(reply.body)
                } else {
                    Err(error_from(status, &reply.body))
                };
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
