//! Secondary indexes.
//!
//! Indexes are **derived representations**: everything here is rebuildable from
//! the primary representation, and nothing in an index is the only copy of
//! anything. That invariant is why they can live purely in memory at this
//! milestone without risking data — losing an index costs a rebuild, never a
//! record — and it is the same invariant that will later make it safe for the
//! optimizer to create and drop them under a live workload.
//!
//! Two kinds, with genuinely different capabilities rather than two names for
//! the same structure: a hash index answers equality faster and cannot answer
//! ranges at all; a B-tree answers both and costs more to maintain. Making the
//! planner choose between them is the smallest real instance of the choice the
//! whole optimizer exists to make.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;

pub use adabt_core::index_kind::IndexKind;

/// A secondary index over one field.
pub trait Index: Send {
    fn kind(&self) -> IndexKind;
    fn field(&self) -> &str;

    fn insert(&mut self, key: Value, id: RecordId);
    fn remove(&mut self, key: &Value, id: RecordId);

    /// Ids whose indexed field equals `key`, in ascending id order.
    fn lookup(&self, key: &Value) -> Vec<RecordId>;

    /// Ids whose indexed field falls in the range, ascending by key then id.
    /// Returns `None` when the index cannot answer ranges.
    fn range(&self, lo: Bound<&Value>, hi: Bound<&Value>) -> Option<Vec<RecordId>>;

    /// Every key with the ids filed under it.
    ///
    /// For persisting a rebuilt copy. Order is unspecified on purpose: an index
    /// is a set, both kinds reconstruct the same structure from the same
    /// entries however they arrive, and promising an order here would be a
    /// promise the hash kind cannot keep.
    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)>;

    /// Distinct keys held.
    fn key_count(&self) -> usize;
    /// Entries held, counting duplicates.
    fn entry_count(&self) -> usize;
    /// Approximate heap footprint, for the resource axis of a cost estimate.
    fn memory_bytes(&self) -> usize;

    /// Index a record, ignoring it when the field is absent.
    fn index_record(&mut self, id: RecordId, rec: &Record) {
        if let Some(v) = rec.get(self.field()) {
            if !v.is_null() {
                self.insert(v.clone(), id);
            }
        }
    }

    fn unindex_record(&mut self, id: RecordId, rec: &Record) {
        if let Some(v) = rec.get(self.field()) {
            if !v.is_null() {
                self.remove(&v.clone(), id);
            }
        }
    }

    /// Fraction of entries that share their key with another entry. A
    /// selectivity near zero means the index pinpoints records; near one means
    /// it barely narrows anything and is probably not worth its upkeep.
    fn selectivity(&self) -> f64 {
        if self.entry_count() == 0 {
            return 0.0;
        }
        self.key_count() as f64 / self.entry_count() as f64
    }
}

/// Rough heap cost of holding one value.
fn value_bytes(v: &Value) -> usize {
    const BASE: usize = std::mem::size_of::<Value>();
    BASE + match v {
        Value::Str(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::List(items) => items.iter().map(value_bytes).sum(),
        Value::Map(m) => m.iter().map(|(k, v)| k.len() + value_bytes(v)).sum(),
        _ => 0,
    }
}

const ID_BYTES: usize = std::mem::size_of::<RecordId>();
/// Per-entry overhead of the backing map: node pointers, hash slots, headers.
const NODE_OVERHEAD: usize = 48;

/// The bookkeeping both index kinds share: sorted, de-duplicated id lists per
/// key, plus a running memory estimate.
///
/// Written once against a small map abstraction rather than duplicated per
/// kind. The subtle parts — keeping ids sorted so lookups are stable, dropping
/// a key when its last entry goes, keeping the byte estimate honest in both
/// directions — are exactly the parts that must not drift between two copies.
trait KeyMap: Default {
    fn get(&self, k: &Value) -> Option<&Vec<RecordId>>;
    fn get_mut(&mut self, k: &Value) -> Option<&mut Vec<RecordId>>;
    fn entry_or_default(&mut self, k: Value) -> &mut Vec<RecordId>;
    fn remove(&mut self, k: &Value);
    fn pairs(&self) -> Vec<(Value, Vec<RecordId>)>;
}

impl KeyMap for HashMap<Value, Vec<RecordId>> {
    fn get(&self, k: &Value) -> Option<&Vec<RecordId>> {
        HashMap::get(self, k)
    }
    fn get_mut(&mut self, k: &Value) -> Option<&mut Vec<RecordId>> {
        HashMap::get_mut(self, k)
    }
    fn entry_or_default(&mut self, k: Value) -> &mut Vec<RecordId> {
        self.entry(k).or_default()
    }
    fn remove(&mut self, k: &Value) {
        HashMap::remove(self, k);
    }
    fn pairs(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

impl KeyMap for BTreeMap<Value, Vec<RecordId>> {
    fn get(&self, k: &Value) -> Option<&Vec<RecordId>> {
        BTreeMap::get(self, k)
    }
    fn get_mut(&mut self, k: &Value) -> Option<&mut Vec<RecordId>> {
        BTreeMap::get_mut(self, k)
    }
    fn entry_or_default(&mut self, k: Value) -> &mut Vec<RecordId> {
        self.entry(k).or_default()
    }
    fn remove(&mut self, k: &Value) {
        BTreeMap::remove(self, k);
    }
    fn pairs(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

struct Core<M: KeyMap> {
    field: String,
    map: M,
    entries: usize,
    bytes: usize,
}

impl<M: KeyMap> Core<M> {
    fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            map: M::default(),
            entries: 0,
            bytes: 0,
        }
    }

    fn insert(&mut self, key: Value, id: RecordId) {
        let kb = value_bytes(&key);
        let slot = self.map.entry_or_default(key);
        if slot.is_empty() {
            self.bytes += kb + NODE_OVERHEAD;
        }
        // Sorted and unique: lookups return a stable order, and re-indexing the
        // same record cannot duplicate it.
        match slot.binary_search(&id) {
            Ok(_) => return,
            Err(at) => slot.insert(at, id),
        }
        self.entries += 1;
        self.bytes += ID_BYTES;
    }

    fn remove(&mut self, key: &Value, id: RecordId) {
        let Some(slot) = self.map.get_mut(key) else {
            return;
        };
        if let Ok(at) = slot.binary_search(&id) {
            slot.remove(at);
            self.entries -= 1;
            self.bytes = self.bytes.saturating_sub(ID_BYTES);
        }
        if slot.is_empty() {
            self.bytes = self.bytes.saturating_sub(value_bytes(key) + NODE_OVERHEAD);
            self.map.remove(key);
        }
    }

    fn lookup(&self, key: &Value) -> Vec<RecordId> {
        self.map.get(key).cloned().unwrap_or_default()
    }

    fn memory_bytes(&self) -> usize {
        self.field.len() + self.bytes
    }
}

macro_rules! declare_index {
    ($name:ident, $map:ty, $kind:expr) => {
        pub struct $name {
            core: Core<$map>,
        }

        impl $name {
            pub fn new(field: impl Into<String>) -> Self {
                Self {
                    core: Core::new(field),
                }
            }

            /// Build from an existing dataset.
            ///
            /// This is what makes index creation reversible: a dropped index is
            /// reconstructible from the primary representation at any time,
            /// which is why the optimizer may create and destroy them freely.
            pub fn build<'a>(
                field: impl Into<String>,
                records: impl Iterator<Item = (RecordId, &'a Record)>,
            ) -> Self {
                let mut idx = Self::new(field);
                for (id, rec) in records {
                    idx.index_record(id, rec);
                }
                idx
            }
        }
    };
}

declare_index!(HashIndex, HashMap<Value, Vec<RecordId>>, IndexKind::Hash);
declare_index!(BTreeIndex, BTreeMap<Value, Vec<RecordId>>, IndexKind::BTree);

// -- bitmap ------------------------------------------------------------

/// A raw bitset over record ids: one bit per id, packed 64 to a word.
///
/// Grows on demand — the word vector is only ever as long as the highest id
/// set so far requires — rather than being pre-sized to some assumed
/// collection size, so a bitmap over a small or sparse id range costs only
/// what it actually spans.
#[derive(Default, Clone)]
struct Bitset {
    words: Vec<u64>,
}

impl Bitset {
    fn set(&mut self, id: u64) {
        let word = (id / 64) as usize;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << (id % 64);
    }

    fn clear(&mut self, id: u64) {
        let word = (id / 64) as usize;
        if word < self.words.len() {
            self.words[word] &= !(1 << (id % 64));
        }
    }

    fn get(&self, id: u64) -> bool {
        let word = (id / 64) as usize;
        word < self.words.len() && (self.words[word] & (1 << (id % 64))) != 0
    }

    fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Set bits, ascending — a bitset's natural iteration order already is
    /// id order, unlike `HashMap`'s, so no sort is needed to meet `Index::
    /// lookup`'s "ascending id order" promise.
    fn ids(&self) -> Vec<RecordId> {
        let mut out = Vec::new();
        for (wi, &w) in self.words.iter().enumerate() {
            let mut bits = w;
            while bits != 0 {
                let tz = bits.trailing_zeros();
                out.push(RecordId(wi as u64 * 64 + tz as u64));
                bits &= bits - 1; // clear the lowest set bit
            }
        }
        out
    }

    fn memory_bytes(&self) -> usize {
        self.words.len() * 8
    }
}

/// One bit per record id, per distinct value — see `IndexKind::Bitmap` for
/// when this earns its keep over `HashIndex`.
///
/// Hand-written rather than a `declare_index!` instance: `Core<M: KeyMap>`
/// is built specifically around a `Vec<RecordId>` per key, and a bitmap's
/// per-key value is a `Bitset` instead — genuinely different bookkeeping
/// (`set`/`clear`/`is_empty` in place of push/binary-search/remove), not a
/// parameter the shared core could take without stopping being one honest
/// abstraction for two structures.
pub struct BitmapIndex {
    field: String,
    map: HashMap<Value, Bitset>,
    entries: usize,
}

impl BitmapIndex {
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            map: HashMap::new(),
            entries: 0,
        }
    }

    /// Build from an existing dataset — the same reversibility every index
    /// kind has: a dropped bitmap index costs a rebuild, never a record.
    pub fn build<'a>(
        field: impl Into<String>,
        records: impl Iterator<Item = (RecordId, &'a Record)>,
    ) -> Self {
        let mut idx = Self::new(field);
        for (id, rec) in records {
            idx.index_record(id, rec);
        }
        idx
    }
}

impl Index for BitmapIndex {
    fn kind(&self) -> IndexKind {
        IndexKind::Bitmap
    }
    fn field(&self) -> &str {
        &self.field
    }

    fn insert(&mut self, key: Value, id: RecordId) {
        let bits = self.map.entry(key).or_default();
        if !bits.get(id.0) {
            bits.set(id.0);
            self.entries += 1;
        }
    }

    fn remove(&mut self, key: &Value, id: RecordId) {
        let Some(bits) = self.map.get_mut(key) else {
            return;
        };
        if bits.get(id.0) {
            bits.clear(id.0);
            self.entries -= 1;
        }
        if bits.is_empty() {
            self.map.remove(key);
        }
    }

    fn lookup(&self, key: &Value) -> Vec<RecordId> {
        self.map.get(key).map(Bitset::ids).unwrap_or_default()
    }

    /// Declines, the same way `HashIndex` does and for the same reason: it
    /// would need its keys visited in sorted order, and a `HashMap` does not
    /// keep one.
    fn range(&self, _lo: Bound<&Value>, _hi: Bound<&Value>) -> Option<Vec<RecordId>> {
        None
    }

    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.map.iter().map(|(k, b)| (k.clone(), b.ids())).collect()
    }

    fn key_count(&self) -> usize {
        self.map.len()
    }
    fn entry_count(&self) -> usize {
        self.entries
    }
    fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.field.len()
            + self
                .map
                .iter()
                .map(|(k, b)| value_bytes(k) + b.memory_bytes() + NODE_OVERHEAD)
                .sum::<usize>()
    }
}

impl Index for HashIndex {
    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.core.map.pairs()
    }
    fn kind(&self) -> IndexKind {
        IndexKind::Hash
    }
    fn field(&self) -> &str {
        &self.core.field
    }
    fn insert(&mut self, key: Value, id: RecordId) {
        self.core.insert(key, id)
    }
    fn remove(&mut self, key: &Value, id: RecordId) {
        self.core.remove(key, id)
    }
    fn lookup(&self, key: &Value) -> Vec<RecordId> {
        self.core.lookup(key)
    }
    /// Declines rather than answering wrongly.
    ///
    /// `None` and `Some(vec![])` are different answers: the second means
    /// "nothing matched", and a planner that confused them would silently drop
    /// every row in the range.
    fn range(&self, _lo: Bound<&Value>, _hi: Bound<&Value>) -> Option<Vec<RecordId>> {
        None
    }
    fn key_count(&self) -> usize {
        self.core.map.len()
    }
    fn entry_count(&self) -> usize {
        self.core.entries
    }
    fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.core.memory_bytes()
    }
}

impl Index for BTreeIndex {
    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.core.map.pairs()
    }
    fn kind(&self) -> IndexKind {
        IndexKind::BTree
    }
    fn field(&self) -> &str {
        &self.core.field
    }
    fn insert(&mut self, key: Value, id: RecordId) {
        self.core.insert(key, id)
    }
    fn remove(&mut self, key: &Value, id: RecordId) {
        self.core.remove(key, id)
    }
    fn lookup(&self, key: &Value) -> Vec<RecordId> {
        self.core.lookup(key)
    }
    fn range(&self, lo: Bound<&Value>, hi: Bound<&Value>) -> Option<Vec<RecordId>> {
        let mut out = Vec::new();
        for ids in self.core.map.range((lo, hi)).map(|(_, v)| v) {
            out.extend_from_slice(ids);
        }
        Some(out)
    }
    fn key_count(&self) -> usize {
        self.core.map.len()
    }
    fn entry_count(&self) -> usize {
        self.core.entries
    }
    fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.core.memory_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(v: i64) -> Record {
        Record::new().with("k", v)
    }

    fn filled<I: Index>(mut idx: I) -> I {
        for i in 0..100u64 {
            idx.index_record(RecordId(i), &rec((i % 10) as i64));
        }
        idx
    }

    #[test]
    fn lookup_finds_every_matching_id_in_order() {
        for mut idx in [
            Box::new(filled(HashIndex::new("k"))) as Box<dyn Index>,
            Box::new(filled(BTreeIndex::new("k"))),
            Box::new(filled(BitmapIndex::new("k"))),
        ] {
            let got = idx.lookup(&Value::I64(3));
            assert_eq!(got.len(), 10, "{}", idx.kind().as_str());
            assert!(got.windows(2).all(|w| w[0] < w[1]), "ids not ascending");
            assert!(idx.lookup(&Value::I64(999)).is_empty());
            let _ = &mut idx;
        }
    }

    #[test]
    fn removal_is_exact_and_leaves_siblings_alone() {
        let mut idx = filled(BTreeIndex::new("k"));
        let before = idx.lookup(&Value::I64(3));
        idx.unindex_record(RecordId(3), &rec(3));
        let after = idx.lookup(&Value::I64(3));
        assert_eq!(after.len(), before.len() - 1);
        assert!(!after.contains(&RecordId(3)));
        assert_eq!(
            idx.lookup(&Value::I64(4)).len(),
            10,
            "neighbouring key damaged"
        );
    }

    #[test]
    fn removing_the_last_entry_drops_the_key() {
        let mut idx = HashIndex::new("k");
        idx.index_record(RecordId(1), &rec(7));
        assert_eq!(idx.key_count(), 1);
        idx.unindex_record(RecordId(1), &rec(7));
        assert_eq!(idx.key_count(), 0);
        assert_eq!(idx.entry_count(), 0);
    }

    #[test]
    fn indexing_the_same_record_twice_does_not_duplicate_it() {
        let mut idx = BTreeIndex::new("k");
        idx.index_record(RecordId(1), &rec(5));
        idx.index_record(RecordId(1), &rec(5));
        assert_eq!(idx.lookup(&Value::I64(5)), vec![RecordId(1)]);
        assert_eq!(idx.entry_count(), 1);
    }

    #[test]
    fn removing_something_absent_is_harmless() {
        let mut idx = HashIndex::new("k");
        idx.unindex_record(RecordId(1), &rec(5));
        idx.index_record(RecordId(2), &rec(5));
        idx.remove(&Value::I64(5), RecordId(99));
        assert_eq!(idx.entry_count(), 1);
    }

    #[test]
    fn records_missing_the_field_are_not_indexed() {
        let mut idx = BTreeIndex::new("k");
        idx.index_record(RecordId(1), &Record::new().with("other", 1i64));
        idx.index_record(RecordId(2), &Record::new().with("k", Value::Null));
        assert_eq!(idx.entry_count(), 0);
    }

    #[test]
    fn a_btree_answers_ranges_and_a_hash_index_does_not() {
        let bt = filled(BTreeIndex::new("k"));
        let got = bt
            .range(
                Bound::Included(&Value::I64(2)),
                Bound::Excluded(&Value::I64(5)),
            )
            .expect("btree must answer ranges");
        // Keys 2, 3 and 4, ten ids each.
        assert_eq!(got.len(), 30);

        let hash = filled(HashIndex::new("k"));
        assert!(
            hash.range(Bound::Unbounded, Bound::Unbounded).is_none(),
            "a hash index must decline ranges rather than answer them wrongly"
        );
        assert!(!IndexKind::Hash.supports_range());
        assert!(IndexKind::BTree.supports_range());
    }

    #[test]
    fn range_bounds_are_respected() {
        let mut bt = BTreeIndex::new("k");
        for i in 0..10i64 {
            bt.index_record(RecordId(i as u64), &rec(i));
        }
        let count = |lo, hi| bt.range(lo, hi).unwrap().len();
        assert_eq!(
            count(
                Bound::Included(&Value::I64(3)),
                Bound::Included(&Value::I64(5))
            ),
            3
        );
        assert_eq!(
            count(
                Bound::Excluded(&Value::I64(3)),
                Bound::Excluded(&Value::I64(5))
            ),
            1
        );
        assert_eq!(count(Bound::Unbounded, Bound::Excluded(&Value::I64(3))), 3);
        assert_eq!(count(Bound::Included(&Value::I64(7)), Bound::Unbounded), 3);
        assert_eq!(count(Bound::Unbounded, Bound::Unbounded), 10);
    }

    #[test]
    fn range_results_are_ordered_by_key() {
        let mut bt = BTreeIndex::new("k");
        for i in (0..20i64).rev() {
            bt.index_record(RecordId(i as u64), &rec(i));
        }
        let got = bt.range(Bound::Unbounded, Bound::Unbounded).unwrap();
        let ids: Vec<u64> = got.iter().map(|r| r.0).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "range must come back in key order");
    }

    #[test]
    fn selectivity_distinguishes_a_useful_index_from_a_useless_one() {
        // Unique keys: perfectly selective.
        let mut unique = BTreeIndex::new("k");
        for i in 0..100i64 {
            unique.index_record(RecordId(i as u64), &rec(i));
        }
        assert!((unique.selectivity() - 1.0).abs() < 1e-9);

        // One key for everything: narrows nothing.
        let mut useless = BTreeIndex::new("k");
        for i in 0..100u64 {
            useless.index_record(RecordId(i), &rec(0));
        }
        assert!(useless.selectivity() < 0.02, "{}", useless.selectivity());

        assert_eq!(BTreeIndex::new("k").selectivity(), 0.0, "empty index");
    }

    #[test]
    fn memory_grows_with_content_and_shrinks_when_emptied() {
        let mut idx = BTreeIndex::new("k");
        let empty = idx.memory_bytes();
        for i in 0..500u64 {
            idx.index_record(RecordId(i), &Record::new().with("k", format!("key-{i}")));
        }
        let full = idx.memory_bytes();
        assert!(
            full > empty * 4,
            "memory estimate did not grow: {empty} -> {full}"
        );
        for i in 0..500u64 {
            idx.unindex_record(RecordId(i), &Record::new().with("k", format!("key-{i}")));
        }
        assert!(
            idx.memory_bytes() < full / 4,
            "memory estimate did not shrink back: {} vs {full}",
            idx.memory_bytes()
        );
    }

    #[test]
    fn build_reconstructs_an_index_from_records() {
        let records: Vec<(RecordId, Record)> = (0..50u64)
            .map(|i| (RecordId(i), rec((i % 5) as i64)))
            .collect();
        let idx = BTreeIndex::build("k", records.iter().map(|(i, r)| (*i, r)));
        assert_eq!(idx.entry_count(), 50);
        assert_eq!(idx.key_count(), 5);
        assert_eq!(idx.lookup(&Value::I64(2)).len(), 10);
    }

    // -- bitmap -------------------------------------------------------------

    #[test]
    fn a_bitmap_index_declines_ranges_like_a_hash_index_does() {
        let bm = filled(BitmapIndex::new("k"));
        assert!(
            bm.range(Bound::Unbounded, Bound::Unbounded).is_none(),
            "a bitmap index must decline ranges rather than answer them wrongly"
        );
        assert!(!IndexKind::Bitmap.supports_range());
    }

    #[test]
    fn bitmap_ids_are_correct_across_word_boundaries() {
        // 64-bit words: 63, 64 and 65 land in different words (or the edge
        // of one), which is exactly where an off-by-one in the shift/mask
        // arithmetic would show up.
        let mut idx = BitmapIndex::new("k");
        for i in [0u64, 1, 63, 64, 65, 127, 128, 1000] {
            idx.index_record(RecordId(i), &rec(1));
        }
        let mut got: Vec<u64> = idx.lookup(&Value::I64(1)).iter().map(|r| r.0).collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 63, 64, 65, 127, 128, 1000]);
    }

    #[test]
    fn bitmap_removal_clears_exactly_one_bit() {
        let mut idx = BitmapIndex::new("k");
        for i in 60..70u64 {
            idx.index_record(RecordId(i), &rec(1));
        }
        idx.unindex_record(RecordId(64), &rec(1));
        let got: Vec<u64> = idx.lookup(&Value::I64(1)).iter().map(|r| r.0).collect();
        assert!(!got.contains(&64));
        assert_eq!(got.len(), 9);
    }

    #[test]
    fn removing_a_bitmap_indexs_last_entry_drops_the_key() {
        let mut idx = BitmapIndex::new("k");
        idx.index_record(RecordId(5), &rec(9));
        assert_eq!(idx.key_count(), 1);
        idx.unindex_record(RecordId(5), &rec(9));
        assert_eq!(idx.key_count(), 0);
        assert_eq!(idx.entry_count(), 0);
    }

    #[test]
    fn indexing_the_same_bitmap_record_twice_does_not_duplicate_it() {
        let mut idx = BitmapIndex::new("k");
        idx.index_record(RecordId(1), &rec(5));
        idx.index_record(RecordId(1), &rec(5));
        assert_eq!(idx.lookup(&Value::I64(5)), vec![RecordId(1)]);
        assert_eq!(idx.entry_count(), 1);
    }

    #[test]
    fn a_bitmap_index_is_cheaper_than_hash_for_many_rows_sharing_few_values() {
        // The whole reason `Bitmap` exists: low cardinality, high fan-out per
        // key, where `Hash`'s per-entry `Vec<RecordId>` overhead adds up and
        // a bitmap's per-entry cost is one bit.
        let n = 5_000u64;
        let mut hash = HashIndex::new("k");
        let mut bitmap = BitmapIndex::new("k");
        for i in 0..n {
            let v = (i % 4) as i64; // four distinct values
            hash.index_record(RecordId(i), &rec(v));
            bitmap.index_record(RecordId(i), &rec(v));
        }
        assert!(
            bitmap.memory_bytes() < hash.memory_bytes(),
            "bitmap {} should be smaller than hash {} at this cardinality",
            bitmap.memory_bytes(),
            hash.memory_bytes()
        );
    }

    #[test]
    fn bitmap_memory_grows_with_content_and_shrinks_when_emptied() {
        let mut idx = BitmapIndex::new("k");
        let empty = idx.memory_bytes();
        for i in 0..500u64 {
            idx.index_record(RecordId(i), &rec((i % 3) as i64));
        }
        let full = idx.memory_bytes();
        assert!(
            full > empty,
            "memory estimate did not grow: {empty} -> {full}"
        );
        for i in 0..500u64 {
            idx.unindex_record(RecordId(i), &rec((i % 3) as i64));
        }
        assert!(
            idx.memory_bytes() < full,
            "memory estimate did not shrink back: {} vs {full}",
            idx.memory_bytes()
        );
    }

    #[test]
    fn bitmap_build_reconstructs_an_index_from_records() {
        let records: Vec<(RecordId, Record)> = (0..50u64)
            .map(|i| (RecordId(i), rec((i % 5) as i64)))
            .collect();
        let idx = BitmapIndex::build("k", records.iter().map(|(i, r)| (*i, r)));
        assert_eq!(idx.entry_count(), 50);
        assert_eq!(idx.key_count(), 5);
        assert_eq!(idx.lookup(&Value::I64(2)).len(), 10);
    }
}
