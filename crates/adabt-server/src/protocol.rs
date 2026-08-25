//! Wire protocol: framing, request kinds and status codes.
//!
//! Length-prefixed binary frames, deliberately not a text protocol. The Level 9
//! transport work in the roadmap — zero-copy reads, `io_uring`, per-core accept
//! — all assume a frame whose length is known before its body is parsed, and
//! retrofitting that onto a text protocol means rewriting every call site.
//!
//! Every frame carries a request id it did not need until pipelining exists.
//! Adding one later would be a format break; carrying eight unused bytes now is
//! not.

use adabt_core::error::{Error, Result};

pub const MAGIC: u32 = 0x4144_4254; // "ADBT"
pub const PROTOCOL_VERSION: u16 = 1;
/// Largest frame accepted, so a corrupt length cannot drive an allocation.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Ping,
    Get,
    Insert,
    Update,
    Delete,
    Count,
    Query,
    Explain,
    /// Ask the engine to run an optimization cycle.
    Optimize,
    /// Ask why the database is configured as it is.
    ExplainOptimizations,
    /// Ask for a Prometheus-formatted snapshot of what has been observed.
    Metrics,
    /// Present a bearer token. The only request a server with auth enabled
    /// will answer before it succeeds.
    Auth,
}

impl RequestKind {
    pub fn code(self) -> u8 {
        match self {
            RequestKind::Ping => 0,
            RequestKind::Get => 1,
            RequestKind::Insert => 2,
            RequestKind::Update => 3,
            RequestKind::Delete => 4,
            RequestKind::Count => 5,
            RequestKind::Query => 6,
            RequestKind::Explain => 7,
            RequestKind::Optimize => 8,
            RequestKind::ExplainOptimizations => 9,
            RequestKind::Metrics => 10,
            RequestKind::Auth => 11,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => RequestKind::Ping,
            1 => RequestKind::Get,
            2 => RequestKind::Insert,
            3 => RequestKind::Update,
            4 => RequestKind::Delete,
            5 => RequestKind::Count,
            6 => RequestKind::Query,
            7 => RequestKind::Explain,
            8 => RequestKind::Optimize,
            9 => RequestKind::ExplainOptimizations,
            10 => RequestKind::Metrics,
            11 => RequestKind::Auth,
            _ => return None,
        })
    }
    pub fn is_write(self) -> bool {
        matches!(
            self,
            RequestKind::Insert | RequestKind::Update | RequestKind::Delete
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Ok,
    NotFound,
    BadRequest,
    SchemaError,
    Conflict,
    /// The request was well-formed and understood, and this build genuinely
    /// does not do it yet — `Error::Unsupported`, e.g. a query containing a
    /// `Join`. Distinct from `BadRequest`: nothing about the request was
    /// wrong, and retrying it against a build that has caught up would work.
    NotImplemented,
    /// The query was stopped before finishing — a memory budget it exceeded,
    /// or an explicit cancellation on the caller's side. Distinct from every
    /// other status here: the request was fine and might succeed if sent
    /// again with more room or without being cancelled.
    Cancelled,
    /// Not authenticated. The connection may retry with an Auth request;
    /// nothing else on this connection will be answered until one succeeds.
    Unauthorized,
    /// Authentication failed. The token did not match; the connection stays
    /// unauthenticated and may try again, because a typo should cost a
    /// retry and not a reconnect.
    AuthDenied,
    /// Authenticated, but this connection's role does not permit the
    /// request — a read-only token asking to insert, say. Distinct from
    /// `Unauthorized`: the caller *is* known, so re-authenticating cannot
    /// help, and retrying the same request on this connection will always
    /// fail the same way.
    Forbidden,
    Internal,
}

impl StatusCode {
    pub fn code(self) -> u8 {
        match self {
            StatusCode::Ok => 0,
            StatusCode::NotFound => 1,
            StatusCode::BadRequest => 2,
            StatusCode::SchemaError => 3,
            StatusCode::Conflict => 4,
            StatusCode::Internal => 5,
            StatusCode::NotImplemented => 6,
            StatusCode::Cancelled => 7,
            StatusCode::Unauthorized => 8,
            StatusCode::AuthDenied => 9,
            StatusCode::Forbidden => 10,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => StatusCode::Ok,
            1 => StatusCode::NotFound,
            2 => StatusCode::BadRequest,
            3 => StatusCode::SchemaError,
            4 => StatusCode::Conflict,
            5 => StatusCode::Internal,
            6 => StatusCode::NotImplemented,
            7 => StatusCode::Cancelled,
            8 => StatusCode::Unauthorized,
            9 => StatusCode::AuthDenied,
            10 => StatusCode::Forbidden,
            _ => return None,
        })
    }
    pub fn is_ok(self) -> bool {
        self == StatusCode::Ok
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StatusCode::Ok => "ok",
            StatusCode::NotFound => "not found",
            StatusCode::BadRequest => "bad request",
            StatusCode::SchemaError => "schema error",
            StatusCode::Conflict => "conflict",
            StatusCode::Internal => "internal error",
            StatusCode::NotImplemented => "not implemented",
            StatusCode::Cancelled => "cancelled",
            StatusCode::Unauthorized => "unauthorized",
            StatusCode::Forbidden => "forbidden",
            StatusCode::AuthDenied => "authentication failed",
        }
    }

    /// Map an engine error onto a wire status.
    pub fn of(e: &Error) -> Self {
        match e {
            Error::NoSuchCollection(_) => StatusCode::NotFound,
            Error::CollectionExists(_) | Error::RecordExists(_) => StatusCode::Conflict,
            Error::UniqueViolation { .. } | Error::TransactionConflict { .. } => {
                StatusCode::Conflict
            }
            Error::Schema(_) => StatusCode::SchemaError,
            Error::InvalidOptimization(_) | Error::InvalidRestore(_) => StatusCode::BadRequest,
            Error::Unsupported(_) => StatusCode::NotImplemented,
            Error::Cancelled(_) => StatusCode::Cancelled,
            _ => StatusCode::Internal,
        }
    }
}

/// `magic(4) | version(2) | kind(1) | flags(1) | request_id(8) | len(4)`
pub const HEADER_LEN: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub flags: u8,
    pub request_id: u64,
    pub body: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u8, request_id: u64, body: Vec<u8>) -> Self {
        Self {
            kind,
            flags: 0,
            request_id,
            body,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.body.len());
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        out.push(self.kind);
        out.push(self.flags);
        out.extend_from_slice(&self.request_id.to_le_bytes());
        out.extend_from_slice(&(self.body.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.body);
        out
    }

    /// Decode one frame, returning it and the bytes consumed.
    ///
    /// Returns `Ok(None)` when the buffer holds only part of a frame, which is
    /// the normal case on a stream and must not be confused with corruption.
    pub fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            return Err(Error::Corruption(format!(
                "bad frame magic {magic:#010x}; expected {MAGIC:#010x}"
            )));
        }
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != PROTOCOL_VERSION {
            return Err(Error::Corruption(format!(
                "unsupported protocol version {version}"
            )));
        }
        let len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        if len > MAX_FRAME {
            return Err(Error::Corruption(format!(
                "frame of {len} bytes exceeds the {MAX_FRAME}-byte limit"
            )));
        }
        let total = HEADER_LEN + len as usize;
        if buf.len() < total {
            return Ok(None);
        }
        let mut request_id = [0u8; 8];
        request_id.copy_from_slice(&buf[8..16]);
        Ok(Some((
            Frame {
                kind: buf[6],
                flags: buf[7],
                request_id: u64::from_le_bytes(request_id),
                body: buf[HEADER_LEN..total].to_vec(),
            },
            total,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let f = Frame::new(RequestKind::Get.code(), 42, b"hello".to_vec());
        let bytes = f.encode();
        let (got, used) = Frame::decode(&bytes).unwrap().unwrap();
        assert_eq!(got, f);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn a_partial_frame_is_incomplete_not_corrupt() {
        // The normal case on a stream. Confusing it with corruption would drop
        // connections every time a read landed mid-frame.
        let bytes = Frame::new(0, 1, vec![7; 100]).encode();
        for n in 0..bytes.len() {
            assert!(
                matches!(Frame::decode(&bytes[..n]), Ok(None)),
                "prefix of {n} bytes was not reported as incomplete"
            );
        }
        assert!(Frame::decode(&bytes).unwrap().is_some());
    }

    #[test]
    fn several_frames_decode_from_one_buffer() {
        let mut buf = Vec::new();
        for i in 0..5u64 {
            buf.extend_from_slice(&Frame::new(1, i, vec![i as u8; 10]).encode());
        }
        let mut pos = 0;
        let mut seen = Vec::new();
        while let Some((f, used)) = Frame::decode(&buf[pos..]).unwrap() {
            seen.push(f.request_id);
            pos += used;
            if pos >= buf.len() {
                break;
            }
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = Frame::new(0, 1, vec![]).encode();
        bytes[0] ^= 0xff;
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn an_unknown_protocol_version_is_rejected() {
        let mut bytes = Frame::new(0, 1, vec![]).encode();
        bytes[4] = 99;
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn an_absurd_length_is_rejected_before_allocating() {
        let mut bytes = Frame::new(0, 1, vec![]).encode();
        bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Frame::decode(&bytes).is_err());
    }

    #[test]
    fn every_request_kind_round_trips() {
        for k in [
            RequestKind::Ping,
            RequestKind::Get,
            RequestKind::Insert,
            RequestKind::Update,
            RequestKind::Delete,
            RequestKind::Count,
            RequestKind::Query,
            RequestKind::Explain,
            RequestKind::Optimize,
            RequestKind::ExplainOptimizations,
        ] {
            assert_eq!(RequestKind::from_code(k.code()), Some(k));
        }
        assert_eq!(RequestKind::from_code(200), None);
    }

    #[test]
    fn every_status_round_trips() {
        for s in [
            StatusCode::Ok,
            StatusCode::NotFound,
            StatusCode::BadRequest,
            StatusCode::SchemaError,
            StatusCode::Conflict,
            StatusCode::Internal,
        ] {
            assert_eq!(StatusCode::from_code(s.code()), Some(s));
        }
    }

    #[test]
    fn engine_errors_map_to_sensible_statuses() {
        assert_eq!(
            StatusCode::of(&Error::NoSuchCollection("x".into())),
            StatusCode::NotFound
        );
        assert_eq!(
            StatusCode::of(&Error::RecordExists(adabt_core::ids::RecordId(1))),
            StatusCode::Conflict
        );
        assert_eq!(
            StatusCode::of(&Error::Corruption("x".into())),
            StatusCode::Internal
        );
    }

    #[test]
    fn writes_are_distinguishable_from_reads() {
        assert!(RequestKind::Insert.is_write());
        assert!(!RequestKind::Get.is_write());
        assert!(!RequestKind::Query.is_write());
    }
}
