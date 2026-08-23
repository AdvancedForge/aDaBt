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

    #[error("schema mode Fixed requires every field to be fixed-width, but `{0}` is not")]
    NotFixedWidth(String),

    #[error("duplicate field name `{0}`")]
    DuplicateField(String),

    #[error("schema has no fields")]
    Empty,
}
