//! The heap representation: the first *primary* physical representation.
//!
//! A heap holds records in whatever page has room, addressed through a page
//! directory. It makes no promises about ordering or locality, which is what
//! makes it the right general-purpose default and the right thing for later,
//! more specialised representations to be compared against.
//!
//! # Durability
//!
//! Every mutation is written to the WAL and made durable per policy *before* it
//! touches a page. Pages are flushed lazily. A crash is repaired by replaying
//! the log from the last checkpoint, and replay is idempotent — an insert of an
//! id already present becomes an overwrite — so it does not matter how much of
//! the heap had already reached disk.
//!
//! # Recovery order
//!
//! Collection definitions come first — the persisted catalog, then whatever
//! definitions the log holds past it — because the page scan that rebuilds the
//! directory needs to know which collection each stored record belongs to.
//! Only then are data operations replayed.
//!
//! # Layout of this module
//!
//! The types and opening a store live here. Each child module adds one
//! responsibility to [`HeapStore`]: `recovery` (rebuilding state on open),
//! `write` (the logged physical write path), `placement` (which page a
//! payload lands on), `mvcc` (version chains and snapshot reads), `reclaim`
//! (returning space), `migration` (schema changes), `checkpoint` (durability,
//! backup and restore), `definitions` (index and clustering declarations),
//! `tuning` (runtime settings), `diagnostics` (read-only internals), and
//! `logical_store` (the [`LogicalStore`](adabt_core::store::LogicalStore)
//! contract).

use adabt_core::error::{Error, Result};
use adabt_core::ids::{CollectionId, Lsn, RecordId};
use adabt_core::policy::Durability;
use adabt_core::schema::Schema;
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
#[cfg(not(feature = "loom"))]
use std::sync::Arc;

use crate::codec::RecordCodec;
use crate::page::MAX_PAYLOAD;
use crate::pager::BufferPool;
use crate::version::VersionTracker;
use crate::wal::Wal;

mod checkpoint;
mod definitions;
mod diagnostics;
mod logical_store;
mod migration;
mod mvcc;
mod placement;
mod reclaim;
mod recovery;
mod tuning;
mod write;

use mvcc::VersionChain;
use placement::{ClusterState, FreeSpaceMap};

/// `collection_id: u32 | record_id: u64 | encoding: u8` prefixed to every stored
/// payload, so a page scan can rebuild the directory without consulting any
/// other structure.
///
/// The encoding byte lives *outside* the record, not in the record header's
/// reserved flags, because a compressed record's header cannot be read until it
/// has been decompressed. Keeping it per-record is what lets compressed and raw
/// records coexist, which in turn means enabling compression needs no migration
/// and disabling it needs no rewrite.
const SLOT_PREFIX: usize = 13;

/// Usable record size after the slot prefix.
pub const MAX_RECORD_BYTES: usize = MAX_PAYLOAD - SLOT_PREFIX;

struct Collection {
    id: CollectionId,
    codec: RecordCodec,
    directory: BTreeMap<RecordId, VersionChain>,
    /// The id an auto-allocated insert will use next.
    ///
    /// Persisted, and never reused even across a restart: a manual insert at id
    /// 100 pushes it past 100 too, and a deleted record's id is never handed
    /// out again, which is what lets a foreign key survive a restart without
    /// silently starting to point at something else.
    next_record_id: u64,
    /// Clustering state: which pages hold which key ranges. Present only once
    /// a keyed insert arrives; a collection nobody clusters is untouched by
    /// any of this.
    cluster: Option<ClusterState>,
    /// The declared clustering field's name, as logged via
    /// [`WalOp::SetClusterField`](crate::wal::WalOp::SetClusterField).
    /// Survives restarts through replay and the catalog; the *ranges* above
    /// do not (they re-derive from subsequent keyed inserts).
    cluster_name: Option<String>,
}

pub struct HeapStore {
    pool: BufferPool,
    wal: Wal,
    collections: HashMap<String, Collection>,
    by_id: HashMap<CollectionId, String>,
    next_collection_id: u32,
    fsm: FreeSpaceMap,
    dir: PathBuf,
    /// Whether new writes are compressed. Reads handle either encoding.
    compress: bool,
    /// Optimizer-controlled global flags persisted via the catalog.
    delta_encoding: bool,
    thread_per_core: bool,
    /// Hands out write ids and tracks which snapshots are still open.
    versions: Arc<VersionTracker>,
    /// Superseded versions currently retained, so reclamation can be triggered
    /// by pressure rather than by a timer.
    retained: usize,
    /// Index definitions, so they can be rebuilt after a restart without the
    /// caller having to remember what existed.
    index_defs: Vec<(String, String, String)>,
    replaying: bool,
    /// Distinguishes this database from another with an identical history.
    identity: u128,
    /// How far `recover` replayed the log. `Latest` outside a restore.
    recover_target: RecoverTarget,
    /// Pages touched by `get` since the last clear — the diagnostic a
    /// locality claim is measured against. See [`Self::touched_pages`].
    touched: std::collections::HashSet<u32>,
}

/// How far to replay the log when opening a store.
///
/// The ordinary case is `Latest` — everything the log holds. `Lsn` is what a
/// point-in-time restore is, mechanically: the exact same recovery passes
/// every other open runs, fed a log deliberately truncated to a chosen prefix
/// instead of one truncated by wherever a crash happened to land. Nothing
/// about *how* recovery behaves once handed a shorter log is new; only the
/// place the shortening comes from is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecoverTarget {
    #[default]
    Latest,
    Lsn(Lsn),
}

impl HeapStore {
    pub fn heap_path(dir: &Path) -> PathBuf {
        dir.join("heap.adabt")
    }
    /// The directory the log's segments live in.
    ///
    /// A directory rather than a file: the log is a sequence of bounded
    /// segments, so that everything below a checkpoint can be dropped and
    /// recovery need not read history it has already folded into pages.
    pub fn wal_path(dir: &Path) -> PathBuf {
        dir.join("wal")
    }

    /// Open (or create) a store, replaying the log to bring it up to date.
    pub fn open(dir: &Path, durability: Durability, pool_pages: usize) -> Result<Self> {
        Self::open_shared(dir, durability, pool_pages, Arc::new(VersionTracker::new()))
    }

    /// Open a store, replaying the log only up to `target` — a point-in-time
    /// restore. See [`RecoverTarget`] for what "up to" means and why it costs
    /// no new recovery logic.
    pub fn open_at(
        dir: &Path,
        durability: Durability,
        pool_pages: usize,
        target: RecoverTarget,
    ) -> Result<Self> {
        Self::open_shared_at(
            dir,
            durability,
            pool_pages,
            Arc::new(VersionTracker::new()),
            target,
        )
    }

    /// Open a store whose version timestamps come from a tracker shared with
    /// other stores, rather than one of its own.
    ///
    /// Every shard of a sharded database opens through this with the *same*
    /// tracker, so a `TxnId` stamped by shard 0 and one stamped by shard 1 order
    /// against each other exactly as if they had come from one store. Nothing
    /// reads a timestamp across shards, so this changes nothing observable;
    /// it costs one shared `Arc`, and it means a cross-shard reader could
    /// compare "when" two shards' writes happened without first reconciling
    /// two clocks.
    pub fn open_shared(
        dir: &Path,
        durability: Durability,
        pool_pages: usize,
        versions: Arc<VersionTracker>,
    ) -> Result<Self> {
        Self::open_shared_at(dir, durability, pool_pages, versions, RecoverTarget::Latest)
    }

    /// [`Self::open_shared`], restoring only up to `target`.
    pub fn open_shared_at(
        dir: &Path,
        durability: Durability,
        pool_pages: usize,
        versions: Arc<VersionTracker>,
        target: RecoverTarget,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        // Before anything else, and deliberately before a single page is read:
        // this establishes which database this is and whether this build may
        // read it. Opening the heap first would mean interpreting bytes whose
        // format has not yet been agreed.
        let superblock = crate::superblock::open_or_create(dir, crate::page::PAGE_SIZE as u32)?;
        let pool = BufferPool::open(&Self::heap_path(dir), pool_pages)?;
        // Opened before reading, so a torn tail on the active segment is
        // repaired first and recovery never sees a half-written entry.
        let wal = Wal::open(&Self::wal_path(dir), durability)?;

        let mut store = Self {
            pool,
            wal,
            collections: HashMap::new(),
            by_id: HashMap::new(),
            next_collection_id: 0,
            fsm: FreeSpaceMap::new(),
            dir: dir.to_path_buf(),
            compress: false,
            delta_encoding: true,
            thread_per_core: false,
            versions,
            retained: 0,
            index_defs: Vec::new(),
            replaying: true,
            identity: superblock.identity,
            recover_target: target,
            touched: std::collections::HashSet::new(),
        };
        store.recover()?;
        store.replaying = false;
        Ok(store)
    }

    fn register_collection(&mut self, name: &str, schema: Schema) -> CollectionId {
        let id = CollectionId(self.next_collection_id);
        self.next_collection_id += 1;
        self.register_collection_with_id(name, schema, id);
        id
    }

    fn register_collection_with_id(
        &mut self,
        name: &str,
        schema: Schema,
        id: CollectionId,
    ) -> CollectionId {
        // A collection re-registered by replay keeps the id it was given the
        // first time; handing out a fresh one would orphan its pages.
        self.next_collection_id = self.next_collection_id.max(id.0 + 1);
        self.collections.insert(
            name.to_string(),
            Collection {
                id,
                codec: RecordCodec::new(schema),
                directory: BTreeMap::new(),
                next_record_id: 0,
                cluster: None,
                cluster_name: None,
            },
        );
        self.by_id.insert(id, name.to_string());
        id
    }

    fn coll(&self, name: &str) -> Result<&Collection> {
        self.collections
            .get(name)
            .ok_or_else(|| Error::NoSuchCollection(name.to_string()))
    }

    /// The id an auto-allocated insert into `collection` will use next.
    ///
    /// A peek, not an allocation: nothing is reserved until a record is actually
    /// written with it. Safe because this crate offers no concurrent access to
    /// one store — the caller either inserts with this id next, in which case
    /// `apply_put` advances the counter past it, or does not, in which case
    /// nothing has changed.
    pub fn next_id(&self, collection: &str) -> Result<RecordId> {
        Ok(RecordId(self.coll(collection)?.next_record_id))
    }
}
