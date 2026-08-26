//! Write-ahead log.
//!
//! Every change is described here before it is allowed to reach a heap page, so
//! a crash can be repaired by replaying what the log remembers. The log is the
//! authority on what happened; the heap file is a materialisation of it that
//! may lag behind.
//!
//! Entries are logical (`insert record 7 into "users"`) rather than physical
//! (`write these bytes at this page offset`). That costs some replay speed and
//! buys something worth more: replay does not depend on the heap having the
//! same physical layout it had when the entry was written, so the physical
//! layer stays free to change underneath — which is the entire point of the
//! project.
//!
//! **Durability is policy.** `Durability::Strict` fsyncs before acknowledging a
//! commit, `GroupCommit` batches, `Relaxed` does not fsync at all. The optimizer
//! may never choose a weaker setting than the policy allows; that check lives in
//! `GuaranteeRequirements`, not here.

use adabt_core::error::{Error, Result};
use adabt_core::ids::{Lsn, RecordId, TxnId};
use adabt_core::policy::Durability;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::varint;

const OP_INSERT: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_DELETE: u8 = 3;
const OP_CREATE_COLLECTION: u8 = 4;
const OP_DROP_COLLECTION: u8 = 5;
const OP_COMMIT: u8 = 6;
const OP_CHECKPOINT: u8 = 7;
const OP_CREATE_INDEX: u8 = 8;
const OP_DROP_INDEX: u8 = 9;
const OP_ADOPT_MIGRATION: u8 = 10;
const OP_ALTER_SCHEMA_IN_PLACE: u8 = 11;
/// The clustering declaration: which field steers record placement. Logged
/// like an index definition — a declaration about the physical shape, not
/// content, replayed to restore the catalog's memory of it.
const OP_SET_CLUSTER_FIELD: u8 = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOp {
    Insert {
        collection: String,
        id: RecordId,
        bytes: Vec<u8>,
    },
    Update {
        collection: String,
        id: RecordId,
        bytes: Vec<u8>,
    },
    Delete {
        collection: String,
        id: RecordId,
    },
    /// DDL is **not transactional**: this entry commits on write and is not
    /// rolled back with any surrounding `Begin`/`Commit`. A `CreateCollection`
    /// that is later "aborted" by discarding its transaction handle remains
    /// visible — the format-level contract, not a note in passing.
    CreateCollection {
        name: String,
        schema: Vec<u8>,
    },
    /// DDL is not transactional — see `CreateCollection`.
    DropCollection {
        name: String,
    },
    /// Declare (or clear, with `None`) the field whose integer values steer
    /// record placement. The declaration is catalog state; placement itself
    /// re-derives from subsequent keyed inserts.
    SetClusterField {
        collection: String,
        field: Option<String>,
    },
    Commit {
        txn: TxnId,
    },
    /// Every heap page up to `flushed_lsn` is on stable storage, so replay may
    /// start after it.
    Checkpoint {
        flushed_lsn: Lsn,
    },
    /// An index *definition*. Only the definition is logged; the contents are
    /// derived and rebuilt on open, which is the rebuildability invariant doing
    /// its job — losing them costs a scan, never a record.
    CreateIndex {
        collection: String,
        field: String,
        kind: String,
    },
    DropIndex {
        collection: String,
        field: String,
        kind: String,
    },
    /// A schema migration completed: `target` takes over `source` entirely —
    /// its schema, its codec and its records.
    ///
    /// The single point at which a schema change becomes true. Everything
    /// before it built the new encoding *beside* the old one under a private
    /// name, so a log truncated anywhere earlier leaves the original collection
    /// exactly as it was. Re-encoding records in place and logging the schema
    /// first cannot be made safe: recovery would apply the new codec to the
    /// bytes the crash left behind in the old one, and decode them to garbage
    /// without raising an error.
    AdoptMigration {
        target: String,
        source: String,
    },
    /// A schema change applied without touching a single stored record: the
    /// new schema replaces the old one in the catalog and nothing else moves.
    ///
    /// Only ever logged for a change `codec::schema_editable_in_place` has
    /// already accepted, so replay does not re-check eligibility — it just
    /// rebuilds the collection's codec from `schema`, exactly as the original
    /// call did. Idempotent by construction: replaying it twice sets the same
    /// codec twice.
    AlterSchemaInPlace {
        collection: String,
        schema: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalEntry {
    pub lsn: Lsn,
    /// When this was appended, in nanoseconds since the Unix epoch.
    ///
    /// A log position answers "restore to entry 4,102,993". Only a clock reading
    /// answers "restore to 14:32", which is the question an operator actually
    /// asks. It is written now, while the framing is being rewritten anyway,
    /// because adding it once segments have been archived would mean a format
    /// change reaching into files that are already somebody's backup.
    ///
    /// Not trusted for ordering — the LSN is the order. A clock that steps
    /// backwards makes point-in-time recovery approximate, never inconsistent.
    pub nanos: u64,
    pub txn: TxnId,
    pub op: WalOp,
}

fn put_str(s: &str, out: &mut Vec<u8>) {
    varint::write_u64(s.len() as u64, out);
    out.extend_from_slice(s.as_bytes());
}

fn put_bytes(b: &[u8], out: &mut Vec<u8>) {
    varint::write_u64(b.len() as u64, out);
    out.extend_from_slice(b);
}

impl WalEntry {
    fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.lsn.0.to_le_bytes());
        out.extend_from_slice(&self.nanos.to_le_bytes());
        out.extend_from_slice(&self.txn.0.to_le_bytes());
        match &self.op {
            WalOp::Insert {
                collection,
                id,
                bytes,
            } => {
                out.push(OP_INSERT);
                put_str(collection, &mut out);
                varint::write_u64(id.0, &mut out);
                put_bytes(bytes, &mut out);
            }
            WalOp::Update {
                collection,
                id,
                bytes,
            } => {
                out.push(OP_UPDATE);
                put_str(collection, &mut out);
                varint::write_u64(id.0, &mut out);
                put_bytes(bytes, &mut out);
            }
            WalOp::Delete { collection, id } => {
                out.push(OP_DELETE);
                put_str(collection, &mut out);
                varint::write_u64(id.0, &mut out);
            }
            WalOp::CreateCollection { name, schema } => {
                out.push(OP_CREATE_COLLECTION);
                put_str(name, &mut out);
                put_bytes(schema, &mut out);
            }
            WalOp::DropCollection { name } => {
                out.push(OP_DROP_COLLECTION);
                put_str(name, &mut out);
            }
            WalOp::Commit { txn } => {
                out.push(OP_COMMIT);
                varint::write_u64(txn.0, &mut out);
            }
            WalOp::Checkpoint { flushed_lsn } => {
                out.push(OP_CHECKPOINT);
                varint::write_u64(flushed_lsn.0, &mut out);
            }
            WalOp::CreateIndex {
                collection,
                field,
                kind,
            } => {
                out.push(OP_CREATE_INDEX);
                put_str(collection, &mut out);
                put_str(field, &mut out);
                put_str(kind, &mut out);
            }
            WalOp::DropIndex {
                collection,
                field,
                kind,
            } => {
                out.push(OP_DROP_INDEX);
                put_str(collection, &mut out);
                put_str(field, &mut out);
                put_str(kind, &mut out);
            }
            WalOp::AdoptMigration { target, source } => {
                out.push(OP_ADOPT_MIGRATION);
                put_str(target, &mut out);
                put_str(source, &mut out);
            }
            WalOp::AlterSchemaInPlace { collection, schema } => {
                out.push(OP_ALTER_SCHEMA_IN_PLACE);
                put_str(collection, &mut out);
                // Varint length, matching `blob()`'s reader — a raw u32 here
                // made replay consume three zero bytes of the length word as
                // the first three bytes of the schema.
                put_bytes(schema, &mut out);
            }
            WalOp::SetClusterField { collection, field } => {
                out.push(OP_SET_CLUSTER_FIELD);
                put_str(collection, &mut out);
                match field {
                    Some(f) => {
                        out.push(1);
                        put_str(f, &mut out);
                    }
                    None => out.push(0),
                }
            }
        }
        out
    }

    fn decode_payload(buf: &[u8]) -> Result<Self> {
        let mut r = PayloadReader { buf, pos: 0 };
        let lsn = Lsn(r.u64()?);
        let nanos = r.u64()?;
        let txn = TxnId(r.u64()?);
        let op = match r.u8()? {
            OP_INSERT => WalOp::Insert {
                collection: r.string()?,
                id: RecordId(r.varint()?),
                bytes: r.blob()?,
            },
            OP_UPDATE => WalOp::Update {
                collection: r.string()?,
                id: RecordId(r.varint()?),
                bytes: r.blob()?,
            },
            OP_DELETE => WalOp::Delete {
                collection: r.string()?,
                id: RecordId(r.varint()?),
            },
            OP_CREATE_COLLECTION => WalOp::CreateCollection {
                name: r.string()?,
                schema: r.blob()?,
            },
            OP_DROP_COLLECTION => WalOp::DropCollection { name: r.string()? },
            OP_COMMIT => WalOp::Commit {
                txn: TxnId(r.varint()?),
            },
            OP_CHECKPOINT => WalOp::Checkpoint {
                flushed_lsn: Lsn(r.varint()?),
            },
            OP_CREATE_INDEX => WalOp::CreateIndex {
                collection: r.string()?,
                field: r.string()?,
                kind: r.string()?,
            },
            OP_DROP_INDEX => WalOp::DropIndex {
                collection: r.string()?,
                field: r.string()?,
                kind: r.string()?,
            },
            OP_ADOPT_MIGRATION => WalOp::AdoptMigration {
                target: r.string()?,
                source: r.string()?,
            },
            OP_ALTER_SCHEMA_IN_PLACE => WalOp::AlterSchemaInPlace {
                collection: r.string()?,
                schema: r.blob()?,
            },
            OP_SET_CLUSTER_FIELD => WalOp::SetClusterField {
                collection: r.string()?,
                field: match r.u8()? {
                    0 => None,
                    1 => Some(r.string()?),
                    other => {
                        return Err(Error::Corruption(format!(
                            "invalid cluster-field presence byte {other}"
                        )));
                    }
                },
            },
            other => {
                return Err(Error::Corruption(format!("unknown wal opcode {other}")));
            }
        };
        Ok(WalEntry {
            lsn,
            nanos,
            txn,
            op,
        })
    }
}

struct PayloadReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| Error::Corruption("truncated wal entry".into()))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn varint(&mut self) -> Result<u64> {
        let (v, used) = varint::read_u64(&self.buf[self.pos..])?;
        self.pos += used;
        Ok(v)
    }
    fn string(&mut self) -> Result<String> {
        let n = self.varint()? as usize;
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|e| Error::Corruption(format!("invalid utf-8 in wal: {e}")))?
            .to_string())
    }
    fn blob(&mut self) -> Result<Vec<u8>> {
        let n = self.varint()? as usize;
        Ok(self.take(n)?.to_vec())
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Largest single log entry accepted while reading.
///
/// A corrupt length field must not make the reader allocate wildly before the
/// checksum gets a chance to reject it.
const MAX_ENTRY: u32 = 64 * 1024 * 1024;

/// Bytes after which the active segment is sealed and a new one begun.
///
/// Small enough that recovery and discard work in bounded memory; large enough
/// that rotation is rare compared to appending. Sixteen mebibytes is a few
/// hundred thousand ordinary entries.
pub const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;

const SEGMENT_MAGIC: &[u8; 8] = b"aDaBtWAL";
const SEGMENT_VERSION: u32 = 1;
/// magic(8) + version(4) + pad(4) + first_lsn(8) + created_nanos(8) + checksum(8)
const SEGMENT_HEADER: usize = 40;

/// One log segment on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentInfo {
    pub first_lsn: u64,
    pub first_nanos: u64,
    pub path: PathBuf,
}

pub struct Wal {
    /// The directory holding the segments. One file per segment.
    dir: PathBuf,
    writer: BufWriter<File>,
    /// Segments in ascending order of `first_lsn`; the last is the active one.
    segments: Vec<SegmentInfo>,
    active_bytes: u64,
    segment_bytes: u64,
    next_lsn: u64,
    /// The oldest entry still present. Zero while nothing has been discarded.
    start_lsn: u64,
    /// Where discarded segments go instead of being deleted.
    ///
    /// Point-in-time recovery is the reason: a segment below the last checkpoint
    /// is redundant for *restart*, and is exactly what "restore to 14:32" needs.
    /// The hook is here now so that turning archiving on later is a policy
    /// change rather than a format change.
    archive: Option<PathBuf>,
    durability: Durability,
    /// Entries appended since the last fsync, for group commit.
    pending: u32,
    group_size: u32,
    syncs: u64,
    appended: u64,
}

impl Wal {
    /// Open the log held in `dir`, which is a directory of segments.
    pub fn open(dir: &Path, durability: Durability) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Self::adopt_single_file_log(dir)?;

        let mut segments = Self::list_segments(dir)?;
        if segments.is_empty() {
            segments.push(Self::create_segment(dir, 1)?);
        }

        // Only the last segment can have a torn tail: every earlier one was
        // sealed and synced before the next was created. A half-written entry
        // was never acknowledged, so dropping it is correct.
        let active = segments.last().expect("just ensured non-empty").clone();
        let good_len = Self::intact_prefix_len(&active.path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&active.path)?;
        file.set_len(good_len)?;
        let mut file = file;
        file.seek(SeekFrom::End(0))?;

        let next_lsn = Self::last_lsn_in(&active.path)?
            .map(|l| l + 1)
            .unwrap_or(active.first_lsn);
        let start_lsn = if segments[0].first_lsn <= 1 {
            0
        } else {
            segments[0].first_lsn
        };

        Ok(Self {
            dir: dir.to_path_buf(),
            writer: BufWriter::new(file),
            segments,
            active_bytes: good_len,
            segment_bytes: SEGMENT_BYTES,
            next_lsn,
            start_lsn,
            archive: None,
            durability,
            pending: 0,
            group_size: 32,
            syncs: 0,
            appended: 0,
        })
    }

    /// Send discarded segments here rather than deleting them.
    pub fn set_archive(&mut self, dir: Option<PathBuf>) {
        self.archive = dir;
    }

    /// Bytes after which the active segment is sealed.
    ///
    /// A real trade rather than only a test hook: smaller segments are discarded
    /// at a finer grain, so a busy database holds less redundant log, and pay for
    /// it with more rotations and more files. The default suits a database
    /// checkpointing every few seconds.
    pub fn set_segment_bytes(&mut self, bytes: u64) {
        self.segment_bytes = bytes.max(SEGMENT_HEADER as u64 + 1);
    }

    pub fn segments(&self) -> &[SegmentInfo] {
        &self.segments
    }

    /// The segment currently being appended to.
    ///
    /// The only one that can hold a torn tail, and therefore the only one a
    /// crash test has any reason to damage.
    pub fn active_segment(dir: &Path) -> Result<Option<PathBuf>> {
        Ok(Self::list_segments(dir)?.pop().map(|s| s.path))
    }

    /// Convert a pre-segment log, if one is lying here.
    ///
    /// The single-file layout is format version 1. Rather than refuse to open
    /// such a database, its log becomes the first segment — which is what it
    /// already is, structurally, once a header is put in front of it. It is the
    /// second migration this project has written and the first that moves real
    /// data, which is the point of having built the version gate first.
    fn adopt_single_file_log(dir: &Path) -> Result<()> {
        let legacy = dir
            .parent()
            .map(|p| p.join("wal.adabt"))
            .filter(|p| p.exists());
        let Some(legacy) = legacy else {
            return Ok(());
        };
        if !Self::list_segments(dir)?.is_empty() {
            // Both present means a crash between conversion and cleanup. The
            // segments are the newer truth; the leftover is dropped.
            let _ = std::fs::remove_file(&legacy);
            return Ok(());
        }
        let body = std::fs::read(&legacy)?;
        let first_lsn = Self::first_lsn_of_frames(&body).unwrap_or(1);
        let target = Self::segment_path(dir, first_lsn);
        let mut out = Self::segment_header(first_lsn);
        out.extend_from_slice(&body);
        let tmp = target.with_extension("tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &target)?;
        std::fs::remove_file(&legacy)?;
        Ok(())
    }

    pub fn durability(&self) -> Durability {
        self.durability
    }
    pub fn next_lsn(&self) -> Lsn {
        Lsn(self.next_lsn)
    }

    /// The oldest entry the log still holds.
    ///
    /// Zero while nothing has been discarded, which is to say always, today.
    /// It becomes meaningful when the log learns to drop segments below a
    /// checkpoint — and the *reason* it is here already is that everything which
    /// rebuilds state from the log has to be able to ask whether the log is
    /// still complete. Adding the question later would mean adding it to code
    /// that had already been written on the assumption that the answer was yes.
    pub fn start_lsn(&self) -> Lsn {
        Lsn(self.start_lsn)
    }
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }
    pub fn appended(&self) -> u64 {
        self.appended
    }

    fn segment_path(dir: &Path, first_lsn: u64) -> PathBuf {
        dir.join(format!("seg-{first_lsn:016}.adabt"))
    }

    fn segment_header(first_lsn: u64) -> Vec<u8> {
        let mut h = Vec::with_capacity(SEGMENT_HEADER);
        h.extend_from_slice(SEGMENT_MAGIC);
        h.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&first_lsn.to_le_bytes());
        h.extend_from_slice(&Self::now_nanos().to_le_bytes());
        let sum = checksum(&h) as u64;
        h.extend_from_slice(&sum.to_le_bytes());
        debug_assert_eq!(h.len(), SEGMENT_HEADER);
        h
    }

    fn read_segment_header(path: &Path) -> Result<Option<(u64, u64)>> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut h = [0u8; SEGMENT_HEADER];
        if f.read_exact(&mut h).is_err() {
            return Ok(None);
        }
        if &h[..8] != SEGMENT_MAGIC {
            return Ok(None);
        }
        let stored = u64::from_le_bytes(h[32..40].try_into().expect("8 bytes"));
        if checksum(&h[..32]) as u64 != stored {
            return Ok(None);
        }
        let first_lsn = u64::from_le_bytes(h[16..24].try_into().expect("8 bytes"));
        let nanos = u64::from_le_bytes(h[24..32].try_into().expect("8 bytes"));
        Ok(Some((first_lsn, nanos)))
    }

    fn create_segment(dir: &Path, first_lsn: u64) -> Result<SegmentInfo> {
        let path = Self::segment_path(dir, first_lsn);
        let header = Self::segment_header(first_lsn);
        let mut f = File::create(&path)?;
        f.write_all(&header)?;
        f.sync_all()?;
        let nanos = u64::from_le_bytes(header[24..32].try_into().expect("8 bytes"));
        Ok(SegmentInfo {
            first_lsn,
            first_nanos: nanos,
            path,
        })
    }

    /// Every readable segment, ascending. Unreadable ones are ignored: a
    /// segment whose header does not check out cannot be placed in the sequence
    /// at all, and guessing where it belongs is how a gap becomes a silent
    /// reordering.
    fn list_segments(dir: &Path) -> Result<Vec<SegmentInfo>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for e in entries {
            let path = e?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("adabt") {
                continue;
            }
            if let Some((first_lsn, first_nanos)) = Self::read_segment_header(&path)? {
                out.push(SegmentInfo {
                    first_lsn,
                    first_nanos,
                    path,
                });
            }
        }
        out.sort_by_key(|s| s.first_lsn);
        Ok(out)
    }

    fn first_lsn_of_frames(body: &[u8]) -> Option<u64> {
        if body.len() < 16 {
            return None;
        }
        Some(u64::from_le_bytes(body[8..16].try_into().ok()?))
    }

    fn last_lsn_in(path: &Path) -> Result<Option<u64>> {
        Ok(Self::entries_in(path)?.last().map(|e| e.lsn.0))
    }

    /// Seal the active segment and begin a new one at the next LSN.
    fn rotate(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        let seg = Self::create_segment(&self.dir, self.next_lsn)?;
        let file = OpenOptions::new().read(true).append(true).open(&seg.path)?;
        self.writer = BufWriter::new(file);
        self.active_bytes = SEGMENT_HEADER as u64;
        self.segments.push(seg);
        Ok(())
    }

    /// Drop every segment lying entirely below `through`.
    ///
    /// **This is the whole point of segmenting.** Until it existed the log grew
    /// for the life of the database and was read in full on every open.
    ///
    /// A segment is entirely below `through` when the *next* segment begins at
    /// or before `through + 1`. Asking the question that way means no segment
    /// has to be read to decide whether it can go — the headers already say
    /// where each one starts, and the last is never a candidate because nothing
    /// follows it.
    ///
    /// The caller must have made everything up to `through` durable elsewhere
    /// first. In practice that is a checkpoint: pages flushed, the checkpoint
    /// entry logged and synced, the directory cache and the catalog written.
    /// Discarding before the catalog is durable would remove the only remaining
    /// record of which collection each page belongs to.
    pub fn discard_below(&mut self, through: Lsn) -> Result<usize> {
        let mut removable = 0usize;
        while removable + 1 < self.segments.len()
            && self.segments[removable + 1].first_lsn <= through.0 + 1
        {
            removable += 1;
        }
        if removable == 0 {
            return Ok(0);
        }
        for seg in self.segments.drain(..removable) {
            match &self.archive {
                Some(dest) => {
                    std::fs::create_dir_all(dest)?;
                    let to = dest.join(seg.path.file_name().unwrap_or_default());
                    // Rename across filesystems can fail; copy then remove is
                    // the portable fallback and archiving is not worth aborting
                    // a checkpoint over.
                    if std::fs::rename(&seg.path, &to).is_err() {
                        std::fs::copy(&seg.path, &to)?;
                        std::fs::remove_file(&seg.path)?;
                    }
                }
                None => std::fs::remove_file(&seg.path)?,
            }
        }
        self.start_lsn = self.segments[0].first_lsn;
        Ok(removable)
    }

    /// Append an operation and return the LSN assigned to it.
    pub fn append(&mut self, txn: TxnId, op: WalOp) -> Result<Lsn> {
        let lsn = Lsn(self.next_lsn);
        self.next_lsn += 1;
        let entry = WalEntry {
            lsn,
            nanos: Self::now_nanos(),
            txn,
            op,
        };
        let payload = entry.encode_payload();
        if payload.len() as u32 > MAX_ENTRY {
            return Err(Error::Corruption(format!(
                "wal entry of {} bytes exceeds the {MAX_ENTRY}-byte limit",
                payload.len()
            )));
        }
        self.writer
            .write_all(&(payload.len() as u32).to_le_bytes())?;
        self.writer.write_all(&checksum(&payload).to_le_bytes())?;
        self.writer.write_all(&payload)?;
        self.active_bytes += 8 + payload.len() as u64;
        self.pending += 1;
        self.appended += 1;
        if self.active_bytes >= self.segment_bytes {
            self.rotate()?;
        }
        Ok(lsn)
    }

    /// Make everything appended so far durable, as the policy requires.
    ///
    /// Called at commit. Under `Relaxed` this is a no-op, which is exactly the
    /// trade the policy asked for.
    pub fn commit(&mut self) -> Result<()> {
        match self.durability {
            Durability::Strict => self.sync(),
            Durability::GroupCommit => {
                if self.pending >= self.group_size {
                    self.sync()
                } else {
                    // Still leaves the bytes in the OS buffer; a crash loses at
                    // most one group, which is what GroupCommit promises.
                    self.writer.flush()?;
                    Ok(())
                }
            }
            Durability::Relaxed => Ok(()),
        }
    }

    /// Unconditionally flush and fsync, whatever the policy.
    pub fn sync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.pending = 0;
        self.syncs += 1;
        Ok(())
    }

    /// Length of the longest prefix of the file made up of intact entries.
    fn intact_prefix_len(path: &Path) -> Result<u64> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut whole = Vec::new();
        f.read_to_end(&mut whole)?;
        if whole.len() < SEGMENT_HEADER {
            // A segment with no complete header holds nothing; the header itself
            // is written and synced before any entry, so this is only reachable
            // for a file that was never a segment.
            return Ok(whole.len() as u64);
        }
        let buf = &whole[SEGMENT_HEADER..];
        let base = SEGMENT_HEADER as u64;
        let mut pos = 0usize;
        loop {
            if pos + 8 > buf.len() {
                return Ok(base + pos as u64);
            }
            let len = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
            let want = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]);
            if len > MAX_ENTRY {
                return Ok(base + pos as u64);
            }
            let start = pos + 8;
            let end = start + len as usize;
            if end > buf.len() || checksum(&buf[start..end]) != want {
                return Ok(base + pos as u64);
            }
            pos = end;
        }
    }

    /// Read every intact entry, stopping at the first damaged one.
    ///
    /// Stopping rather than skipping is deliberate: entries after a gap describe
    /// changes that assume the missing one happened, so replaying them would
    /// build a state that never existed.
    /// Wall clock, in nanoseconds since the epoch, as a `u64`.
    fn now_nanos() -> u64 {
        crate::superblock::now_nanos() as u64
    }

    /// Every intact entry in one segment file.
    fn entries_in(path: &Path) -> Result<Vec<WalEntry>> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Self::decode_frames(&buf[SEGMENT_HEADER.min(buf.len())..])
    }

    /// Entries at or above `from`, across every segment that can hold them.
    ///
    /// **Segments below `from` are never opened.** That is what makes recovery
    /// cost proportional to what happened since the last checkpoint rather than
    /// to the whole history of the database — which, before segmenting, was the
    /// entire cost of opening one.
    ///
    /// Memory is bounded by one segment, not by the log.
    pub fn entries_from(dir: &Path, from: Lsn) -> Result<Vec<WalEntry>> {
        let segments = Self::list_segments(dir)?;
        let mut out = Vec::new();
        for (i, seg) in segments.iter().enumerate() {
            // Skip a segment only when the *next* one starts at or below `from`;
            // otherwise this one may still contain it.
            let next_starts_below = segments
                .get(i + 1)
                .map(|n| n.first_lsn <= from.0)
                .unwrap_or(false);
            if next_starts_below {
                continue;
            }
            for e in Self::entries_in(&seg.path)? {
                if e.lsn.0 >= from.0 {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Every entry in the log. Kept for tests and tools; recovery uses
    /// [`Wal::entries_from`] so that it does not pay for history it has already
    /// folded into a checkpoint.
    pub fn read_all(dir: &Path) -> Result<Vec<WalEntry>> {
        Self::entries_from(dir, Lsn(0))
    }

    /// The lsn of the last entry at or before `nanos` — the translation from
    /// "restore to 14:32" into the lsn a point-in-time restore actually
    /// replays to.
    ///
    /// `nanos` is not trusted for ordering (see [`WalEntry::nanos`]), so this
    /// walks the log in lsn order and keeps the last entry whose clock reading
    /// had not yet passed the target, rather than trusting entries to be
    /// sorted by clock reading themselves. A clock that stepped backwards
    /// makes the answer approximate — some entries at or after the true
    /// moment may be included, or excluded, depending which side of the step
    /// they landed on — never inconsistent: whatever lsn this returns is
    /// still a real prefix of the log, and replaying to it is exactly the
    /// crash-recovery case of a log that stops at an arbitrary point, which
    /// every other recovery path in this module already has to get right.
    ///
    /// `None` means every entry in the log is already after `nanos` — there
    /// is no prefix of this log that ends at or before the requested moment.
    pub fn lsn_at_or_before(dir: &Path, nanos: u64) -> Result<Option<Lsn>> {
        let mut best: Option<Lsn> = None;
        for e in Self::entries_from(dir, Lsn(0))? {
            if e.nanos <= nanos {
                best = Some(match best {
                    Some(b) if b.0 >= e.lsn.0 => b,
                    _ => e.lsn,
                });
            }
        }
        Ok(best)
    }

    fn decode_frames(buf: &[u8]) -> Result<Vec<WalEntry>> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= buf.len() {
            let len = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
            let want = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]);
            if len > MAX_ENTRY {
                break;
            }
            let start = pos + 8;
            let end = start + len as usize;
            if end > buf.len() {
                break;
            }
            if checksum(&buf[start..end]) != want {
                break;
            }
            match WalEntry::decode_payload(&buf[start..end]) {
                Ok(e) => out.push(e),
                Err(_) => break,
            }
            pos = end;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adabt_testkit::rng::Rng;

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("adabt-wal-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_file(&p);
            Tmp(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn ops() -> Vec<WalOp> {
        vec![
            WalOp::CreateCollection {
                name: "users".into(),
                schema: vec![1, 2, 3],
            },
            WalOp::Insert {
                collection: "users".into(),
                id: RecordId(7),
                bytes: vec![9; 40],
            },
            WalOp::Update {
                collection: "users".into(),
                id: RecordId(7),
                bytes: vec![8; 12],
            },
            WalOp::Delete {
                collection: "users".into(),
                id: RecordId(7),
            },
            WalOp::Commit { txn: TxnId(3) },
            WalOp::Checkpoint {
                flushed_lsn: Lsn(4),
            },
            WalOp::DropCollection {
                name: "users".into(),
            },
        ]
    }

    #[test]
    fn every_op_round_trips_through_the_log() {
        let t = Tmp::new("roundtrip");
        {
            let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
            for op in ops() {
                w.append(TxnId(1), op).unwrap();
            }
            w.sync().unwrap();
        }
        let read = Wal::read_all(t.path()).unwrap();
        assert_eq!(read.len(), ops().len());
        for (got, want) in read.iter().zip(ops()) {
            assert_eq!(got.op, want);
        }
    }

    #[test]
    fn lsns_are_assigned_in_order_and_continue_across_a_reopen() {
        let t = Tmp::new("lsn");
        {
            let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
            assert_eq!(
                w.append(TxnId(1), WalOp::Commit { txn: TxnId(1) }).unwrap(),
                Lsn(1)
            );
            assert_eq!(
                w.append(TxnId(1), WalOp::Commit { txn: TxnId(1) }).unwrap(),
                Lsn(2)
            );
            w.sync().unwrap();
        }
        let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
        assert_eq!(w.next_lsn(), Lsn(3), "reopening must not reuse an LSN");
        assert_eq!(
            w.append(TxnId(1), WalOp::Commit { txn: TxnId(1) }).unwrap(),
            Lsn(3)
        );
    }

    #[test]
    fn strict_durability_syncs_on_every_commit() {
        let t = Tmp::new("strict");
        let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
        for _ in 0..5 {
            w.append(TxnId(1), WalOp::Commit { txn: TxnId(1) }).unwrap();
            w.commit().unwrap();
        }
        assert_eq!(w.sync_count(), 5);
    }

    #[test]
    fn relaxed_durability_never_syncs() {
        let t = Tmp::new("relaxed");
        let mut w = Wal::open(t.path(), Durability::Relaxed).unwrap();
        for _ in 0..50 {
            w.append(TxnId(1), WalOp::Commit { txn: TxnId(1) }).unwrap();
            w.commit().unwrap();
        }
        assert_eq!(w.sync_count(), 0, "Relaxed must not pay for fsync");
    }

    #[test]
    fn group_commit_syncs_far_less_often_than_strict() {
        let t = Tmp::new("group");
        let mut w = Wal::open(t.path(), Durability::GroupCommit).unwrap();
        for _ in 0..200 {
            w.append(TxnId(1), WalOp::Commit { txn: TxnId(1) }).unwrap();
            w.commit().unwrap();
        }
        let syncs = w.sync_count();
        assert!(syncs > 0, "group commit must still reach disk eventually");
        assert!(syncs < 20, "expected far fewer than 200 syncs, got {syncs}");
    }

    #[test]
    fn a_torn_tail_is_discarded_and_the_rest_survives() {
        let t = Tmp::new("torn");
        {
            let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
            for i in 0..10 {
                w.append(
                    TxnId(1),
                    WalOp::Insert {
                        collection: "c".into(),
                        id: RecordId(i),
                        bytes: vec![7; 30],
                    },
                )
                .unwrap();
            }
            w.sync().unwrap();
        }
        let seg = Wal::active_segment(t.path()).unwrap().unwrap();
        let full = std::fs::read(&seg).unwrap();
        // Cut the segment mid-entry, as a power loss would.
        std::fs::write(&seg, &full[..full.len() - 17]).unwrap();
        let read = Wal::read_all(t.path()).unwrap();
        assert!(
            read.len() >= 9 && read.len() < 10,
            "expected the torn entry dropped and the rest kept, got {}",
            read.len()
        );
        // Reopening must truncate to the intact prefix and append cleanly after.
        let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
        let lsn = w.append(TxnId(2), WalOp::Commit { txn: TxnId(2) }).unwrap();
        w.sync().unwrap();
        let after = Wal::read_all(t.path()).unwrap();
        assert_eq!(after.last().unwrap().lsn, lsn);
        assert_eq!(after.len(), read.len() + 1);
    }

    #[test]
    fn replay_stops_at_a_damaged_entry_rather_than_skipping_it() {
        let t = Tmp::new("gap");
        {
            let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
            for i in 0..6 {
                w.append(
                    TxnId(1),
                    WalOp::Insert {
                        collection: "c".into(),
                        id: RecordId(i),
                        bytes: vec![1; 20],
                    },
                )
                .unwrap();
            }
            w.sync().unwrap();
        }
        let seg = Wal::active_segment(t.path()).unwrap().unwrap();
        let mut bytes = std::fs::read(&seg).unwrap();
        // Damage an entry in the middle. Everything after it describes changes
        // that assume it happened, so replaying them would invent a state that
        // never existed.
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&seg, &bytes).unwrap();
        let read = Wal::read_all(t.path()).unwrap();
        assert!(read.len() < 6, "damaged entry did not stop replay");
    }

    #[test]
    fn an_empty_or_missing_log_reads_as_empty() {
        let t = Tmp::new("missing");
        assert!(Wal::read_all(t.path()).unwrap().is_empty());
        // A directory containing nothing that looks like a segment is an empty
        // log, not a damaged one.
        std::fs::create_dir_all(t.path()).unwrap();
        std::fs::write(t.path().join("not-a-segment.txt"), b"").unwrap();
        assert!(Wal::read_all(t.path()).unwrap().is_empty());
        let w = Wal::open(t.path(), Durability::Strict).unwrap();
        assert_eq!(w.next_lsn(), Lsn(1));
    }

    #[test]
    fn arbitrary_corruption_never_panics_or_hangs() {
        let t = Tmp::new("fuzz");
        {
            let mut w = Wal::open(t.path(), Durability::Strict).unwrap();
            for op in ops() {
                w.append(TxnId(1), op).unwrap();
            }
            w.sync().unwrap();
        }
        let seg = Wal::active_segment(t.path()).unwrap().unwrap();
        let clean = std::fs::read(&seg).unwrap();
        let mut rng = Rng::new(0xFA11);
        for _ in 0..2_000 {
            let mut b = clean.clone();
            for _ in 0..1 + rng.below_usize(4) {
                let i = rng.below_usize(b.len());
                b[i] ^= 1 << rng.below_usize(8);
            }
            std::fs::write(&seg, &b).unwrap();
            let _ = Wal::read_all(t.path());
        }
    }
}
