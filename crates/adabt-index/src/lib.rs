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

    /// Fields this index stores alongside the ids. Empty for an ordinary one.
    ///
    /// An index that carries a projection of each indexed record can answer a
    /// query *entirely from the index*, with no fetch at all. That matters
    /// here more than it does in most engines: the fetch is the measured
    /// majority of what a lookup costs (see `docs/m36-notes.md`), so removing
    /// it is not a constant-factor saving but the removal of the dominant
    /// term.
    fn covers(&self) -> &[String] {
        &[]
    }

    /// The stored projection for `id`, when this index carries one.
    ///
    /// Returning `None` means "no projection here" and never "the record has
    /// no fields" — the same `None`-versus-empty distinction `range` makes,
    /// and for the same reason: a caller that confused them would serve empty
    /// records instead of falling back to the heap.
    fn covered(&self, _id: RecordId) -> Option<&Record> {
        None
    }

    /// Whether this index holds only the records satisfying some condition.
    ///
    /// A partial index is smaller and cheaper to maintain, and is only a legal
    /// access path for a query whose own predicate guarantees every row it
    /// wants is present. `None` means the index holds every record with the
    /// field, which is always safe to read.
    fn condition(&self) -> Option<&str> {
        None
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

// -- composite ---------------------------------------------------------

/// Separator joining a composite index's field names into the single name
/// `Index::field` must return.
///
/// A NUL cannot appear in a field name the logical API accepts, so the join
/// is unambiguous and a composite index can never collide with a
/// single-field one — which also means the existing single-field planner
/// path simply never matches a composite index, rather than matching one
/// wrongly.
pub const COMPOSITE_SEP: char = '\u{0}';

/// The name a composite index over `fields` answers to.
pub fn composite_name(fields: &[String]) -> String {
    fields.join(&COMPOSITE_SEP.to_string())
}

/// Split a composite index's name back into its fields.
pub fn composite_fields(name: &str) -> Vec<String> {
    name.split(COMPOSITE_SEP).map(|s| s.to_string()).collect()
}

/// An index over several fields at once.
///
/// # Why this needed no new key type
///
/// A composite key is just the tuple of its fields' values, and `Value`
/// already has a `List` variant with a total `Ord` and a `Hash` that agrees
/// with it. So the key is `Value::List(vec![v1, v2, ...])` and the whole
/// existing `Core`/`KeyMap` machinery — sorted id lists, key removal when
/// the last entry goes, the running memory estimate — works unchanged.
/// Inventing a parallel multi-value key type would have duplicated all of
/// that for no gain.
///
/// # What it deliberately does not do
///
/// **Equality on every field, or nothing.** A composite index over
/// `(a, b)` serves `a = 1 AND b = 2`. It does *not* serve `a = 1` alone:
/// that would need the key ordering to be a prefix ordering and the lookup
/// to be a range scan over it, which a hash-backed structure cannot do at
/// all. `BTreeIndex`-backed prefix lookups are a real extension and are not
/// claimed here — see `supports_prefix_lookup`, which returns false, rather
/// than a comment nobody checks.
///
/// A record missing *any* of the fields is not indexed, matching the
/// single-field rule that a record without the field is simply absent.
pub struct CompositeIndex {
    fields: Vec<String>,
    name: String,
    core: Core<HashMap<Value, Vec<RecordId>>>,
}

impl CompositeIndex {
    pub fn new(fields: Vec<String>) -> Self {
        let name = composite_name(&fields);
        Self {
            core: Core::new(name.clone()),
            fields,
            name,
        }
    }

    pub fn build<'a>(
        fields: Vec<String>,
        records: impl Iterator<Item = (RecordId, &'a Record)>,
    ) -> Self {
        let mut idx = Self::new(fields);
        for (id, rec) in records {
            idx.index_record(id, rec);
        }
        idx
    }

    /// The fields this index covers, in order.
    pub fn fields(&self) -> &[String] {
        &self.fields
    }

    /// Whether this index can answer a lookup on a proper prefix of its
    /// fields. It cannot: see the type docs.
    pub fn supports_prefix_lookup(&self) -> bool {
        false
    }

    /// The key for a record, or `None` when any covered field is absent or
    /// null — the composite equivalent of the single-field rule that a
    /// record without the field is not indexed.
    fn key_for(&self, rec: &Record) -> Option<Value> {
        let mut parts = Vec::with_capacity(self.fields.len());
        for f in &self.fields {
            let v = rec.get(f)?;
            if v.is_null() {
                return None;
            }
            parts.push(v.clone());
        }
        Some(Value::List(parts))
    }

    /// The key a set of equality constraints implies, when they cover every
    /// field of this index. `None` when any field is unconstrained — which
    /// is exactly when this index cannot serve the predicate.
    pub fn key_from_equalities(&self, equalities: &[(String, Value)]) -> Option<Value> {
        let mut parts = Vec::with_capacity(self.fields.len());
        for f in &self.fields {
            let v = equalities.iter().find(|(name, _)| name == f)?;
            parts.push(v.1.clone());
        }
        Some(Value::List(parts))
    }
}

impl Index for CompositeIndex {
    fn kind(&self) -> IndexKind {
        IndexKind::Hash
    }
    fn field(&self) -> &str {
        &self.name
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
    /// Declines, like every hash-backed index here.
    fn range(&self, _lo: Bound<&Value>, _hi: Bound<&Value>) -> Option<Vec<RecordId>> {
        None
    }
    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.core.map.pairs()
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

    /// Overridden because the composite key spans several fields; the
    /// default implementation reads exactly one.
    fn index_record(&mut self, id: RecordId, rec: &Record) {
        if let Some(k) = self.key_for(rec) {
            self.insert(k, id);
        }
    }

    fn unindex_record(&mut self, id: RecordId, rec: &Record) {
        if let Some(k) = self.key_for(rec) {
            self.remove(&k, id);
        }
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;

    fn rec(a: i64, b: &str) -> Record {
        Record::new().with("a", a).with("b", b)
    }

    fn idx() -> CompositeIndex {
        CompositeIndex::new(vec!["a".into(), "b".into()])
    }

    #[test]
    fn a_composite_index_matches_only_the_full_tuple() {
        let mut i = idx();
        i.index_record(RecordId(1), &rec(1, "x"));
        i.index_record(RecordId(2), &rec(1, "y"));
        i.index_record(RecordId(3), &rec(2, "x"));

        let key = |a: i64, b: &str| Value::List(vec![Value::I64(a), Value::Str(b.into())]);
        assert_eq!(i.lookup(&key(1, "x")), vec![RecordId(1)]);
        assert_eq!(i.lookup(&key(1, "y")), vec![RecordId(2)]);
        assert!(i.lookup(&key(9, "z")).is_empty());
        assert_eq!(i.entry_count(), 3);
        assert_eq!(i.key_count(), 3);
    }

    #[test]
    fn a_record_missing_any_covered_field_is_not_indexed() {
        let mut i = idx();
        i.index_record(RecordId(1), &Record::new().with("a", 1i64));
        i.index_record(RecordId(2), &Record::new().with("b", "x"));
        i.index_record(
            RecordId(3),
            &Record::new().with("a", 1i64).with("b", Value::Null),
        );
        assert_eq!(i.entry_count(), 0, "a partial record was indexed");
    }

    #[test]
    fn equalities_covering_every_field_produce_a_key() {
        let i = idx();
        let eqs = vec![
            ("b".to_string(), Value::Str("x".into())),
            ("a".to_string(), Value::I64(1)),
        ];
        // Order of the constraints must not matter; the index's own field
        // order is what fixes the key.
        assert_eq!(
            i.key_from_equalities(&eqs),
            Some(Value::List(vec![Value::I64(1), Value::Str("x".into())]))
        );
    }

    #[test]
    fn equalities_missing_a_field_produce_no_key() {
        // The case that makes a composite index unusable for a predicate,
        // and the reason the planner must ask before choosing one.
        let i = idx();
        let eqs = vec![("a".to_string(), Value::I64(1))];
        assert_eq!(i.key_from_equalities(&eqs), None);
        assert!(!i.supports_prefix_lookup());
    }

    #[test]
    fn removal_is_exact() {
        let mut i = idx();
        i.index_record(RecordId(1), &rec(1, "x"));
        i.index_record(RecordId(2), &rec(1, "x"));
        i.unindex_record(RecordId(1), &rec(1, "x"));
        let key = Value::List(vec![Value::I64(1), Value::Str("x".into())]);
        assert_eq!(i.lookup(&key), vec![RecordId(2)]);
    }

    #[test]
    fn names_round_trip_through_the_separator() {
        let fields = vec!["country".to_string(), "age".to_string()];
        let name = composite_name(&fields);
        assert_eq!(composite_fields(&name), fields);
        // And it cannot be confused with a single field of the same text.
        assert_ne!(name, "countryage");
    }

    #[test]
    fn build_reconstructs_from_records() {
        let rows: Vec<(RecordId, Record)> = (0..20u64)
            .map(|i| (RecordId(i), rec((i % 4) as i64, "x")))
            .collect();
        let i = CompositeIndex::build(
            vec!["a".into(), "b".into()],
            rows.iter().map(|(id, r)| (*id, r)),
        );
        assert_eq!(i.entry_count(), 20);
        assert_eq!(i.key_count(), 4);
    }
}

// -- covering and partial indexes ----------------------------------------

/// Separator between an index's field and its covered projection in a name.
///
/// Same trick and the same reason as [`COMPOSITE_SEP`]: a name has to survive
/// a round trip through the catalog, and it must not be able to collide with
/// an ordinary index name. `\u{1}` cannot appear in an accepted field name, so
/// `country\u{1}city\u{0}population` is unambiguously "index on country,
/// covering city and population" and can never be a field somebody declared.
pub const COVER_SEP: char = '\u{1}';

/// Name for an index on `field` carrying `covers`.
pub fn covering_name(field: &str, covers: &[String]) -> String {
    if covers.is_empty() {
        return field.to_string();
    }
    format!(
        "{field}{COVER_SEP}{}",
        covers.join(&COMPOSITE_SEP.to_string())
    )
}

/// Split a covering index's name back into its field and its projection.
pub fn covering_parts(name: &str) -> (String, Vec<String>) {
    match name.split_once(COVER_SEP) {
        None => (name.to_string(), Vec::new()),
        Some((field, rest)) => (
            field.to_string(),
            rest.split(COMPOSITE_SEP).map(str::to_string).collect(),
        ),
    }
}

/// An index that also stores a projection of each record it indexes.
///
/// # What it buys, and what it costs
///
/// An ordinary index answers "which ids" and the executor then fetches each
/// one. This stores the fields the query needs next to the id, so a query
/// whose output is contained in the projection never touches the heap.
///
/// The cost is symmetric and worth stating plainly: the projection is a second
/// copy of that data, so the index is larger, and *every write to a covered
/// field must update it* — not just writes to the indexed field. That is
/// strictly more maintenance than an ordinary index, which is exactly why
/// this is worth building only when the read side actually uses it.
///
/// # Rebuildability
///
/// Held to the same invariant as everything else derived: the projection comes
/// from the records and can be rebuilt from them, so losing this index costs a
/// rebuild and never a record.
pub struct CoveringIndex {
    inner: Box<dyn Index>,
    covers: Vec<String>,
    /// Projection per indexed id. Sorted, so `snapshot`-driven rebuilds and
    /// iteration are deterministic.
    rows: BTreeMap<RecordId, Record>,
    name: String,
    bytes: usize,
}

impl CoveringIndex {
    pub fn new(field: impl Into<String>, covers: Vec<String>, kind: IndexKind) -> Self {
        let field = field.into();
        let inner: Box<dyn Index> = match kind {
            IndexKind::BTree => Box::new(BTreeIndex::new(field.clone())),
            IndexKind::Bitmap => Box::new(BitmapIndex::new(field.clone())),
            IndexKind::Hash => Box::new(HashIndex::new(field.clone())),
        };
        Self {
            name: covering_name(&field, &covers),
            inner,
            covers,
            rows: BTreeMap::new(),
            bytes: 0,
        }
    }

    pub fn build<'a>(
        field: impl Into<String>,
        covers: Vec<String>,
        kind: IndexKind,
        records: impl Iterator<Item = (RecordId, &'a Record)>,
    ) -> Self {
        let mut idx = Self::new(field, covers, kind);
        for (id, rec) in records {
            idx.index_record(id, rec);
        }
        idx
    }

    fn projection_of(&self, rec: &Record) -> Record {
        let names: Vec<&str> = self.covers.iter().map(String::as_str).collect();
        rec.project(&names)
    }
}

impl Index for CoveringIndex {
    fn kind(&self) -> IndexKind {
        self.inner.kind()
    }
    /// The *name*, not the bare field: this is what the catalog stores and what
    /// `Database` keys its index map by, and two indexes on the same field with
    /// different projections are different indexes.
    fn field(&self) -> &str {
        &self.name
    }
    fn insert(&mut self, key: Value, id: RecordId) {
        self.inner.insert(key, id)
    }
    fn remove(&mut self, key: &Value, id: RecordId) {
        self.inner.remove(key, id)
    }
    fn lookup(&self, key: &Value) -> Vec<RecordId> {
        self.inner.lookup(key)
    }
    fn range(&self, lo: Bound<&Value>, hi: Bound<&Value>) -> Option<Vec<RecordId>> {
        self.inner.range(lo, hi)
    }
    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.inner.snapshot()
    }
    fn key_count(&self) -> usize {
        self.inner.key_count()
    }
    fn entry_count(&self) -> usize {
        self.inner.entry_count()
    }
    fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.inner.memory_bytes() + self.bytes
    }
    fn covers(&self) -> &[String] {
        &self.covers
    }
    fn covered(&self, id: RecordId) -> Option<&Record> {
        self.rows.get(&id)
    }

    /// Indexes on the *underlying* field, and stores the projection beside it.
    ///
    /// Deliberately written out rather than delegating to `inner`: the two
    /// halves must agree about which records are present, because a lookup
    /// returns ids from `inner` and the covering path then reads `rows` for
    /// each. An id in one and not the other would serve a row with no fields.
    fn index_record(&mut self, id: RecordId, rec: &Record) {
        let field = {
            let f = self.inner.field();
            f.to_string()
        };
        match rec.get(&field) {
            Some(v) if !v.is_null() => {
                self.inner.insert(v.clone(), id);
                let projection = self.projection_of(rec);
                self.bytes += projection.approx_size() + ID_BYTES + NODE_OVERHEAD;
                if let Some(old) = self.rows.insert(id, projection) {
                    self.bytes = self
                        .bytes
                        .saturating_sub(old.approx_size() + ID_BYTES + NODE_OVERHEAD);
                }
            }
            _ => {
                // Not indexed, so not covered either — the two stay in step.
                if let Some(old) = self.rows.remove(&id) {
                    self.bytes = self
                        .bytes
                        .saturating_sub(old.approx_size() + ID_BYTES + NODE_OVERHEAD);
                }
            }
        }
    }

    fn unindex_record(&mut self, id: RecordId, rec: &Record) {
        let field = self.inner.field().to_string();
        if let Some(v) = rec.get(&field) {
            if !v.is_null() {
                self.inner.remove(v, id);
            }
        }
        if let Some(old) = self.rows.remove(&id) {
            self.bytes = self
                .bytes
                .saturating_sub(old.approx_size() + ID_BYTES + NODE_OVERHEAD);
        }
    }
}

/// Separator between an index's field and its condition in a name.
pub const PARTIAL_SEP: char = '\u{2}';

/// Name for an index on `field` restricted to `condition`.
pub fn partial_name(field: &str, condition: &str) -> String {
    format!("{field}{PARTIAL_SEP}{condition}")
}

/// Split a partial index's name back into its field and its condition.
pub fn partial_parts(name: &str) -> (String, Option<String>) {
    match name.split_once(PARTIAL_SEP) {
        None => (name.to_string(), None),
        Some((field, cond)) => (field.to_string(), Some(cond.to_string())),
    }
}

/// An index holding only the records that satisfy a condition.
///
/// # Why this is worth having
///
/// Most indexes are mostly dead weight. An index on `orders.status` where
/// 99% of rows are `shipped` and every query asks for `pending` still stores
/// and maintains all of them. A partial index stores the 1%, which makes it
/// smaller, faster to probe, and — the part that matters most on this engine —
/// far cheaper to *maintain*, since a write to a record that fails the
/// condition touches nothing.
///
/// # Why using one is the hard part
///
/// A partial index is only a legal access path for a query whose own predicate
/// guarantees every row it wants is present. Deciding that in general is
/// predicate implication, which is undecidable in the limit and expensive well
/// before that. This engine does not attempt it.
///
/// Instead the condition is stored as its canonical *text*, and a query may
/// use the index only when its predicate contains a syntactically identical
/// conjunct. That is far weaker than real implication — `age > 20` will not
/// match an index conditioned on `age > 18`, though it plainly should — and it
/// is deliberately so. The failure mode of being too weak is a slower plan.
/// The failure mode of being too clever is wrong answers.
pub struct PartialIndex {
    inner: Box<dyn Index>,
    /// The canonical text of the condition, and the predicate it came from.
    condition: String,
    predicate: adabt_ir::Expr,
    name: String,
}

impl PartialIndex {
    pub fn new(
        field: impl Into<String>,
        predicate: adabt_ir::Expr,
        encoded_condition: impl Into<String>,
        kind: IndexKind,
    ) -> Self {
        let field = field.into();
        let condition = encoded_condition.into();
        let inner: Box<dyn Index> = match kind {
            IndexKind::BTree => Box::new(BTreeIndex::new(field.clone())),
            IndexKind::Bitmap => Box::new(BitmapIndex::new(field.clone())),
            IndexKind::Hash => Box::new(HashIndex::new(field.clone())),
        };
        Self {
            name: partial_name(&field, &condition),
            inner,
            condition,
            predicate,
        }
    }

    pub fn build<'a>(
        field: impl Into<String>,
        predicate: adabt_ir::Expr,
        encoded_condition: impl Into<String>,
        kind: IndexKind,
        records: impl Iterator<Item = (RecordId, &'a Record)>,
    ) -> Self {
        let mut idx = Self::new(field, predicate, encoded_condition, kind);
        for (id, rec) in records {
            idx.index_record(id, rec);
        }
        idx
    }

    /// Whether a record belongs in this index.
    ///
    /// `Unknown` is treated as "no", the same way a `WHERE` clause treats it.
    /// A record whose condition cannot be evaluated is not one the condition
    /// is known to hold for, and admitting it would put rows in the index that
    /// a query relying on the condition would not expect.
    fn admits(&self, rec: &Record) -> bool {
        self.predicate.evaluate(rec) == adabt_ir::Truth::True
    }
}

impl Index for PartialIndex {
    fn kind(&self) -> IndexKind {
        self.inner.kind()
    }
    fn field(&self) -> &str {
        &self.name
    }
    fn insert(&mut self, key: Value, id: RecordId) {
        self.inner.insert(key, id)
    }
    fn remove(&mut self, key: &Value, id: RecordId) {
        self.inner.remove(key, id)
    }
    fn lookup(&self, key: &Value) -> Vec<RecordId> {
        self.inner.lookup(key)
    }
    fn range(&self, lo: Bound<&Value>, hi: Bound<&Value>) -> Option<Vec<RecordId>> {
        self.inner.range(lo, hi)
    }
    fn snapshot(&self) -> Vec<(Value, Vec<RecordId>)> {
        self.inner.snapshot()
    }
    fn key_count(&self) -> usize {
        self.inner.key_count()
    }
    fn entry_count(&self) -> usize {
        self.inner.entry_count()
    }
    fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.inner.memory_bytes() + self.condition.len()
    }
    fn condition(&self) -> Option<&str> {
        Some(&self.condition)
    }

    fn index_record(&mut self, id: RecordId, rec: &Record) {
        let field = self.inner.field().to_string();
        if !self.admits(rec) {
            // A record that no longer qualifies must leave, not merely fail to
            // be added: `index_record` is what an update calls, and the old
            // version may well have qualified.
            if let Some(v) = rec.get(&field) {
                self.inner.remove(v, id);
            }
            return;
        }
        if let Some(v) = rec.get(&field) {
            if !v.is_null() {
                self.inner.insert(v.clone(), id);
            }
        }
    }

    fn unindex_record(&mut self, id: RecordId, rec: &Record) {
        let field = self.inner.field().to_string();
        // Unconditionally, without consulting the condition. Whether the record
        // qualifies is irrelevant when it is being removed, and an index that
        // only removed qualifying records would leak entries every time a
        // record stopped qualifying before it was deleted.
        if let Some(v) = rec.get(&field) {
            if !v.is_null() {
                self.inner.remove(v, id);
            }
        }
    }
}
