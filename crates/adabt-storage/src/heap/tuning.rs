//! Runtime-tunable physical settings: compression, delta encoding, the
//! thread-per-core flag, log segmenting and archiving, read-ahead, and the
//! buffer pool's capacity.

use adabt_core::error::Result;
use adabt_core::store::LogicalStore;

use super::migration::is_migration_name;
use super::HeapStore;
use crate::page::RecordLocation;
use crate::wal::RecordFrame;

impl HeapStore {
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

    pub fn compression_enabled(&self) -> bool {
        self.compress
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
                self.log_record(RecordFrame::Update {
                    collection: &name,
                    id,
                    bytes: &bytes,
                })?;
                self.apply_put(&name, id, &bytes)?;
            }
        }
        Ok(self.stored_bytes()? as i64 - before)
    }

    pub(super) fn store_collection_names(&self) -> Vec<String> {
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

    pub fn set_pool_capacity(&mut self, pages: usize) -> Result<()> {
        self.pool.set_capacity(pages)
    }
}
