use adabt_core::ids::RecordId;

/// A structural hash of a logical plan with literals erased.
///
/// This is the aggregation key for telemetry, the cache key for plans, the unit
/// of compilation and the trigger for materialization. It is introduced now,
/// well before the query IR exists, because retrofitting it later would touch
/// every layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct QueryShape(pub u64);

impl QueryShape {
    pub const UNKNOWN: QueryShape = QueryShape(0);
}

impl std::fmt::Display for QueryShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shape:{:016x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Get,
    Insert,
    Update,
    Delete,
    Scan,
    Count,
}

impl OpKind {
    pub fn is_write(self) -> bool {
        matches!(self, OpKind::Insert | OpKind::Update | OpKind::Delete)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Get => "get",
            OpKind::Insert => "insert",
            OpKind::Update => "update",
            OpKind::Delete => "delete",
            OpKind::Scan => "scan",
            OpKind::Count => "count",
        }
    }

    pub const ALL: [OpKind; 6] = [
        OpKind::Get,
        OpKind::Insert,
        OpKind::Update,
        OpKind::Delete,
        OpKind::Scan,
        OpKind::Count,
    ];
}

#[derive(Debug, Clone)]
pub enum Event<'a> {
    /// One logical operation completed.
    Op {
        collection: &'a str,
        kind: OpKind,
        shape: QueryShape,
        nanos: u64,
        rows: u64,
    },
    /// A record was touched. Feeds data-temperature estimation.
    Touch { collection: &'a str, id: RecordId },
    /// A cache was consulted.
    CacheProbe { name: &'static str, hit: bool },
    /// The planner chose an index. The evidence for *dropping* one: an index
    /// that is never chosen costs write maintenance and memory for nothing,
    /// and no amount of watching queries arrive would reveal that.
    IndexUsed { collection: &'a str, field: &'a str },
    /// A query filtered on a field. The evidence `auto_index` needs.
    ///
    /// Recorded here rather than tracked separately in the engine so that
    /// everything the adaptive driver keys on comes from one place, with one
    /// cost model and one on/off switch.
    /// A query filtered on a field, and how.
    ///
    /// `equality` matters because the two want different structures: a hash
    /// index serves equality and cannot answer a range at all. Recording only
    /// "this field was filtered" is what let the driver build hash indexes for
    /// range predicates and pay for them forever.
    FieldFiltered {
        collection: &'a str,
        field: &'a str,
        equality: bool,
    },
    /// An index was maintained on the write path — one entry inserted or
    /// removed because a record changed.
    ///
    /// **The cost half of the retraction decision.** `IndexUsed` measures
    /// what an index is worth; without this, an optimizer weighing whether
    /// to keep one can only see the benefit and has to treat the price as
    /// either zero or a guess. Recorded at the exact point the price is
    /// actually paid, so it counts real work rather than an estimate of it.
    IndexMaintained { collection: &'a str, field: &'a str },
    /// An optimization changed state.
    OptChanged { name: &'static str, enabled: bool },
}
