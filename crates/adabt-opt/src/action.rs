//! Physical actions.
//!
//! An optimization does not touch the engine. It *describes* what should change,
//! and something else carries it out.
//!
//! That indirection is what keeps `adabt-opt` free of any dependency on storage,
//! indexes or execution — check its `Cargo.toml`, there is nothing physical in
//! it. The payoff is that the same optimization definitions are usable by the
//! manual driver and, later, by the adaptive one, against any engine that can
//! interpret an `Action`. It also makes every change trivially inspectable
//! before it happens, which is what the decision log and dry-run rely on.

use adabt_core::index_kind::IndexKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    CreateIndex {
        collection: String,
        field: String,
        kind: IndexKind,
    },
    DropIndex {
        collection: String,
        field: String,
        kind: IndexKind,
    },
    /// Buffer pool size, in pages. The cleanest RAM-for-latency trade available.
    SetBufferPoolPages(usize),
    /// Plan cache capacity, in entries. Zero disables it.
    SetPlanCacheEntries(usize),
    /// Result cache capacity, in entries. Zero disables it.
    SetResultCacheEntries(usize),
    /// Compress stored records, trading CPU for storage, I/O and residency.
    SetRecordCompression(bool),
    /// Read ahead on sequential access.
    SetPrefetch(bool),
    /// Serve `GetById` from a directly-addressed array where the schema allows.
    SetDirectLookup(bool),
    /// Keep a columnar copy for scans and aggregates.
    SetColumnStore(bool),
    /// Keep grouped counts up to date on write, so an aggregate over them costs
    /// the number of groups rather than the number of rows.
    SetMaterializedViews(bool),
    /// Raise a collection's schema mode to the most rigid its data supports.
    ///
    /// The only action that takes something away from the user: a frozen
    /// collection rejects records the loose one accepted. Its `freedom` cost is
    /// real, not notional.
    FreezeSchema {
        collection: String,
    },
    /// Place records with nearby clustering keys on same pages.
    ///
    /// Subsequent inserts with integer keys near each other land on the same
    /// pages, so a range scan over the clustering field touches pages in
    /// proportion to the range, not the collection. Placement, not content —
    /// answers never change.
    SetClusterField {
        collection: String,
        field: String,
    },
    ClearClusterField {
        collection: String,
    },
    /// Enable delta-varint encoding for sorted integer columns.
    SetDeltaEncoding(bool),
    /// Enable per-core sharding of execution (thread-per-core).
    SetThreadPerCore(bool),
    /// Optimize join order based on collection cardinalities (M32).
    SetJoinOrder(bool),
    /// Data-driven partitioning of hot key ranges (M32).
    SetDataPartitioning(bool),
}

impl Action {
    /// Whether this change can be proved against live traffic before it is
    /// trusted.
    ///
    /// An experiment needs both answers available at the same moment: the old
    /// path to compare against, the new one to judge. That is only possible for
    /// changes that *add* a derived representation, which by the rebuildability
    /// invariant can sit unused beside the primary and be dropped for free.
    ///
    /// Everything else is excluded for a concrete reason, not a cautious one.
    /// Compression and schema freezing rewrite the primary, after which the old
    /// path no longer exists to compare against. Cache and buffer-pool sizes are
    /// single global numbers with no second value to hold simultaneously. A
    /// `Drop` has nothing to prove: removing a structure cannot return a wrong
    /// answer, only a slower one, and the ordinary measured-retraction path
    /// already covers that.
    pub fn is_shadowable(&self) -> bool {
        matches!(
            self,
            Action::CreateIndex { .. }
                | Action::SetColumnStore(true)
                | Action::SetDirectLookup(true)
                | Action::SetMaterializedViews(true)
                | Action::SetClusterField { .. }
                | Action::SetDeltaEncoding(true)
                | Action::SetJoinOrder(true)
                | Action::SetDataPartitioning(true)
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Action::CreateIndex {
                collection,
                field,
                kind,
            } => format!("create {} index on {collection}.{field}", kind.as_str()),
            Action::DropIndex {
                collection,
                field,
                kind,
            } => format!("drop {} index on {collection}.{field}", kind.as_str()),
            Action::SetBufferPoolPages(n) => format!("set buffer pool to {n} pages"),
            Action::SetPlanCacheEntries(n) => format!("set plan cache to {n} entries"),
            Action::SetResultCacheEntries(n) => format!("set result cache to {n} entries"),
            Action::SetRecordCompression(on) => format!("record compression {}", on_off(*on)),
            Action::SetPrefetch(on) => format!("prefetch {}", on_off(*on)),
            Action::SetDirectLookup(on) => format!("direct lookup {}", on_off(*on)),
            Action::SetColumnStore(on) => format!("column store {}", on_off(*on)),
            Action::SetMaterializedViews(on) => {
                format!("materialized views {}", on_off(*on))
            }
            Action::FreezeSchema { collection } => {
                format!("freeze the schema of {collection}")
            }
            Action::SetClusterField { collection, field } => {
                format!("cluster {collection} by {field}")
            }
            Action::ClearClusterField { collection } => format!("clear clustering of {collection}"),
            Action::SetDeltaEncoding(on) => format!("delta encoding {}", on_off(*on)),
            Action::SetThreadPerCore(on) => format!("thread-per-core {}", on_off(*on)),
            Action::SetJoinOrder(on) => format!("join order {}", on_off(*on)),
            Action::SetDataPartitioning(on) => format!("data partitioning {}", on_off(*on)),
        }
    }
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

/// A set of actions, with the actions that undo them.
///
/// Both directions are computed *before* anything is applied. An optimization
/// that cannot say how to undo itself has no business being applied
/// automatically, and working the inverse out afterwards — from an engine that
/// has already changed — is guesswork.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangePlan {
    pub apply: Vec<Action>,
    pub revert: Vec<Action>,
}

impl ChangePlan {
    pub fn new(apply: Vec<Action>, revert: Vec<Action>) -> Self {
        Self { apply, revert }
    }

    pub fn is_empty(&self) -> bool {
        self.apply.is_empty()
    }

    pub fn describe(&self) -> String {
        self.apply
            .iter()
            .map(|a| a.describe())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// What an engine must be able to do for the optimizer to drive it.
///
/// Deliberately narrow. Anything the optimizer cannot express as an `Action` it
/// cannot do, which bounds the blast radius of a wrong decision to this list.
pub trait ActionSink {
    fn apply_action(&mut self, action: &Action) -> adabt_core::error::Result<()>;

    /// Whether the action is possible right now. Used to reject a change before
    /// half of it has been applied.
    fn can_apply(&mut self, _action: &Action) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_describe_themselves_legibly() {
        let a = Action::CreateIndex {
            collection: "users".into(),
            field: "country".into(),
            kind: IndexKind::Hash,
        };
        assert_eq!(a.describe(), "create hash index on users.country");
        assert_eq!(
            Action::SetBufferPoolPages(512).describe(),
            "set buffer pool to 512 pages"
        );
        assert_eq!(Action::SetPrefetch(false).describe(), "prefetch off");
    }

    #[test]
    fn a_change_plan_describes_its_whole_apply_side() {
        let p = ChangePlan::new(
            vec![Action::SetPrefetch(true), Action::SetBufferPoolPages(64)],
            vec![Action::SetPrefetch(false), Action::SetBufferPoolPages(1024)],
        );
        assert!(p.describe().contains("prefetch on"));
        assert!(p.describe().contains("64 pages"));
        assert!(!p.is_empty());
    }

    #[test]
    fn an_empty_plan_is_recognised() {
        assert!(ChangePlan::default().is_empty());
    }
}
