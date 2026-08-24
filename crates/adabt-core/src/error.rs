use crate::ids::RecordId;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no such collection: {0}")]
    NoSuchCollection(String),

    #[error("collection already exists: {0}")]
    CollectionExists(String),

    #[error("record {0} already exists")]
    RecordExists(RecordId),

    /// A logically valid request this build does not implement yet — as
    /// opposed to an error in the request itself. `Join` is reserved in the
    /// query IR and returns this from the planner and executor until M23
    /// builds a real join algorithm over it; every other error variant here
    /// means "this is wrong," and this one alone means "this is right, and not
    /// yet built."
    #[error("not yet implemented: {0}")]
    Unsupported(String),

    /// A transaction's write conflicted with one committed after its snapshot
    /// began — first-committer-wins, and this one lost.
    #[error("transaction conflict on {collection}.{id}: modified since the transaction's snapshot began")]
    TransactionConflict { collection: String, id: RecordId },

    /// A unique constraint was about to be violated.
    ///
    /// Carries the field and the value, not the conflicting id — the caller
    /// asked "can I write this value" and the answer is no, regardless of which
    /// existing record holds it.
    #[error("unique constraint on {collection}.{field} violated by value {value}")]
    UniqueViolation {
        collection: String,
        field: String,
        value: String,
    },

    #[error("schema violation: {0}")]
    Schema(#[from] SchemaError),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("corruption detected: {0}")]
    Corruption(String),

    /// Raised when an optimization is asked to do something its declared
    /// metadata says it cannot. Always a bug in the optimizer, never user error.
    #[error("invalid optimization request: {0}")]
    InvalidOptimization(String),

    /// The database on disk was written by a build this one cannot read.
    ///
    /// Refused rather than read optimistically. The failure mode of optimism
    /// here is not a crash but a plausible record with the wrong bytes in it.
    #[error("database format version {found} is not supported by this build, which understands {supported}")]
    IncompatibleFormat { found: u32, supported: u32 },

    /// A point-in-time restore asked for a moment this backup cannot produce.
    ///
    /// The backup's own checkpoint already advanced the catalog past
    /// `earliest` — replaying only entries at or below `requested` would mean
    /// starting from a catalog that already reflects writes after it, which is
    /// not an earlier state at all, just a corrupted one wearing an earlier
    /// label. An earlier backup is the only fix; there is no way to replay
    /// past a checkpoint backwards.
    #[error(
        "cannot restore to lsn {requested}: this backup's own checkpoint already reached lsn {earliest}"
    )]
    RestoreTargetUnreachable { requested: u64, earliest: u64 },

    /// A backup or restore precondition was not met: the source is not a
    /// backup this build recognizes, or the destination is not the empty
    /// directory a restore is only ever willing to write into.
    #[error("{0}")]
    InvalidRestore(String),

    /// A query was stopped before it produced a result — by an explicit
    /// cancellation, a timeout, or a memory budget it exceeded while
    /// running. Distinct from every other variant here in one way: the
    /// database's state is exactly as it was before the query started. A
    /// read that is stopped reads nothing; a write-in-progress is not
    /// something this variant is ever raised for, because nothing here
    /// buffers a partial write past the point it could still be abandoned
    /// cleanly.
    #[error("query cancelled: {0}")]
    Cancelled(String),

    /// An error a remote engine reported over the wire.
    ///
    /// Its own variant rather than a reconstruction of the original. A status
    /// code narrows what went wrong but does not determine it, and rebuilding
    /// the server's error from one would produce something that names a cause
    /// nobody established — a schema failure arriving home as a corruption
    /// report. The status and the server's own words are what is actually
    /// known, so they are what is carried.
    #[error("{status} from the server: {message}")]
    Remote {
        status: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    #[error("unknown field `{field}` (schema mode {mode:?} forbids extra fields)")]
    UnknownField {
        field: String,
        mode: crate::schema::SchemaMode,
    },

    #[error("missing required field `{0}`")]
    MissingField(String),

    #[error("field `{field}`: expected {expected}, got {actual}")]
    TypeMismatch {
        field: String,
        expected: String,
        actual: String,
    },

    #[error("field `{field}`: value of length {len} exceeds fixed width {width}")]
    TooWide {
        field: String,
        len: usize,
        width: u32,
    },

    #[error(
        "field `{field}`: fixed width {width} leaves no room for content after its length prefix"
    )]
    ZeroCapacity { field: String, width: u32 },

    #[error("schema mode Fixed requires every field to be fixed-width, but `{0}` is not")]
    NotFixedWidth(String),

    #[error("duplicate field name `{0}`")]
    DuplicateField(String),

    #[error("schema has no fields")]
    Empty,
}
