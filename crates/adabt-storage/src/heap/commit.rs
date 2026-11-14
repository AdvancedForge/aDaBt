//! Crash-atomic groups of writes: a transaction's write-set, or a batch of
//! inserts, survives a crash entirely or not at all.
//!
//! # The protocol
//!
//! 1. **Prepare.** Every write is validated and encoded, and every record
//!    checked to fit in a page. Nothing has been logged or touched, so any
//!    refusal here leaves no trace.
//! 2. **Log.** Each write is appended as an ordinary `Insert`, `Update` or
//!    `Delete` frame, tagged with a transaction id, followed by one
//!    [`WalOp::Commit`] for that id. The log is then made durable *once*, as
//!    the durability policy requires, and in every case handed to the
//!    operating system.
//! 3. **Apply.** Only now is any heap page or directory entry changed.
//!
//! Recovery buffers tagged frames per transaction and applies them when, and
//! only when, it reaches their `Commit`. Frames tagged `TxnId(0)` are
//! autocommit and apply as they are read, exactly as before transactional
//! frames existed, so every existing log replays unchanged and the on-disk
//! format is the one it was: `Commit` and the per-frame transaction id were
//! always part of it, and simply went unused.
//!
//! # Why this is correct
//!
//! The commit point is the `Commit` frame reaching the log. A crash before it
//! leaves at most some of the tagged frames, which recovery discards; a crash
//! after it leaves all of them, which recovery applies. That relies on the log
//! surviving as a prefix: frames are appended in order, a segment is fsynced
//! before the next one is started, and reading stops at the first damaged
//! frame. Recovery does not take the prefix on trust either — a commit record
//! whose transaction is missing a frame is reported as corruption, never
//! applied in part. There is no third case, so the outcome is all or none.
//!
//! That argument is only about the log. It also has to hold for the heap file,
//! because the buffer pool *steals*: evicting a dirty page writes it to the
//! heap file whenever the pool needs room, checkpoint or not, so a page can
//! reach the file carrying changes the log has not yet committed. A heap that
//! updates in place under a steal policy needs undo to take such a change back
//! out, and replay here is redo-only.
//!
//! Step 3 is what makes undo unnecessary. No page is changed until the
//! transaction's `Commit` is in the log, so a page the pool steals can only
//! ever carry committed changes. Nothing has to be pinned, nothing rolled
//! back, and the crash window collapses to the one question that matters:
//! did the commit record make it or not?
//!
//! Which crashes this covers follows the durability policy, as it does for
//! every other write. Under `Strict` the log is fsynced before any page
//! changes, so the guarantee holds through power loss. Under `GroupCommit` and
//! `Relaxed` the log is only flushed to the operating system first, which
//! covers the process dying. It does not cover power loss: the operating
//! system may then persist an evicted heap page and lose the log tail that
//! committed it — the same exposure every autocommit write already has under
//! those policies, which is what choosing them trades away.
//!
//! # What is not covered
//!
//! A failure in step 3 itself — an I/O error allocating a page — leaves the
//! in-memory state short of what the log already records as committed, which
//! is the same position a failed autocommit write is in. The log is the
//! authority: the next open replays the whole transaction.
//!
//! DDL is never part of a group. Definitions are logged autocommit, and pass 1
//! of recovery applies them in log order regardless of any transaction.

use adabt_core::error::Result;
use adabt_core::ids::{RecordId, TxnId};
use adabt_core::record::Record;
use adabt_core::store::normalize_for_storage;

use super::HeapStore;
use crate::wal::{RecordFrame, WalOp};

/// One write in a group committed by [`HeapStore::commit_writes`].
#[derive(Debug, Clone)]
pub enum TxnWrite {
    /// Insert or overwrite `id`.
    Put {
        collection: String,
        id: RecordId,
        record: Record,
    },
    /// Remove `id`; a no-op if it is already absent.
    Delete { collection: String, id: RecordId },
}

/// A write that has passed every check and been encoded, so that nothing
/// left to do with it can be refused.
enum Prepared {
    Put {
        collection: String,
        id: RecordId,
        bytes: Vec<u8>,
    },
    Delete {
        collection: String,
        id: RecordId,
        existed: bool,
    },
}

impl Prepared {
    /// The log frame describing this write, if it changes anything.
    ///
    /// Deleting a record that is not there changes nothing, so — exactly as
    /// for a standalone delete — nothing is logged for it.
    fn frame(&self) -> Option<RecordFrame<'_>> {
        match self {
            Prepared::Put {
                collection,
                id,
                bytes,
            } => Some(RecordFrame::Update {
                collection,
                id: *id,
                bytes,
            }),
            Prepared::Delete {
                collection,
                id,
                existed: true,
            } => Some(RecordFrame::Delete {
                collection,
                id: *id,
            }),
            Prepared::Delete { existed: false, .. } => None,
        }
    }
}

impl HeapStore {
    /// Apply every write in `writes`, in order, as one crash-atomic unit.
    ///
    /// Returns, per write, whether the record existed beforehand — what
    /// `update` and `delete` return for a single write.
    ///
    /// Either every write is validated and the whole group is logged under
    /// one commit record before any is applied, or an error is returned and
    /// nothing at all has happened. See the module documentation for why a
    /// crash at any point recovers all of the group or none of it. Under
    /// `Strict` the group costs one fsync, not one per write.
    ///
    /// Each key should appear at most once. A repeated key is still applied
    /// in order, live and on replay alike, but its "existed" answer describes
    /// the state before the group, not before its earlier write.
    pub fn commit_writes(&mut self, writes: Vec<TxnWrite>) -> Result<Vec<bool>> {
        let prepared = self.prepare_writes(writes)?;
        let existed = prepared.iter().map(|(_, existed)| *existed).collect();
        self.log_atomically(prepared.iter().filter_map(|(p, _)| p.frame()))?;
        self.apply_prepared(prepared)?;
        Ok(existed)
    }

    /// Validate and encode every write, without logging or touching anything.
    fn prepare_writes(&mut self, writes: Vec<TxnWrite>) -> Result<Vec<(Prepared, bool)>> {
        let mut prepared = Vec::with_capacity(writes.len());
        for write in writes {
            prepared.push(match write {
                TxnWrite::Put {
                    collection,
                    id,
                    mut record,
                } => {
                    normalize_for_storage(&mut record);
                    let c = self.coll(&collection)?;
                    c.codec.schema().validate_record(&record)?;
                    let existed = c.directory.get(&id).is_some_and(|v| !v.is_absent());
                    let bytes = c.codec.encode(&record)?;
                    self.check_fits(&bytes)?;
                    (
                        Prepared::Put {
                            collection,
                            id,
                            bytes,
                        },
                        existed,
                    )
                }
                TxnWrite::Delete { collection, id } => {
                    let existed = self
                        .coll(&collection)?
                        .directory
                        .get(&id)
                        .is_some_and(|v| !v.is_absent());
                    (
                        Prepared::Delete {
                            collection,
                            id,
                            existed,
                        },
                        existed,
                    )
                }
            });
        }
        Ok(prepared)
    }

    /// Apply prepared writes to the heap. Only ever called once their commit
    /// record is in the log.
    fn apply_prepared(&mut self, prepared: Vec<(Prepared, bool)>) -> Result<()> {
        for (p, _) in prepared {
            match p {
                Prepared::Put {
                    collection,
                    id,
                    bytes,
                } => self.apply_put(&collection, id, &bytes)?,
                Prepared::Delete {
                    collection,
                    id,
                    existed: true,
                } => {
                    self.apply_delete(&collection, id)?;
                }
                Prepared::Delete { existed: false, .. } => {}
            }
        }
        Ok(())
    }

    /// Log `frames` as one transaction: tagged frames, then their `Commit`,
    /// then made durable once and handed to the operating system.
    ///
    /// Takes an iterator so that a large batch is encoded into the log one
    /// frame at a time rather than copied into a second list first.
    ///
    /// The caller must not touch a page for any of these writes until this
    /// returns; that ordering is the whole of the correctness argument in the
    /// module documentation.
    pub(super) fn log_atomically<'a>(
        &mut self,
        frames: impl IntoIterator<Item = RecordFrame<'a>>,
    ) -> Result<()> {
        let mut frames = frames.into_iter().peekable();
        if self.replaying || frames.peek().is_none() {
            return Ok(());
        }
        // The first frame's own LSN: unique for the life of the log, never
        // zero (LSNs start at 1), and what lets recovery check that it holds
        // every frame between this one and the commit record.
        let txn = TxnId(self.wal.next_lsn().0);
        debug_assert_ne!(txn, TxnId(0), "LSNs start at 1; 0 means autocommit");
        for frame in frames {
            self.wal.append_record(txn, frame)?;
        }
        self.wal.append(txn, WalOp::Commit { txn })?;
        self.wal.commit()?;
        // `commit` leaves bytes in this process's buffer under `Relaxed`. A
        // page the pool evicts during step 3 goes straight to the operating
        // system, so without this a process crash could keep that page and
        // lose the commit record that justifies it.
        self.wal.flush()
    }
}
