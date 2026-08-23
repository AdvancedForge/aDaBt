//! Operation sequences and their outcomes.
//!
//! `OpOutcome` compares *error kinds*, never error strings. Two implementations
//! must agree on whether an operation failed and broadly why; demanding
//! identical prose would couple the engine's diagnostics to the reference
//! model's and make good error messages a test failure.

use adabt_core::error::Error;
use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::store::LogicalStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Insert {
        collection: String,
        id: RecordId,
        rec: Record,
    },
    Get {
        collection: String,
        id: RecordId,
    },
    Update {
        collection: String,
        id: RecordId,
        rec: Record,
    },
    Delete {
        collection: String,
        id: RecordId,
    },
    Scan {
        collection: String,
    },
    Count {
        collection: String,
    },
}

impl Op {
    pub fn collection(&self) -> &str {
        match self {
            Op::Insert { collection, .. }
            | Op::Get { collection, .. }
            | Op::Update { collection, .. }
            | Op::Delete { collection, .. }
            | Op::Scan { collection }
            | Op::Count { collection } => collection,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Op::Insert { .. } => "insert",
            Op::Get { .. } => "get",
            Op::Update { .. } => "update",
            Op::Delete { .. } => "delete",
            Op::Scan { .. } => "scan",
            Op::Count { .. } => "count",
        }
    }
}

/// Coarse error classification: the granularity at which two correct
/// implementations are required to agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    NoSuchCollection,
    CollectionExists,
    RecordExists,
    Schema,
    Other,
}

impl From<&Error> for ErrKind {
    fn from(e: &Error) -> Self {
        match e {
            Error::NoSuchCollection(_) => ErrKind::NoSuchCollection,
            Error::CollectionExists(_) => ErrKind::CollectionExists,
            Error::RecordExists(_) => ErrKind::RecordExists,
            Error::Schema(_) => ErrKind::Schema,
            _ => ErrKind::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    Unit,
    Fetched(Option<Record>),
    Existed(bool),
    Rows(Vec<(RecordId, Record)>),
    Count(usize),
    Failed(ErrKind),
}

/// Apply `op` to any `LogicalStore` and classify the result.
pub fn apply<S: LogicalStore>(store: &mut S, op: &Op) -> OpOutcome {
    fn wrap<T>(r: adabt_core::error::Result<T>, ok: impl FnOnce(T) -> OpOutcome) -> OpOutcome {
        match r {
            Ok(v) => ok(v),
            Err(e) => OpOutcome::Failed(ErrKind::from(&e)),
        }
    }
    match op {
        Op::Insert {
            collection,
            id,
            rec,
        } => wrap(store.insert(collection, *id, rec.clone()), |_| {
            OpOutcome::Unit
        }),
        Op::Get { collection, id } => wrap(store.get(collection, *id), OpOutcome::Fetched),
        Op::Update {
            collection,
            id,
            rec,
        } => wrap(
            store.update(collection, *id, rec.clone()),
            OpOutcome::Existed,
        ),
        Op::Delete { collection, id } => wrap(store.delete(collection, *id), OpOutcome::Existed),
        Op::Scan { collection } => wrap(store.scan(collection), OpOutcome::Rows),
        Op::Count { collection } => wrap(store.count(collection), OpOutcome::Count),
    }
}
