//! Runtime-tunable physical settings: compression, delta encoding, the
//! thread-per-core flag, log segmenting and archiving, read-ahead, and the
//! buffer pool's capacity.

use adabt_core::error::{Error, Result};
use adabt_core::store::LogicalStore;

use super::migration::is_migration_name;
use super::{HeapStore, SLOT_PREFIX};
use crate::compress::{compressed_len, decoded_len, Encoding};
use crate::page::RecordLocation;
use crate::wal::RecordFrame;

/// Records sampled per collection to measure how well they compress.
///
/// Enough to read a ratio to a few percent on uniform data, and few enough
/// that measuring it every optimization cycle costs well under a
/// millisecond.
pub const FOOTPRINT_SAMPLE: usize = 256;

/// One collection's records, sized uncompressed and as compression would
/// store them. See [`HeapStore::record_footprint`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RecordFootprint {
    /// Live records.
    pub records: u64,
    /// Bytes those records take uncompressed, whatever is stored now,
    /// extrapolated from the sample.
    pub raw_bytes: u64,
    /// Compressed over uncompressed bytes, on a sample of the records, as
    /// compression would store them: 1.0 when nothing shrinks. `None` with
    /// no records to measure.
    pub compression_ratio: Option<f64>,
}

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

    /// What each collection's live records take uncompressed, and how far
    /// compression would shrink them.
    ///
    /// Both figures come from an evenly spaced sample of at most
    /// [`FOOTPRINT_SAMPLE`] records, so a measurement reads a bounded number
    /// of pages however large the collection: the optimizer asks every
    /// cycle, and a full pass would put a scan of the database inside every
    /// measured window. Each sampled record is sized from its slot's framing
    /// (no decompression) and as `maybe_compress` would store it, and neither
    /// depends on the current setting, since a record already compressed
    /// holds the body `maybe_compress` chose. An estimate built on these
    /// claims the same saving before and after it is acted on, rather than a
    /// smaller one once the saving has been made.
    ///
    /// Collections are in name order; one with no live records has no ratio.
    pub fn record_footprint(&mut self) -> Result<Vec<(String, RecordFootprint)>> {
        let mut out = Vec::new();
        for name in self.store_collection_names() {
            let (records, sample) = self.sample_live(&name)?;
            let (mut sampled_raw, mut sampled_packed) = (0u64, 0u64);
            for loc in &sample {
                let page = self.pool.get(loc.page)?;
                let payload = page.get(loc.slot)?;
                if payload.len() < SLOT_PREFIX {
                    return Err(Error::Corruption(
                        "stored slot is shorter than its prefix".into(),
                    ));
                }
                let encoding = Encoding::from_bit(payload[SLOT_PREFIX - 1])?;
                let body = &payload[SLOT_PREFIX..];
                sampled_raw += decoded_len(encoding, body).ok_or_else(|| {
                    Error::Corruption("compressed record is missing its length".into())
                })? as u64;
                sampled_packed += compressed_len(encoding, body)? as u64;
            }
            let mut fp = RecordFootprint {
                records: records as u64,
                ..RecordFootprint::default()
            };
            if !sample.is_empty() {
                fp.raw_bytes = (sampled_raw as f64 * records as f64 / sample.len() as f64) as u64;
            }
            if sampled_raw > 0 {
                fp.compression_ratio = Some(sampled_packed as f64 / sampled_raw as f64);
            }
            out.push((name, fp));
        }
        Ok(out)
    }

    /// [`stored_bytes`](Self::stored_bytes) estimated from the same bounded
    /// sample [`record_footprint`](Self::record_footprint) reads, for callers
    /// that ask every optimization cycle and cannot afford a pass over every
    /// record.
    pub fn sampled_stored_bytes(&mut self) -> Result<u64> {
        let mut total = 0.0;
        for name in self.store_collection_names() {
            let (records, sample) = self.sample_live(&name)?;
            if sample.is_empty() {
                continue;
            }
            let mut sampled = 0u64;
            for loc in &sample {
                sampled += self.pool.get(loc.page)?.get(loc.slot)?.len() as u64;
            }
            total += sampled as f64 * records as f64 / sample.len() as f64;
        }
        Ok(total as u64)
    }

    /// How many live records a collection holds, and an evenly spaced
    /// sample of at most [`FOOTPRINT_SAMPLE`] of their locations. Reads only
    /// the in-memory directory.
    fn sample_live(&self, name: &str) -> Result<(usize, Vec<RecordLocation>)> {
        let directory = &self.coll(name)?.directory;
        let records = directory.live();
        let stride = records.div_ceil(FOOTPRINT_SAMPLE).max(1);
        let sample = directory
            .values()
            .filter_map(|v| v.newest())
            .step_by(stride)
            .collect();
        Ok((records, sample))
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
