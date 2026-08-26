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
//! Collection definitions are replayed from the whole log first, because the
//! page scan that rebuilds the directory needs to know which collection each
//! stored record belongs to. Only then are data operations replayed.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{CollectionId, Lsn, RecordId, TxnId};
use adabt_core::policy::Durability;
use adabt_core::record::Record;
use adabt_core::schema::Schema;
use adabt_core::store::{normalize_for_storage, LogicalStore};
use adabt_core::value::Value;
#[cfg(feature = "loom")]
use loom::sync::Arc;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
#[cfg(not(feature = "loom"))]
use std::sync::Arc;

use crate::catalog::{decode_schema, encode_schema};
use crate::codec::RecordCodec;
use crate::page::{Page, PageId, RecordLocation, SlotId, MAX_PAYLOAD};
use crate::pager::{BufferPool, BufferStats};
use crate::version::{Snapshot, VersionTracker};
use crate::wal::{Wal, WalOp};

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

/// Every version of one record, oldest first.
///
/// `None` marks a delete: a reader whose snapshot falls after it must see the
/// record as absent rather than seeing the version before it. Recording the
/// deletion as a version rather than removing the entry is what makes that
/// possible.
#[derive(Debug, Default, Clone)]
struct VersionChain {
    versions: Vec<(TxnId, Option<RecordLocation>)>,
}

impl VersionChain {
    fn newest(&self) -> Option<RecordLocation> {
        self.versions.last().and_then(|(_, loc)| *loc)
    }

    /// The stamp of the newest version, whether it is a value or a tombstone.
    ///
    /// Never reclaimed away: the newest entry is always kept, so this is safe to
    /// call at any time and answers "when was this key last touched at all,"
    /// which is exactly what first-committer-wins conflict detection needs —
    /// distinct from `newest`, which answers "what does this key hold now" and
    /// says nothing once the newest entry is a delete.
    fn newest_stamp(&self) -> Option<TxnId> {
        self.versions.last().map(|(txn, _)| *txn)
    }

    /// The version a snapshot at `at` should see.
    fn visible_at(&self, at: TxnId) -> Option<RecordLocation> {
        self.versions
            .iter()
            .rev()
            .find(|(txn, _)| txn.0 <= at.0)
            .and_then(|(_, loc)| *loc)
    }

    fn push(&mut self, txn: TxnId, loc: Option<RecordLocation>) {
        self.versions.push((txn, loc));
    }

    /// Drop versions no snapshot can reach, returning the freed locations.
    ///
    /// Keeps the newest version at or before the horizon: that one is still the
    /// answer for any reader at the horizon, and dropping it would make the
    /// record vanish.
    fn reclaim_to(&mut self, horizon: TxnId) -> Vec<RecordLocation> {
        let keep_from = match self
            .versions
            .iter()
            .rposition(|(txn, _)| txn.0 <= horizon.0)
        {
            Some(i) => i,
            None => return Vec::new(),
        };
        let freed: Vec<RecordLocation> = self.versions[..keep_from]
            .iter()
            .filter_map(|(_, loc)| *loc)
            .collect();
        self.versions.drain(..keep_from);
        freed
    }

    fn len(&self) -> usize {
        self.versions.len()
    }

    fn is_absent(&self) -> bool {
        self.newest().is_none()
    }
}

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
    /// [`WalOp::SetClusterField`]. Survives restarts through replay and the
    /// catalog; the *ranges* above do not (they re-derive from subsequent
    /// keyed inserts).
    cluster_name: Option<String>,
}

/// The clustering hint's bookkeeping: the integer key range each page was
/// filled under.
///
/// This is what turns a keyed range scan from random page touches into a run
/// of consecutive ones — records whose keys are near each other are *placed*
/// on the same pages, so the ids an index range returns land on few pages.
/// Ranges only ever widen after the fact (deletes leave holes, updates may
/// move a record), which costs some pruning precision but never correctness:
/// the directory, not these ranges, is where truth about location lives.
#[derive(Default)]
struct ClusterState {
    ranges: HashMap<PageId, (i64, i64)>,
}

/// Prefix marking a collection as a migration's staging area.
///
/// A leading NUL cannot appear in a name the logical API accepts, so a staging
/// collection can never collide with a user's — and a user can never address
/// one, which is why these are filtered out of `collection_names`.
const MIGRATION_PREFIX: &str = "\u{0}migrating:";

fn migration_name(collection: &str) -> String {
    format!("{MIGRATION_PREFIX}{collection}")
}

fn is_migration_name(name: &str) -> bool {
    name.starts_with(MIGRATION_PREFIX)
}

/// Recursively copy every file under `src` into `dest`, creating `dest` if it
/// does not exist. Used for backup, where the log is a directory of segments
/// rather than a single file.
fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

/// Keep only the fields `schema` still recognizes.
///
/// A no-op whenever `schema`'s mode allows extra fields — `Declared` and
/// `Dynamic` already carry an undeclared field through to the encoded
/// overflow bag, which is the existing, correct behaviour for those modes.
/// For `Strict` and `Fixed`, which forbid it, this is what makes "drop field
/// X" mean "the row no longer has X" instead of "the row fails validation."
fn project_onto_schema(schema: &Schema, rec: &Record) -> Record {
    if schema.mode().allows_extra_fields() {
        return rec.clone();
    }
    let mut out = Record::new();
    for (name, v) in rec.iter() {
        if schema.field(name).is_some() {
            out.set(name.to_string(), v.clone());
        }
    }
    out
}

/// Pages bucketed by how much free space they have.
///
/// A linear scan for a page with room is O(pages) per insert, which dominates
/// everything else once a heap grows. Buckets make it near-constant while
/// staying simple enough to be obviously correct.
#[derive(Default)]
struct FreeSpaceMap {
    free: HashMap<PageId, usize>,
    buckets: Vec<Vec<PageId>>,
}

const BUCKET_COUNT: usize = 8;
const BUCKET_SPAN: usize = crate::page::PAGE_SIZE / BUCKET_COUNT;

impl FreeSpaceMap {
    fn new() -> Self {
        Self {
            free: HashMap::new(),
            buckets: vec![Vec::new(); BUCKET_COUNT],
        }
    }

    fn class_of(free: usize) -> usize {
        (free / BUCKET_SPAN).min(BUCKET_COUNT - 1)
    }

    fn set(&mut self, page: PageId, free: usize) {
        if let Some(old) = self.free.insert(page, free) {
            let b = &mut self.buckets[Self::class_of(old)];
            if let Some(i) = b.iter().position(|p| *p == page) {
                b.swap_remove(i);
            }
        }
        self.buckets[Self::class_of(free)].push(page);
    }

    /// Forget a page entirely, so it is never offered again.
    fn forget(&mut self, page: PageId) {
        if let Some(old) = self.free.remove(&page) {
            let b = &mut self.buckets[Self::class_of(old)];
            if let Some(i) = b.iter().position(|p| *p == page) {
                b.swap_remove(i);
            }
        }
    }

    /// A page with at least `need` bytes free, if one is known.
    fn find(&self, need: usize) -> Option<PageId> {
        // Only the top of the class containing `need` is guaranteed to fit, so
        // search that class candidate-by-candidate and higher classes freely.
        for class in Self::class_of(need)..BUCKET_COUNT {
            for &p in &self.buckets[class] {
                if self.free.get(&p).copied().unwrap_or(0) >= need {
                    return Some(p);
                }
            }
        }
        None
    }
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
    /// Approximate live row count without taking `&mut`.
    ///
    /// Used by the planner's calibrated cost model: an index that matches a
    /// large fraction of the collection can be more expensive than a scan, but
    /// that comparison needs both `rows` and `distinct`. This is the `rows`
    /// half, and keeping it `&self` lets `plan_context` be `&self`.
    pub fn live_count(&self, collection: &str) -> Option<u64> {
        let c = self.collections.get(collection)?;
        Some(c.directory.values().filter(|v| !v.is_absent()).count() as u64)
    }
    pub fn delta_encoding(&self) -> bool {
        self.delta_encoding
    }
    pub fn thread_per_core(&self) -> bool {
        self.thread_per_core
    }
    pub fn set_delta_encoding(&mut self, v: bool) {
        self.delta_encoding = v;
    }
    pub fn set_thread_per_core(&mut self, v: bool) {
        self.thread_per_core = v;
    }

    pub fn heap_path(dir: &Path) -> PathBuf {
        dir.join("heap.adabt")
    }
    /// The directory the log's segments live in.
    ///
    /// A directory rather than a file since M17: the log is now a sequence of
    /// bounded segments, so that everything below a checkpoint can be dropped
    /// and recovery need not read history it has already folded into pages.
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
    /// today reads a timestamp across shards, so this changes nothing observable
    /// yet — but retrofitting it once shards hold real data would mean
    /// rewriting every timestamp already on disk, and doing it now costs one
    /// shared `Arc` instead.
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

    fn recover(&mut self) -> Result<()> {
        // Pass 0: the persisted catalog, if there is one.
        //
        // It supplies the name-to-id binding directly rather than letting it
        // fall out of replay order. That matters because a collection's id is
        // embedded in every one of its heap slots: re-deriving the binding from
        // a log that has lost its beginning silently reattributes every page.
        let catalog = crate::metadata::read(&self.dir, self.identity);
        let from_lsn = match &catalog {
            Some(cat) => {
                self.adopt_catalog(cat)?;
                cat.through_lsn
            }
            None => {
                // No catalog. Sound only while the log still holds everything.
                let start = self.wal.start_lsn();
                if start.0 > 0 {
                    return Err(crate::metadata::missing(start));
                }
                0
            }
        };

        // A restore target below what the catalog already reflects is not an
        // earlier state — the catalog Pass 0 just adopted already contains
        // writes past it, unconditionally. There is no way to "replay less"
        // out of that; the only fix is a backup taken before this one's own
        // checkpoint.
        if let RecoverTarget::Lsn(target) = self.recover_target {
            if target.0 < from_lsn {
                return Err(Error::RestoreTargetUnreachable {
                    requested: target.0,
                    earliest: from_lsn,
                });
            }
        }

        // Read once, from the earliest point any pass still needs.
        //
        // That point is the catalog's `through_lsn`, because the catalog and the
        // checkpoint entry are written by the same call: everything at or below
        // it is already in the pages and in the catalog. Below it the segments
        // are never opened, which is what makes opening a database cost what has
        // happened lately rather than everything that has ever happened.
        //
        // If the catalog is older than the last checkpoint — a crash between the
        // two — this reads slightly more than it needs to, which is the correct
        // direction to be wrong in.
        let mut entries = crate::wal::Wal::entries_from(&Self::wal_path(&self.dir), Lsn(from_lsn))?;
        // A point-in-time restore is recovery fed a deliberately short log
        // instead of an accidentally short one — everything below already
        // knows how to stop wherever the log stops.
        if let RecoverTarget::Lsn(target) = self.recover_target {
            entries.retain(|e| e.lsn.0 <= target.0);
        }
        let entries = &entries[..];

        // Pass 1: collection definitions, for whatever the catalog did not
        // already cover. The page scan below cannot interpret a stored record
        // without knowing its schema.
        for e in entries {
            if e.lsn.0 <= from_lsn {
                continue;
            }
            match &e.op {
                WalOp::CreateCollection { name, schema } => {
                    let schema = decode_schema(schema)?;
                    self.register_collection(name, schema);
                }
                // Deliberately *not* applied here. An adoption moves records
                // that only exist once the log has been replayed, so it is
                // collected in order and executed in pass 4.
                WalOp::AdoptMigration { .. } => {}
                // Unlike an adoption this touches no record, so — unlike an
                // adoption — there is no reason to defer it: replaying it here,
                // in log order alongside the `CreateCollection` it follows, is
                // exactly what the original call did.
                WalOp::AlterSchemaInPlace { collection, schema } => {
                    let schema = decode_schema(schema)?;
                    self.apply_alter_schema_in_place(collection, schema)?;
                }
                WalOp::CreateIndex {
                    collection,
                    field,
                    kind,
                } => {
                    let def = (collection.clone(), field.clone(), kind.clone());
                    if !store_has(&self.index_defs, &def) {
                        self.index_defs.push(def);
                    }
                }
                WalOp::SetClusterField { collection, field } => {
                    if let Some(c) = self.collections.get_mut(collection.as_str()) {
                        c.cluster_name = field.clone();
                    }
                    // A declaration for a collection later dropped replays as
                    // nothing — same rule as every other per-collection op.
                }
                WalOp::DropIndex {
                    collection,
                    field,
                    kind,
                } => {
                    self.index_defs
                        .retain(|(c, f, k)| !(c == collection && f == field && k == kind));
                }
                WalOp::DropCollection { name } => {
                    if let Some(c) = self.collections.remove(name) {
                        self.by_id.remove(&c.id);
                    }
                }
                _ => {}
            }
        }

        // Pass 2: the page directory and free-space map, as of the last
        // checkpoint. Loaded from the cache when one describes that exact
        // checkpoint, and rebuilt by reading every page when it does not.
        //
        // Both produce the same directory. The scan is the definition and the
        // cache is an optimization of it, which is why a cache that cannot be
        // trusted for any reason costs a scan rather than an error.
        let flushed_lsn = entries
            .iter()
            .rev()
            .find_map(|e| match e.op {
                WalOp::Checkpoint { flushed_lsn } => Some(flushed_lsn),
                _ => None,
            })
            .unwrap_or(Lsn(0));
        if !self.load_directory(flushed_lsn)? {
            self.scan_pages()?;
        }

        // Pass 3: replay everything the heap file might not yet contain.
        for e in entries {
            if e.lsn.0 <= flushed_lsn.0 {
                continue;
            }
            match &e.op {
                WalOp::Insert {
                    collection,
                    id,
                    bytes,
                }
                | WalOp::Update {
                    collection,
                    id,
                    bytes,
                } => {
                    // Replay is idempotent by construction: an insert of an id
                    // already present is applied as an overwrite, so it does not
                    // matter how far the heap had got before the crash.
                    self.apply_put(collection, *id, bytes)?;
                }
                WalOp::Delete { collection, id } => {
                    self.apply_delete(collection, *id)?;
                }
                _ => {}
            }
        }
        // Pass 4: schema migrations that reached their commit entry, in log
        // order. Deferred to here because an adoption hands over records, and
        // the records only exist once passes 2 and 3 have placed them.
        for e in entries {
            if let WalOp::AdoptMigration { target, source } = &e.op {
                self.adopt(target, source)?;
            }
        }
        // Anything still wearing a migration name belongs to a change that was
        // cut short. Its target is untouched and correct, so this is waste, not
        // state — and leaving it would let the next attempt inherit a partial
        // copy of the collection it is trying to replace.
        for name in self
            .collections
            .keys()
            .filter(|n| is_migration_name(n))
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Some(c) = self.collections.remove(&name) {
                self.by_id.remove(&c.id);
                self.free_pages_of(&c)?;
            }
        }

        // Replay appends a version onto chains the page scan already built. No
        // snapshot can span a restart, so collapsing them is both safe and
        // necessary: otherwise every restart doubles the retained history.
        self.reclaim()?;
        self.retained = 0;
        Ok(())
    }

    /// Seed the collection tables from the persisted catalog.
    ///
    /// Ids come from the file rather than from a counter, so a page written
    /// under `CollectionId(7)` is still read as belonging to that collection
    /// however much of the log has since been discarded.
    fn adopt_catalog(&mut self, cat: &crate::metadata::Catalog) -> Result<()> {
        for c in &cat.collections {
            let schema = decode_schema(&c.schema)?;
            self.register_collection_with_id(&c.name, schema, CollectionId(c.id));
            // The counter comes from the catalog rather than being left to
            // rebuild from whatever pass 3 replays: a record inserted, then
            // deleted, before the checkpoint must still push the sequence past
            // its id, and no surviving page says that it ever existed.
            if let Some(coll) = self.collections.get_mut(&c.name) {
                coll.next_record_id = coll.next_record_id.max(c.next_record_id);
            }
        }
        self.next_collection_id = cat.next_collection_id;
        self.delta_encoding = cat.delta_encoding;
        self.thread_per_core = cat.thread_per_core;
        self.index_defs = cat
            .indexes
            .iter()
            .map(|i| (i.collection.clone(), i.field.clone(), i.kind.clone()))
            .collect();
        Ok(())
    }

    /// The catalog as it stands, for persisting at a checkpoint.
    fn catalog_snapshot(&self, through_lsn: u64) -> crate::metadata::Catalog {
        crate::metadata::Catalog {
            collections: self
                .collections
                .iter()
                .map(|(name, c)| crate::metadata::CollectionMeta {
                    name: name.clone(),
                    id: c.id.0,
                    next_record_id: c.next_record_id,
                    schema: encode_schema(c.codec.schema()),
                    cluster_field: c.cluster_name.clone(),
                })
                .collect(),
            indexes: self
                .index_defs
                .iter()
                .map(|(c, f, k)| crate::metadata::IndexMeta {
                    collection: c.clone(),
                    field: f.clone(),
                    kind: k.clone(),
                })
                .collect(),
            next_collection_id: self.next_collection_id,
            through_lsn,
            log_start_lsn: self.wal.start_lsn().0,
            delta_encoding: self.delta_encoding,
            thread_per_core: self.thread_per_core,
        }
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

    /// The stamp a cached directory must match to be usable.
    ///
    /// Everything in it is available before a single page has been read, which
    /// is the constraint: this cache replaces a step of recovery, so it cannot
    /// be validated against anything recovery produces.
    fn directory_stamp(&self, flushed_lsn: Lsn) -> crate::directory::Stamp {
        crate::directory::Stamp {
            identity: self.identity,
            checkpoint_lsn: flushed_lsn.0,
            heap_bytes: std::fs::metadata(Self::heap_path(&self.dir))
                .map(|m| m.len())
                .unwrap_or(0),
        }
    }

    /// Load the directory as of the last checkpoint. Returns whether it worked.
    fn load_directory(&mut self, flushed_lsn: Lsn) -> Result<bool> {
        // A database that has never checkpointed has nothing to load: the heap
        // may hold anything and only the log knows what.
        if flushed_lsn.0 == 0 {
            return Ok(false);
        }
        let stamp = self.directory_stamp(flushed_lsn);
        let Some(snapshot) = crate::directory::read(&self.dir, &stamp) else {
            return Ok(false);
        };
        for (cid, records) in snapshot.collections {
            // Records go back under the collection *id* their slots carry, which
            // is what a page scan would have done. A collection the log no
            // longer defines was dropped after the checkpoint; replay will not
            // resurrect it and neither should this.
            let Some(name) = self.by_id.get(&CollectionId(cid)).cloned() else {
                continue;
            };
            let Some(c) = self.collections.get_mut(&name) else {
                continue;
            };
            for (id, loc) in records {
                let mut chain = VersionChain::default();
                chain.push(TxnId(0), Some(loc));
                c.directory.insert(id, chain);
            }
        }
        for (page, free) in snapshot.free_space {
            self.fsm.set(page, free as usize);
        }
        Ok(true)
    }

    /// The directory as it stands, in the form the cache stores.
    fn directory_snapshot(&self) -> crate::directory::Snapshot {
        crate::directory::Snapshot {
            collections: self
                .collections
                .values()
                .map(|c| {
                    let records = c
                        .directory
                        .iter()
                        .filter_map(|(id, chain)| chain.newest().map(|loc| (*id, loc)))
                        .collect();
                    (c.id.0, records)
                })
                .collect(),
            free_space: self
                .fsm
                .free
                .iter()
                .map(|(p, free)| (*p, *free as u32))
                .collect(),
        }
    }

    fn scan_pages(&mut self) -> Result<()> {
        let count = self.pool.page_count();
        for i in 0..count {
            let pid = PageId(i);
            let page = self.pool.get(pid)?;
            let free = page.free_space();
            let mut found: Vec<(CollectionId, RecordId, SlotId)> = Vec::new();
            for slot in page.slots() {
                let payload = page.get(slot)?;
                if payload.len() < SLOT_PREFIX {
                    return Err(Error::Corruption(format!(
                        "slot {} on page {i} is shorter than its prefix",
                        slot.0
                    )));
                }
                let cid = CollectionId(u32::from_le_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
                let mut rid = [0u8; 8];
                rid.copy_from_slice(&payload[4..12]);
                found.push((cid, RecordId(u64::from_le_bytes(rid)), slot));
            }
            for (cid, rid, slot) in found {
                // A record whose collection was dropped is an orphan; leave it
                // in place rather than pretending it belongs somewhere.
                if let Some(name) = self.by_id.get(&cid).cloned() {
                    if let Some(c) = self.collections.get_mut(&name) {
                        let mut chain = VersionChain::default();
                        chain.push(TxnId(0), Some(RecordLocation { page: pid, slot }));
                        c.directory.insert(rid, chain);
                    }
                }
            }
            self.fsm.set(pid, free);
        }
        Ok(())
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

    // -- physical operations, all idempotent ------------------------------

    fn build_payload(cid: CollectionId, id: RecordId, bytes: &[u8], compress: bool) -> Vec<u8> {
        let (encoding, body) = if compress {
            crate::compress::maybe_compress(bytes)
        } else {
            (crate::compress::Encoding::Raw, bytes.to_vec())
        };
        let mut payload = Vec::with_capacity(SLOT_PREFIX + body.len());
        payload.extend_from_slice(&cid.0.to_le_bytes());
        payload.extend_from_slice(&id.0.to_le_bytes());
        payload.push(encoding.bit());
        payload.extend_from_slice(&body);
        payload
    }

    /// Insert or overwrite. The single physical write path, so insert, update
    /// and replay cannot drift apart.
    /// Write a new version. Never updates in place: a superseded version may
    /// still be the answer for an open snapshot, and overwriting it would move
    /// the ground under a reader mid-scan.
    fn apply_put(&mut self, collection: &str, id: RecordId, bytes: &[u8]) -> Result<()> {
        let Some(c) = self.collections.get(collection) else {
            // Replay of an operation against a collection later dropped.
            return Ok(());
        };
        let cid = c.id;
        let payload = Self::build_payload(cid, id, bytes, self.compress);
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Corruption(format!(
                "record of {} bytes exceeds the {MAX_RECORD_BYTES}-byte page limit",
                bytes.len()
            )));
        }

        let txn = self.versions.begin_write();
        let loc = self.place(&payload)?;
        let c = self.collections.get_mut(collection).expect("checked above");
        let chain = c.directory.entry(id).or_default();
        let superseded = !chain.versions.is_empty();
        chain.push(txn, Some(loc));
        // Every write through here — manual or auto-allocated, live or
        // replayed — pushes the sequence past whatever id it just used. This is
        // the one physical write path, so there is nowhere else an id could
        // enter the collection without this running.
        c.next_record_id = c.next_record_id.max(id.0 + 1);
        if superseded {
            self.retained += 1;
        }
        self.maybe_reclaim()?;
        Ok(())
    }

    /// Insert or overwrite with a clustering key: same durability and
    /// directory work as [`Self::apply_put`], placement chosen for locality.
    fn apply_put_keyed(
        &mut self,
        collection: &str,
        id: RecordId,
        bytes: &[u8],
        key: i64,
    ) -> Result<()> {
        let Some(c) = self.collections.get(collection) else {
            return Ok(());
        };
        let cid = c.id;
        let payload = Self::build_payload(cid, id, bytes, self.compress);
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Corruption(format!(
                "record of {} bytes exceeds the {MAX_RECORD_BYTES}-byte page limit",
                bytes.len()
            )));
        }

        let txn = self.versions.begin_write();
        let loc = self.place_keyed(collection, &payload, key)?;
        let c = self.collections.get_mut(collection).expect("checked above");
        let chain = c.directory.entry(id).or_default();
        let superseded = !chain.versions.is_empty();
        chain.push(txn, Some(loc));
        c.next_record_id = c.next_record_id.max(id.0 + 1);
        if superseded {
            self.retained += 1;
        }
        self.maybe_reclaim()?;
        Ok(())
    }

    fn place(&mut self, payload: &[u8]) -> Result<RecordLocation> {
        let need = Page::cost_of(payload.len());
        if let Some(pid) = self.fsm.find(need) {
            let page = self.pool.get_mut(pid)?;
            if page.can_fit(payload.len()) {
                let slot = page.insert(payload)?;
                let free = page.free_space();
                self.fsm.set(pid, free);
                return Ok(RecordLocation { page: pid, slot });
            }
            // The map was stale; correct it and fall through to a new page.
            let free = page.free_space();
            self.fsm.set(pid, free);
        }
        let pid = self.pool.allocate()?;
        let page = self.pool.get_mut(pid)?;
        let slot = page.insert(payload)?;
        let free = page.free_space();
        self.fsm.set(pid, free);
        Ok(RecordLocation { page: pid, slot })
    }

    /// Place a payload under the clustering hint, preferring a page whose key
    /// range already contains `key`.
    ///
    /// The policy in one line: containing pages first (nearest midpoint as tie
    /// break), then the adjacent range with room, then a fresh page. It is
    /// deliberately not a B-tree split — the goal is *locality*, and locality
    /// does not need perfect ordering. What it needs is that a contiguous key
    /// span maps to a small set of pages, which "put it where its neighbours
    /// are" achieves.
    ///
    /// Pages fill left to right along the key domain; when every candidate is
    /// full, a new page extends the run. Deletes leave holes and widen no
    /// ranges, so precision decays only as far as fragmentation does.
    fn place_keyed(
        &mut self,
        collection: &str,
        payload: &[u8],
        key: i64,
    ) -> Result<RecordLocation> {
        let c = self
            .collections
            .get_mut(collection)
            .expect("checked by caller");
        let cluster = c.cluster.get_or_insert_with(ClusterState::default);

        // Candidates are pages whose range is within `slack` of the key,
        // ordered by distance then tightness. The slack is the load-adaptive
        // part: roughly twice one page's share of the observed key domain.
        // It replaces two policies that each fail informatively:
        //
        // - nearest-at-any-distance lets the long-lived wide pages hoard
        //   every insert (the tight ones fill first, the wide ones survive
        //   to absorb whatever remains, and their ranges balloon until
        //   placement is noise);
        // - strict containment refuses to fill gaps, and random-order keys
        //   land between existing ranges often enough to give one row per
        //   page.
        //
        // With a window tied to the current page count, a page stops
        // stretching once its neighbourhood has grown a fair share of the
        // domain, and gaps between ranges still spawn fresh pages rather
        // than stretching old ones past their share.
        let mut lo_min = i64::MAX;
        let mut hi_max = i64::MIN;
        let mut candidates: Vec<(i64, i64, PageId)> = Vec::with_capacity(cluster.ranges.len());
        for (&pid, &(lo, hi)) in &cluster.ranges {
            lo_min = lo_min.min(lo);
            hi_max = hi_max.max(hi);
            let dist = if key < lo {
                lo - key
            } else if key > hi {
                key - hi
            } else {
                0
            };
            candidates.push((dist, hi - lo, pid));
        }
        let pages = candidates.len() as i64;
        let slack = if pages == 0 {
            0
        } else {
            let extent = hi_max.saturating_sub(lo_min);
            (extent / pages * 2).max(1024)
        };
        candidates.retain(|&(dist, _, _)| dist <= slack);
        candidates.sort();
        for (_, _, pid) in candidates {
            if let Ok(page) = self.pool.get_mut(pid) {
                if page.can_fit(payload.len()) {
                    let slot = page.insert(payload)?;
                    // The range stays an honest record of the page's contents:
                    // it widens only to swallow a key it genuinely took.
                    if let Some(r) = cluster.ranges.get_mut(&pid) {
                        r.0 = r.0.min(key);
                        r.1 = r.1.max(key);
                    }
                    self.fsm.set(pid, page.free_space());
                    return Ok(RecordLocation { page: pid, slot });
                }
            }
        }
        // No tracked page both contains the key and has room: start a fresh
        // one. It becomes the tightest range claiming this key, so its
        // neighbourhood settles onto it as it fills.
        let pid = self.pool.allocate()?;
        let page = self.pool.get_mut(pid)?;
        let slot = page.insert(payload)?;
        let free = page.free_space();
        self.fsm.set(pid, free);
        cluster.ranges.insert(pid, (key, key));
        Ok(RecordLocation { page: pid, slot })
    }

    fn apply_delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let txn = self.versions.begin_write();
        let Some(c) = self.collections.get_mut(collection) else {
            return Ok(false);
        };
        let Some(chain) = c.directory.get_mut(&id) else {
            return Ok(false);
        };
        if chain.is_absent() {
            return Ok(false);
        }
        // A tombstone rather than a removal: a reader whose snapshot predates
        // the delete must still find the record.
        chain.push(txn, None);
        self.retained += 1;
        self.maybe_reclaim()?;
        Ok(true)
    }

    fn log(&mut self, op: WalOp) -> Result<()> {
        if self.replaying {
            return Ok(());
        }
        self.wal.append(TxnId(0), op)?;
        self.wal.commit()
    }

    /// Append without making it durable yet. Paired with a single
    /// [`Wal::commit`] after every entry in a batch, so `Strict` pays for one
    /// fsync instead of one per row.
    fn log_uncommitted(&mut self, op: WalOp) -> Result<()> {
        if self.replaying {
            return Ok(());
        }
        self.wal.append(TxnId(0), op)?;
        Ok(())
    }

    /// Insert many records into `collection` in one call.
    ///
    /// **All-or-nothing, and one fsync rather than one per row.** Every record
    /// is normalised and validated, and every id checked against both the
    /// existing directory and the rest of the batch, before anything is
    /// written — a batch containing one bad record inserts nothing, exactly as
    /// calling `insert` in a loop and stopping at the first error would leave
    /// the earlier calls in place, except here there are no earlier calls to
    /// leave in place.
    ///
    /// The durability saving is the point: under `Strict`, inserting a hundred
    /// thousand rows one at a time is a hundred thousand fsyncs. Loading a
    /// dataset is a first-hour activity, and paying that cost per row rather
    /// than per batch is not a trade anyone would choose on purpose.
    pub fn insert_batch(
        &mut self,
        collection: &str,
        records: Vec<(RecordId, Record)>,
    ) -> Result<usize> {
        let mut seen = std::collections::HashSet::with_capacity(records.len());
        let mut prepared = Vec::with_capacity(records.len());
        for (id, mut rec) in records {
            normalize_for_storage(&mut rec);
            let c = self.coll(collection)?;
            c.codec.schema().validate_record(&rec)?;
            if c.directory.get(&id).is_some_and(|v| !v.is_absent()) {
                return Err(Error::RecordExists(id));
            }
            if !seen.insert(id) {
                return Err(Error::RecordExists(id));
            }
            let bytes = c.codec.encode(&rec)?;
            prepared.push((id, bytes));
        }

        for (id, bytes) in &prepared {
            self.log_uncommitted(WalOp::Insert {
                collection: collection.to_string(),
                id: *id,
                bytes: bytes.clone(),
            })?;
        }
        if !self.replaying {
            self.wal.commit()?;
        }
        for (id, bytes) in &prepared {
            self.apply_put(collection, *id, bytes)?;
        }
        Ok(prepared.len())
    }

    // -- maintenance -------------------------------------------------------

    /// Flush every dirty page, fsync, and record the point replay may start from.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pool.checkpoint()?;
        // Ordering matters: the pages must be on disk before the log claims they
        // are, or a crash between the two would skip replaying live changes.
        let flushed = Lsn(self.wal.next_lsn().0.saturating_sub(1));
        self.wal.append(
            TxnId(0),
            WalOp::Checkpoint {
                flushed_lsn: flushed,
            },
        )?;
        self.wal.sync()?;
        // The directory describing the heap this checkpoint just left behind.
        // Written after the log entry, so its stamp names the checkpoint it
        // belongs to. A failure here is not a failure of the checkpoint — the
        // database is durable either way and a missing cache costs a scan — so
        // the stale file is removed rather than the error propagated.
        let flushed_lsn = flushed;
        let stamp = self.directory_stamp(flushed);
        if crate::directory::write(&self.dir, &stamp, &self.directory_snapshot()).is_err() {
            crate::directory::discard(&self.dir);
        }
        // The catalog, unlike the directory, is authoritative: a failure to
        // write it is propagated rather than swallowed. Discarding it and
        // carrying on would leave a database whose name-to-id binding exists
        // only in a log that is about to become discardable.
        let catalog = self.catalog_snapshot(flushed.0);
        crate::metadata::write(&self.dir, self.identity, &catalog)?;

        // Only now. Everything below this point is recorded in pages that have
        // been flushed and in a catalog that has been fsynced, so the segments
        // holding it are redundant. Discarding any earlier would remove the only
        // remaining record of which collection each page belongs to.
        self.wal.discard_below(flushed_lsn)?;
        // The catalog names the log's new lower bound, so it is rewritten once
        // the bound has actually moved. A catalog claiming a complete log over a
        // truncated one would let a later open silently rebuild from a log that
        // has lost its beginning.
        let catalog = self.catalog_snapshot(flushed.0);
        crate::metadata::write(&self.dir, self.identity, &catalog)?;
        Ok(())
    }

    /// Make `dest` a complete, independently openable copy of this database as
    /// of a fresh checkpoint.
    ///
    /// Only the files a restart itself depends on are copied — the heap, the
    /// log, the superblock, the catalog. `directory.adabt` and
    /// `derived.adabt` are not: they are caches (see their own modules), a
    /// restored database rebuilds them exactly as any other reopen would if
    /// they turned out to be missing or stale, and starting a restore from
    /// cache entries stamped against a checkpoint that may not even be
    /// `dest`'s most recent has no advantage over just not copying them.
    ///
    /// Checkpointing first is what makes the copy meaningful: without it,
    /// `dest` would need every log segment back to the last one, rather than
    /// only the ones this checkpoint could not yet fold into pages — and,
    /// since `checkpoint` discards (or archives) everything older, taking the
    /// checkpoint here rather than trusting the caller to have already is
    /// what keeps the two facts — "the log copied" and "the state the log
    /// starts from" — from being able to drift apart.
    ///
    /// Refuses if `dest` already exists and is non-empty, exactly as
    /// `restore_from` does — a backup target is not a thing this library
    /// unilaterally clears out from under a caller who typo'd a path;
    /// deleting whatever was already there is the caller's decision to make
    /// explicitly, not this function's to make for them.
    pub fn backup_to(&mut self, dest: &Path) -> Result<()> {
        self.checkpoint()?;
        std::fs::create_dir_all(dest)?;
        if std::fs::read_dir(dest)?.next().is_some() {
            return Err(Error::InvalidRestore(format!(
                "refusing to back up into non-empty directory {}",
                dest.display()
            )));
        }
        std::fs::copy(Self::heap_path(&self.dir), Self::heap_path(dest))?;
        copy_dir_all(&Self::wal_path(&self.dir), &Self::wal_path(dest))?;
        std::fs::copy(
            crate::superblock::path(&self.dir),
            crate::superblock::path(dest),
        )?;
        std::fs::copy(
            crate::metadata::path(&self.dir),
            crate::metadata::path(dest),
        )?;
        Ok(())
    }

    /// Reconstruct a database directory from a backup made by `backup_to`.
    ///
    /// `dest` must not already exist (or must be empty) — the same reasoning
    /// as `backup_to`'s refusal, in the direction where getting it wrong is
    /// worse: overwriting a live database's files with a backup's is exactly
    /// the kind of destructive action this library never takes without the
    /// caller asking for it by name. Once this returns, `dest` opens exactly
    /// as `src` would, including through `open_at` with a `RecoverTarget::Lsn`
    /// short of `src`'s own history — restoring copies bytes; it does not
    /// decide how much of them a later open replays.
    pub fn restore_from(src: &Path, dest: &Path) -> Result<()> {
        if !crate::superblock::path(src).exists() {
            return Err(Error::InvalidRestore(format!(
                "{} is not an aDaBt backup: no superblock present",
                src.display()
            )));
        }
        std::fs::create_dir_all(dest)?;
        if std::fs::read_dir(dest)?.next().is_some() {
            return Err(Error::InvalidRestore(format!(
                "refusing to restore into non-empty directory {}",
                dest.display()
            )));
        }
        copy_dir_all(src, dest)
    }

    /// Describe the current state of the primary, for the derived-representation
    /// cache to be validated against.
    ///
    /// Everything here is already in memory after recovery — the log position,
    /// the heap file's length, and the directory sizes — so taking a stamp is
    /// cheap enough to do on every checkpoint and every open.
    pub fn state_stamp(&mut self) -> Result<crate::derived::Stamp> {
        let mut counts: Vec<(String, u64)> = Vec::new();
        for name in self.store_collection_names() {
            counts.push((name.clone(), self.count(&name)? as u64));
        }
        counts.sort();
        Ok(crate::derived::Stamp {
            identity: self.identity,
            lsn: self.wal.next_lsn().0,
            heap_bytes: std::fs::metadata(Self::heap_path(&self.dir))
                .map(|m| m.len())
                .unwrap_or(0),
            counts,
        })
    }

    /// Index definitions recorded in the log, for rebuilding after a restart.
    ///
    /// Definitions only. The contents are derived and reconstructed by a scan,
    /// which is the rebuildability invariant doing its job: losing an index
    /// costs a scan, never a record.
    /// Every collection and the id its pages are stamped with, sorted by name.
    ///
    /// Exposed for tests that need to prove the binding did not move. It is the
    /// one piece of state where "the same, exactly" is the whole requirement.
    pub fn collection_ids(&self) -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> = self
            .collections
            .iter()
            .filter(|(n, _)| !is_migration_name(n))
            .map(|(n, c)| (n.clone(), c.id.0))
            .collect();
        v.sort();
        v
    }

    pub fn index_definitions(&self) -> &[(String, String, String)] {
        &self.index_defs
    }

    pub fn record_index(&mut self, collection: &str, field: &str, kind: &str) -> Result<()> {
        let def = (collection.to_string(), field.to_string(), kind.to_string());
        if store_has(&self.index_defs, &def) {
            return Ok(());
        }
        self.log(WalOp::CreateIndex {
            collection: collection.to_string(),
            field: field.to_string(),
            kind: kind.to_string(),
        })?;
        self.index_defs.push(def);
        Ok(())
    }

    pub fn forget_index(&mut self, collection: &str, field: &str, kind: &str) -> Result<()> {
        let before = self.index_defs.len();
        self.index_defs
            .retain(|(c, f, k)| !(c == collection && f == field && k == kind));
        if self.index_defs.len() != before {
            self.log(WalOp::DropIndex {
                collection: collection.to_string(),
                field: field.to_string(),
                kind: kind.to_string(),
            })?;
        }
        Ok(())
    }

    /// Replace a collection's schema, in whichever of two ways applies.
    ///
    /// Every record is validated against the new schema *before* anything is
    /// written. A partial freeze would leave a collection whose stored records
    /// its own schema rejects, which no later operation could repair.
    ///
    /// # Two paths, chosen by `codec::schema_editable_in_place`
    ///
    /// Most schema changes cannot avoid touching every row: a field moved,
    /// retyped, or inserted anywhere but the end changes the byte offset of
    /// data records already have on disk, and there is no way to make an old
    /// record answer to a new layout without rewriting it. For that case, see
    /// "why this is done out of place" below.
    ///
    /// But appending one nullable, fixed-width field, or dropping the last
    /// field, changes no existing byte's meaning at all — see
    /// `codec::schema_editable_in_place` for exactly why those two are safe
    /// and nothing wider is. When it applies, this logs one
    /// `WalOp::AlterSchemaInPlace` entry — a catalog edit, not a data
    /// operation — and returns `Ok(0)`: zero rows rewritten is the honest
    /// answer, and a caller checking the return value learns the change was
    /// free without needing a second method to ask.
    ///
    /// # Why the other path is done out of place
    ///
    /// The obvious implementation — log the new schema, then rewrite each
    /// record — is silently unsafe, and the failure is not a lost record but a
    /// wrong one. Recovery must apply a schema change before replaying the
    /// writes that assume it, so after a crash part-way through the rewrite the
    /// new codec meets the bytes the old codec left in the pages. A tag-length
    /// layout read as a fixed one decodes to plausible nonsense and raises no
    /// error: `id` comes back as 7305804385234280967 and nothing complains.
    ///
    /// So the new encoding is built beside the old one under a private name and
    /// adopted in a single log entry. Truncate the log anywhere before that
    /// entry and the original collection is untouched; anywhere after it and the
    /// migration is complete. There is no in-between state to recover from.
    ///
    /// The cost is honest: the collection is stored twice until the flip.
    pub fn alter_schema(&mut self, collection: &str, schema: Schema) -> Result<usize> {
        if is_migration_name(collection) {
            return Err(Error::InvalidOptimization(
                "cannot alter the schema of a migration collection".into(),
            ));
        }
        let old_schema = self.coll(collection)?.codec.schema().clone();
        if crate::codec::schema_editable_in_place(&old_schema, &schema) {
            self.log(WalOp::AlterSchemaInPlace {
                collection: collection.to_string(),
                schema: encode_schema(&schema),
            })?;
            self.apply_alter_schema_in_place(collection, schema)?;
            return Ok(0);
        }
        let rows = self.scan(collection)?;
        // A field the new schema no longer declares is what "drop this field"
        // *means* — dropping is exactly as much a schema change as adding one,
        // so a row carrying it is not a row that fails to fit the new schema,
        // it is a row that needs projecting onto it first. Validating the raw
        // old record instead, as an earlier version of this method did, meant
        // `alter_schema` could add a field but could never drop one outside
        // `Declared`/`Dynamic`'s overflow bag: any `Strict` or `Fixed` row
        // still carrying the dropped field would fail
        // `Schema::validate_record` with `UnknownField` before a single byte
        // was rewritten.
        let projected: Vec<(RecordId, Record)> = rows
            .iter()
            .map(|(id, rec)| (*id, project_onto_schema(&schema, rec)))
            .collect();
        for (id, rec) in &projected {
            schema.validate_record(rec).map_err(|e| {
                Error::InvalidOptimization(format!(
                    "record {id} does not fit the proposed schema: {e}"
                ))
            })?;
        }

        // A stale staging area from an earlier crash is dead weight, not state.
        let staging = migration_name(collection);
        if self.collections.contains_key(&staging) {
            self.drop_collection(&staging)?;
        }

        self.log(WalOp::CreateCollection {
            name: staging.clone(),
            schema: encode_schema(&schema),
        })?;
        self.register_collection(&staging, schema);

        for (id, rec) in &projected {
            let bytes = self.coll(&staging)?.codec.encode(rec)?;
            self.log(WalOp::Insert {
                collection: staging.clone(),
                id: *id,
                bytes: bytes.clone(),
            })?;
            self.apply_put(&staging, *id, &bytes)?;
        }

        // The flip. Everything above this line is reversible by ignoring it.
        self.log(WalOp::AdoptMigration {
            target: collection.to_string(),
            source: staging.clone(),
        })?;
        self.adopt(collection, &staging)?;
        Ok(rows.len())
    }

    /// Rebuild `collection`'s codec from `schema` and nothing else — no page,
    /// no directory entry, no id is touched. Called both from `alter_schema`
    /// directly and from recovery, so it does not itself decide eligibility;
    /// by the time it runs, `codec::schema_editable_in_place` already has.
    fn apply_alter_schema_in_place(&mut self, collection: &str, schema: Schema) -> Result<()> {
        let c = self
            .collections
            .get_mut(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?;
        c.codec = RecordCodec::new(schema);
        Ok(())
    }

    /// Hand `source`'s identity and records to `target`, discarding the old.
    ///
    /// Idempotent: replaying an adoption whose source is already gone is a
    /// no-op, which is what lets recovery apply it without tracking whether it
    /// already had.
    fn adopt(&mut self, target: &str, source: &str) -> Result<()> {
        let Some(staged) = self.collections.remove(source) else {
            return Ok(());
        };
        self.by_id.remove(&staged.id);
        // The staged records carry the staging collection's id in their slot
        // prefix, so the id travels with them rather than the name.
        if let Some(old) = self.collections.remove(target) {
            self.by_id.remove(&old.id);
            self.free_pages_of(&old)?;
        }
        self.by_id.insert(staged.id, target.to_string());
        self.collections.insert(target.to_string(), staged);
        Ok(())
    }

    /// Release every page slot a collection's live records occupy.
    fn free_pages_of(&mut self, c: &Collection) -> Result<()> {
        let locs: Vec<RecordLocation> = c
            .directory
            .values()
            .flat_map(|chain| chain.versions.iter().filter_map(|(_, l)| *l))
            .collect();
        let mut touched: Vec<PageId> = Vec::new();
        for loc in locs {
            let page = self.pool.get_mut(loc.page)?;
            let _ = page.delete(loc.slot);
            if !touched.contains(&loc.page) {
                touched.push(loc.page);
            }
        }
        // Deleting a slot leaves its bytes in place until the page is compacted,
        // so without this a migration's freed space is never reusable.
        for pid in touched {
            let page = self.pool.get_mut(pid)?;
            if page.fragmentation() > 0 {
                page.compact();
            }
            let free = page.free_space();
            self.fsm.set(pid, free);
        }
        Ok(())
    }

    pub fn compression_enabled(&self) -> bool {
        self.compress
    }

    /// Declare (or clear) the collection's clustering field. Logged, so the
    /// declaration survives restarts; the page ranges themselves re-derive.
    /// Fails only if the collection does not exist.
    pub fn set_cluster_field(&mut self, collection: &str, field: Option<&str>) -> Result<()> {
        self.coll(collection)?;
        self.log(WalOp::SetClusterField {
            collection: collection.to_string(),
            field: field.map(String::from),
        })?;
        let c = self.collections.get_mut(collection).expect("checked above");
        c.cluster_name = field.map(String::from);
        Ok(())
    }

    /// The declared clustering fields across all user collections — what an
    /// engine reads once at open to restore its routing map.
    pub fn declared_cluster_fields(&self) -> Vec<(String, String)> {
        self.collections
            .iter()
            .filter(|(name, _)| !is_migration_name(name))
            .filter_map(|(name, c)| c.cluster_name.as_ref().map(|f| (name.clone(), f.clone())))
            .collect()
    }

    /// How many distinct pages `get` has touched since the last clear.
    ///
    /// This is the number a locality claim is measured against: a clustered
    /// range scan should touch pages in proportion to the *range's* size,
    /// an unclustered one in proportion to the collection's. A diagnostic,
    /// not a statistic — it accumulates under the caller's control rather
    /// than pretending to be a rate.
    pub fn touched_pages(&self) -> usize {
        self.touched.len()
    }

    /// Reset the touched-page set.
    pub fn clear_page_touches(&mut self) {
        self.touched.clear();
    }

    /// Bytes after which a log segment is sealed. See [`crate::wal::Wal::set_segment_bytes`].
    pub fn set_segment_bytes(&mut self, bytes: u64) {
        self.wal.set_segment_bytes(bytes);
    }

    /// Send discarded log segments here rather than deleting them.
    pub fn set_log_archive(&mut self, dir: Option<std::path::PathBuf>) {
        self.wal.set_archive(dir);
    }

    /// Turn sequential read-ahead on or off in the buffer pool.
    pub fn set_prefetch(&mut self, on: bool) {
        self.pool.set_read_ahead(on);
    }

    pub fn prefetch_enabled(&self) -> bool {
        self.pool.read_ahead_enabled()
    }

    pub fn pool_stats(&self) -> crate::pager::BufferStats {
        self.pool.stats()
    }

    /// Log fsyncs so far. Exposed so a batch API's whole reason for existing —
    /// fewer of these — is something a test can assert on rather than time.
    pub fn sync_count(&self) -> u64 {
        self.wal.sync_count()
    }

    /// Turn compression on or off for *future* writes.
    ///
    /// Existing records keep whatever encoding they were written with, so this
    /// is instant and cannot fail. `recompress_all` re-encodes what is already
    /// stored, and is the expensive part a cost estimate has to account for.
    pub fn set_compression(&mut self, on: bool) {
        self.compress = on;
    }

    /// Re-encode every stored record under the current setting.
    ///
    /// Returns the change in stored bytes: negative when compression saved
    /// space. Reported rather than assumed, because whether a particular
    /// dataset compresses is a property of the data, not of the algorithm.
    pub fn recompress_all(&mut self) -> Result<i64> {
        let before = self.stored_bytes()? as i64;
        for name in self.store_collection_names() {
            let rows = self.scan(&name)?;
            for (id, rec) in rows {
                let bytes = self.coll(&name)?.codec.encode(&rec)?;
                // Goes through the ordinary write path, so the WAL records it
                // and a crash mid-rewrite replays correctly.
                self.log(WalOp::Update {
                    collection: name.clone(),
                    id,
                    bytes: bytes.clone(),
                })?;
                self.apply_put(&name, id, &bytes)?;
            }
        }
        Ok(self.stored_bytes()? as i64 - before)
    }

    fn store_collection_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .collections
            .keys()
            .filter(|n| !is_migration_name(n))
            .cloned()
            .collect();
        v.sort();
        v
    }

    /// Total bytes occupied by live records, excluding page and slot overhead.
    pub fn stored_bytes(&mut self) -> Result<u64> {
        let locs: Vec<RecordLocation> = self
            .collections
            .values()
            .flat_map(|c| c.directory.values().filter_map(|v| v.newest()))
            .collect();
        let mut total = 0u64;
        for loc in locs {
            let page = self.pool.get(loc.page)?;
            total += page.get(loc.slot)?.len() as u64;
        }
        Ok(total)
    }

    /// Open a stable read view.
    ///
    /// Versions it might need are retained until it is dropped, which is what
    /// lets two representations be compared against identical state.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::open(Arc::clone(&self.versions))
    }

    /// When `id` was last touched — inserted, updated or deleted — whichever
    /// is newest.
    ///
    /// The building block first-committer-wins conflict detection is made of: a
    /// transaction whose write-set includes a key touched *after* its own
    /// snapshot was taken lost the race and must be told so, rather than
    /// silently overwriting a change it never saw.
    pub fn latest_write_ts(&self, collection: &str, id: RecordId) -> Result<Option<TxnId>> {
        Ok(self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|c| c.newest_stamp()))
    }

    pub fn retained_versions(&self) -> usize {
        self.retained
    }

    pub fn open_snapshots(&self) -> usize {
        self.versions.open_count()
    }

    /// Free versions no open snapshot can reach.
    ///
    /// Returns how many were dropped. With a snapshot open this is bounded by
    /// its position, so a long-running reader keeps exactly what it needs alive
    /// and nothing more.
    pub fn reclaim(&mut self) -> Result<usize> {
        let horizon = self.versions.reclaim_horizon();
        let mut freed_locations = Vec::new();
        for c in self.collections.values_mut() {
            let mut empty_ids = Vec::new();
            for (id, chain) in c.directory.iter_mut() {
                freed_locations.extend(chain.reclaim_to(horizon));
                // A chain holding only a tombstone no reader can precede is
                // just overhead.
                if chain.len() == 1 && chain.is_absent() {
                    empty_ids.push(*id);
                }
            }
            for id in empty_ids {
                c.directory.remove(&id);
            }
        }
        let count = freed_locations.len();
        let mut touched: Vec<PageId> = Vec::new();
        for loc in freed_locations {
            let page = self.pool.get_mut(loc.page)?;
            let _ = page.delete(loc.slot);
            if !touched.contains(&loc.page) {
                touched.push(loc.page);
            }
        }
        // Deleting a slot leaves its payload in place until the page is
        // compacted, so without this the freed bytes are never reusable and a
        // versioned update stream grows the file without bound.
        for pid in touched {
            let page = self.pool.get_mut(pid)?;
            if page.fragmentation() > 0 {
                page.compact();
            }
            let free = page.free_space();
            self.fsm.set(pid, free);
        }
        self.retained = self.retained.saturating_sub(count);
        Ok(count)
    }

    /// Give trailing heap pages back to the operating system.
    ///
    /// The free-space map already lets a deleted record's space be reused, so a
    /// steady workload does not grow the file without bound. What it cannot do
    /// is *return* anything: a collection dropped, or the old copy left behind by
    /// a schema migration, leaves holes that only a future insert can fill. An
    /// operator with a full disk has no answer to that except restoring from a
    /// backup, which is not an answer.
    ///
    /// So live records are moved out of the last pages into the holes earlier in
    /// the file, and the tail is then truncated. Each move goes through the
    /// ordinary write path — a new version at the new location, the old one
    /// reclaimable — so a crash part-way through leaves the record readable in
    /// one place or the other, never neither.
    ///
    /// Returns the number of pages returned.
    pub fn vacuum(&mut self) -> Result<u32> {
        // Dead versions first: they may be the only thing holding a tail page.
        self.reclaim()?;

        let mut freed = 0u32;
        loop {
            let count = self.pool.page_count();
            if count == 0 {
                break;
            }
            let last = PageId(count - 1);
            let occupants = self.live_records_on(last)?;
            if occupants.is_empty() {
                self.pool.truncate_to(count - 1)?;
                self.fsm.forget(last);
                freed += 1;
                continue;
            }
            // Take the page out of the free-space map first, or `place` is at
            // liberty to put the record straight back where it came from.
            self.fsm.forget(last);
            let mut moved_any = false;
            for (collection, id) in occupants {
                if self.relocate(&collection, id, last)? {
                    moved_any = true;
                }
            }
            if !moved_any {
                // Nothing on the last page could be moved anywhere earlier, so
                // the file is as short as it can be made without splitting
                // records across pages.
                break;
            }
            self.reclaim()?;
        }
        Ok(freed)
    }

    /// Which records currently live on a page.
    fn live_records_on(&self, page: PageId) -> Result<Vec<(String, RecordId)>> {
        let mut out = Vec::new();
        for (name, c) in &self.collections {
            for (id, chain) in &c.directory {
                if chain.newest().is_some_and(|l| l.page == page) {
                    out.push((name.clone(), *id));
                }
            }
        }
        Ok(out)
    }

    /// Move one record off `from`, if there is room for it earlier.
    fn relocate(&mut self, collection: &str, id: RecordId, from: PageId) -> Result<bool> {
        let Some(loc) = self
            .collections
            .get(collection)
            .and_then(|c| c.directory.get(&id))
            .and_then(|ch| ch.newest())
        else {
            return Ok(false);
        };
        if loc.page != from {
            return Ok(false);
        }
        let payload = self.pool.get(loc.page)?.get(loc.slot)?.to_vec();
        let need = crate::page::Page::cost_of(payload.len());
        if self.fsm.find(need).is_none() {
            return Ok(false);
        }
        let new_loc = self.place(&payload)?;
        let txn = self.versions.begin_write();
        if let Some(c) = self.collections.get_mut(collection) {
            if let Some(chain) = c.directory.get_mut(&id) {
                chain.push(txn, Some(new_loc));
                self.retained += 1;
            }
        }
        Ok(true)
    }

    /// Reclaim once retention grows past a threshold.
    ///
    /// Driven by pressure rather than by every write: reclamation walks every
    /// chain, and doing that per write would cost more than the versions do.
    fn maybe_reclaim(&mut self) -> Result<()> {
        const RETENTION_LIMIT: usize = 4096;
        if self.retained >= RETENTION_LIMIT {
            self.reclaim()?;
        }
        Ok(())
    }

    /// Read as of a snapshot rather than as of now.
    pub fn get_at(
        &mut self,
        collection: &str,
        id: RecordId,
        snapshot: &Snapshot,
    ) -> Result<Option<Record>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|c| c.visible_at(snapshot.at()))
        else {
            return Ok(None);
        };
        Ok(Some(self.read_at(collection, loc)?))
    }

    /// Scan as of a snapshot, in record-id order.
    pub fn scan_at(
        &mut self,
        collection: &str,
        snapshot: &Snapshot,
    ) -> Result<Vec<(RecordId, Record)>> {
        let locs: Vec<(RecordId, RecordLocation)> = self
            .coll(collection)?
            .directory
            .iter()
            .filter_map(|(id, chain)| chain.visible_at(snapshot.at()).map(|l| (*id, l)))
            .collect();
        let mut out = Vec::with_capacity(locs.len());
        for (id, loc) in locs {
            out.push((id, self.read_at(collection, loc)?));
        }
        Ok(out)
    }

    pub fn buffer_stats(&self) -> BufferStats {
        self.pool.stats()
    }
    pub fn page_count(&self) -> u32 {
        self.pool.page_count()
    }
    pub fn wal_appended(&self) -> u64 {
        self.wal.appended()
    }
    pub fn wal_syncs(&self) -> u64 {
        self.wal.sync_count()
    }
    pub fn durability(&self) -> Durability {
        self.wal.durability()
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn set_pool_capacity(&mut self, pages: usize) -> Result<()> {
        self.pool.set_capacity(pages)
    }

    /// Read one record out of the page it lives on.
    ///
    /// The codec is resolved *before* the page, and by touching
    /// `self.collections` directly rather than through `coll`. Both matter:
    /// `decompress` now borrows the page rather than copying it, so the
    /// decoded bytes hold a borrow of `self.pool` for the rest of the
    /// function. A `&self` method like `coll` borrows the whole struct and
    /// would conflict with that; naming the two fields separately lets the
    /// borrow checker see they are disjoint.
    fn read_at(&mut self, collection: &str, loc: RecordLocation) -> Result<Record> {
        let codec = &self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?
            .codec;
        let page = self.pool.get(loc.page)?;
        let payload = page.get(loc.slot)?;
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        let bytes = crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?;
        codec.decode(&bytes)
    }
}

impl LogicalStore for HeapStore {
    fn create_collection(&mut self, name: &str, schema: Schema) -> Result<()> {
        if self.collections.contains_key(name) {
            return Err(Error::CollectionExists(name.to_string()));
        }
        self.log(WalOp::CreateCollection {
            name: name.to_string(),
            schema: encode_schema(&schema),
        })?;
        self.register_collection(name, schema);
        Ok(())
    }

    fn drop_collection(&mut self, name: &str) -> Result<()> {
        if !self.collections.contains_key(name) {
            return Err(Error::NoSuchCollection(name.to_string()));
        }
        self.log(WalOp::DropCollection {
            name: name.to_string(),
        })?;
        if let Some(c) = self.collections.remove(name) {
            self.by_id.remove(&c.id);
            self.free_pages_of(&c)?;
        }
        Ok(())
    }

    fn collection_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .collections
            .keys()
            .filter(|n| !is_migration_name(n))
            .cloned()
            .collect();
        v.sort();
        v
    }

    fn schema_of(&self, collection: &str) -> Result<&Schema> {
        Ok(self.coll(collection)?.codec.schema())
    }

    fn insert(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<()> {
        normalize_for_storage(&mut rec);
        let c = self.coll(collection)?;
        c.codec.schema().validate_record(&rec)?;
        if c.directory.get(&id).is_some_and(|v| !v.is_absent()) {
            return Err(Error::RecordExists(id));
        }
        let bytes = c.codec.encode(&rec)?;
        self.log(WalOp::Insert {
            collection: collection.to_string(),
            id,
            bytes: bytes.clone(),
        })?;
        self.apply_put(collection, id, &bytes)
    }

    fn insert_keyed(
        &mut self,
        collection: &str,
        id: RecordId,
        rec: Record,
        key: i64,
    ) -> Result<()> {
        let mut rec = rec;
        normalize_for_storage(&mut rec);
        let c = self.coll(collection)?;
        c.codec.schema().validate_record(&rec)?;
        if c.directory.get(&id).is_some_and(|v| !v.is_absent()) {
            return Err(Error::RecordExists(id));
        }
        let bytes = c.codec.encode(&rec)?;
        // The log carries the same op either way: clustering is placement, not
        // content, so replay reproduces the bytes exactly and re-derives (or
        // forgets, which is equally correct) the locality.
        self.log(WalOp::Insert {
            collection: collection.to_string(),
            id,
            bytes: bytes.clone(),
        })?;
        self.apply_put_keyed(collection, id, &bytes, key)
    }

    fn get(&mut self, collection: &str, id: RecordId) -> Result<Option<Record>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|v| v.newest())
        else {
            // Still validates the collection exists, above.
            return Ok(None);
        };
        self.touched.insert(loc.page.0);
        Ok(Some(self.read_at(collection, loc)?))
    }

    fn peek_field(
        &mut self,
        collection: &str,
        id: RecordId,
        field: &str,
    ) -> Result<Option<Option<Value>>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|v| v.newest())
        else {
            return Ok(None);
        };
        let codec = &self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?
            .codec;
        let page = self.pool.get(loc.page)?;
        let payload = page.get(loc.slot)?;
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        let bytes = crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?;
        self.touched.insert(loc.page.0);
        Ok(Some(codec.peek_field(&bytes, field)?))
    }

    fn get_projected(
        &mut self,
        collection: &str,
        id: RecordId,
        fields: &[&str],
    ) -> Result<Option<Record>> {
        let Some(loc) = self
            .coll(collection)?
            .directory
            .get(&id)
            .and_then(|v| v.newest())
        else {
            return Ok(None);
        };
        let codec = &self
            .collections
            .get(collection)
            .ok_or_else(|| Error::NoSuchCollection(collection.to_string()))?
            .codec;
        let page = self.pool.get(loc.page)?;
        let payload = page.get(loc.slot)?;
        if payload.len() < SLOT_PREFIX {
            return Err(Error::Corruption(
                "stored slot is shorter than its prefix".into(),
            ));
        }
        let encoding = crate::compress::Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
        let bytes = crate::compress::decompress(encoding, &payload[SLOT_PREFIX..])?;
        self.touched.insert(loc.page.0);
        if fields.is_empty() {
            return Ok(Some(Record::new()));
        }
        Ok(Some(codec.peek_fields(&bytes, fields)?))
    }

    fn update(&mut self, collection: &str, id: RecordId, mut rec: Record) -> Result<bool> {
        normalize_for_storage(&mut rec);
        let c = self.coll(collection)?;
        c.codec.schema().validate_record(&rec)?;
        let existed = c.directory.get(&id).is_some_and(|v| !v.is_absent());
        let bytes = c.codec.encode(&rec)?;
        self.log(WalOp::Update {
            collection: collection.to_string(),
            id,
            bytes: bytes.clone(),
        })?;
        self.apply_put(collection, id, &bytes)?;
        Ok(existed)
    }

    fn delete(&mut self, collection: &str, id: RecordId) -> Result<bool> {
        let existed = self
            .coll(collection)?
            .directory
            .get(&id)
            .is_some_and(|v| !v.is_absent());
        if existed {
            self.log(WalOp::Delete {
                collection: collection.to_string(),
                id,
            })?;
        }
        self.apply_delete(collection, id)
    }

    fn scan(&mut self, collection: &str) -> Result<Vec<(RecordId, Record)>> {
        let locs: Vec<(RecordId, RecordLocation)> = self
            .coll(collection)?
            .directory
            .iter()
            .filter_map(|(k, v)| v.newest().map(|l| (*k, l)))
            .collect();
        let mut out = Vec::with_capacity(locs.len());
        for (id, loc) in locs {
            out.push((id, self.read_at(collection, loc)?));
        }
        Ok(out)
    }

    fn count(&mut self, collection: &str) -> Result<usize> {
        Ok(self
            .coll(collection)?
            .directory
            .values()
            .filter(|v| !v.is_absent())
            .count())
    }

    /// Straight off the in-memory page directory: no pages, no decodes.
    ///
    /// The filter matches `scan`'s exactly — `newest()` is `None` for a record
    /// whose newest version is a tombstone — because the two must agree. They
    /// are the same question asked at different costs, and a scan driven by
    /// ids that disagreed with `scan` would drop or invent rows.
    fn ids(&mut self, collection: &str) -> Result<Vec<RecordId>> {
        Ok(self
            .coll(collection)?
            .directory
            .iter()
            .filter_map(|(k, v)| v.newest().map(|_| *k))
            .collect())
    }
}

/// Whether a definition is already recorded.
fn store_has(defs: &[(String, String, String)], def: &(String, String, String)) -> bool {
    defs.iter().any(|d| d == def)
}
