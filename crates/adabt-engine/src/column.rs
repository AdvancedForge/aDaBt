//! Columnar storage: a derived representation for scans and aggregates.
//!
//! A heap stores whole records together, which is right when a query wants a
//! whole record and wrong when it wants one field of every record. An aggregate
//! over `balance` reads and decodes `id`, `country` and `notes` too, and throws
//! them away.
//!
//! Storing each field contiguously fixes that, and brings a second win with it:
//! a column of one type compresses in ways a row of mixed types cannot. Strings
//! here are **dictionary-encoded** — a low-cardinality field like `country`
//! becomes one small integer per row instead of a padded string — which is why
//! this representation can be smaller than the rows it derives from despite
//! being a second copy.
//!
//! Derived, like everything else: rebuildable from the heap, droppable without
//! loss, and never the only copy of anything.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::value::Value;
use std::collections::BinaryHeap;
use std::collections::HashMap;

/// One row's candidacy in a top-K selection: its id and whatever the key
/// column holds for it.
///
/// The ordering mirrors the executor's single-key sort exactly — value
/// comparison first with direction applied to the value alone, an absent
/// cell ordered as a missing field is after a fetch (last, ascending), and
/// the record id ascending as a tiebreak that direction never touches. That
/// last part matters: reversing the whole composite would also reverse the
/// tiebreak, and ties under `Sort` come out in ascending id order regardless
/// of direction.
struct Candidate {
    id: RecordId,
    value: Option<Value>,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.value == other.value
    }
}
impl Eq for Candidate {}

impl Candidate {
    fn order_vs(&self, other: &Self, descending: bool) -> std::cmp::Ordering {
        let ord = match (&self.value, &other.value) {
            (Some(x), Some(y)) => x.cmp(y),
            (None, None) => std::cmp::Ordering::Equal,
            // A missing field sorts after a present one; `compare_rows` in
            // adabt-exec is the definition this mirrors.
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
        };
        let ord = if descending { ord.reverse() } else { ord };
        ord.then(self.id.cmp(&other.id))
    }
}

/// A candidacy paired with the query's direction, ordered by the query's own
/// total order.
///
/// The selection heap needs `peek()` to be the worst row it holds. Ordered
/// by the full query order — direction included, id tiebreak last — a
/// max-heap's peek is the maximum under that order, which is exactly the
/// row to evict whether the query wants the smallest or the largest: in both
/// cases the kept set is the k minima of the order, and its worst member is
/// the largest of those minima. One rule serves both directions; reversing
/// the order instead would put the best row on top for descending sorts and
/// let early losers hide beneath it.
struct HeapCand(Candidate, bool);

impl PartialEq for HeapCand {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for HeapCand {}
impl PartialOrd for HeapCand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapCand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.order_vs(&other.0, self.1)
    }
}

/// One field's values, in row order.
#[derive(Debug, Clone)]
enum Column {
    Bool(Vec<Option<bool>>),
    I64(Vec<Option<i64>>),
    U64(Vec<Option<u64>>),
    F64(Vec<Option<f64>>),
    /// Dictionary-encoded text: each row holds an index into `dict`.
    Dict {
        dict: Vec<String>,
        codes: Vec<Option<u32>>,
    },
    /// Anything that does not fit a typed column.
    Other(Vec<Option<Value>>),
}

impl Column {
    fn len(&self) -> usize {
        match self {
            Column::Bool(v) => v.len(),
            Column::I64(v) => v.len(),
            Column::U64(v) => v.len(),
            Column::F64(v) => v.len(),
            Column::Dict { codes, .. } => codes.len(),
            Column::Other(v) => v.len(),
        }
    }

    fn get(&self, row: usize) -> Option<Value> {
        match self {
            Column::Bool(v) => v.get(row).copied().flatten().map(Value::Bool),
            Column::I64(v) => v.get(row).copied().flatten().map(Value::I64),
            Column::U64(v) => v.get(row).copied().flatten().map(Value::U64),
            Column::F64(v) => v.get(row).copied().flatten().map(Value::F64),
            Column::Dict { dict, codes } => codes
                .get(row)
                .copied()
                .flatten()
                .and_then(|c| dict.get(c as usize))
                .map(|s| Value::Str(s.clone())),
            Column::Other(v) => v.get(row).cloned().flatten(),
        }
    }

    fn push(&mut self, v: Option<&Value>) {
        match (self, v) {
            (Column::Bool(c), Some(Value::Bool(b))) => c.push(Some(*b)),
            (Column::I64(c), Some(Value::I64(n))) => c.push(Some(*n)),
            (Column::I64(c), Some(Value::U64(n))) => c.push(Some(*n as i64)),
            (Column::U64(c), Some(Value::U64(n))) => c.push(Some(*n)),
            (Column::F64(c), Some(Value::F64(f))) => c.push(Some(*f)),
            (Column::Dict { dict, codes }, Some(Value::Str(s))) => {
                let code = match dict.iter().position(|d| d == s) {
                    Some(i) => i as u32,
                    None => {
                        dict.push(s.clone());
                        (dict.len() - 1) as u32
                    }
                };
                codes.push(Some(code));
            }
            (Column::Other(c), v) => c.push(v.cloned()),
            // A value that does not match its column's type: record the absence
            // rather than guessing. The heap remains authoritative, so a query
            // needing exactness can always fall back to it.
            (col, _) => col.push_null(),
        }
    }

    fn push_null(&mut self) {
        match self {
            Column::Bool(v) => v.push(None),
            Column::I64(v) => v.push(None),
            Column::U64(v) => v.push(None),
            Column::F64(v) => v.push(None),
            Column::Dict { codes, .. } => codes.push(None),
            Column::Other(v) => v.push(None),
        }
    }

    fn for_value(v: &Value) -> Column {
        match v {
            Value::Bool(_) => Column::Bool(Vec::new()),
            Value::I64(_) => Column::I64(Vec::new()),
            Value::U64(_) => Column::U64(Vec::new()),
            Value::F64(_) => Column::F64(Vec::new()),
            Value::Str(_) => Column::Dict {
                dict: Vec::new(),
                codes: Vec::new(),
            },
            _ => Column::Other(Vec::new()),
        }
    }

    fn memory_bytes(&self) -> usize {
        match self {
            Column::Bool(v) => v.len() * 2,
            Column::I64(v) => v.len() * 9,
            Column::U64(v) => v.len() * 9,
            Column::F64(v) => v.len() * 9,
            // The dictionary win: one code per row plus the distinct strings.
            Column::Dict { dict, codes } => {
                codes.len() * 5 + dict.iter().map(|s| s.len() + 24).sum::<usize>()
            }
            Column::Other(v) => v.len() * 48,
        }
    }

    /// Distinct values held, where cheaply known.
    fn cardinality(&self) -> Option<usize> {
        match self {
            Column::Dict { dict, .. } => Some(dict.len()),
            _ => None,
        }
    }
}

/// A columnar copy of one collection.
pub struct ColumnStore {
    /// Row order, matching every column's index.
    ids: Vec<RecordId>,
    /// Where each id sits, for point access.
    row_of: HashMap<RecordId, usize>,
    columns: HashMap<String, Column>,
    /// One interned handle per field name, kept beside the column it names.
    ///
    /// Every record `project` builds carries the same field-name strings as
    /// every other. Handing out fresh `Arc`s cloned from here is a refcount
    /// bump; the alternative — `set("...")` with a literal — allocates a
    /// `String` per field per row and then another allocation to turn it
    /// into an `Arc`. On a scan that is the difference between one
    /// allocation per row and one per cell.
    arcs: HashMap<String, std::sync::Arc<str>>,
    /// Rows whose id has been deleted. Skipped on read; reclaimed by a rebuild.
    dead: Vec<bool>,
    dead_count: usize,
}

impl ColumnStore {
    /// Build from the authoritative representation.
    pub fn build<'a>(rows: impl Iterator<Item = (RecordId, &'a Record)>) -> Self {
        let mut store = ColumnStore {
            ids: Vec::new(),
            row_of: HashMap::new(),
            columns: HashMap::new(),
            arcs: HashMap::new(),
            dead: Vec::new(),
            dead_count: 0,
        };
        for (id, rec) in rows {
            store.append(id, rec);
        }
        store
    }

    /// Append a row. Public because maintenance appends rather than updating:
    /// columns are contiguous, so changing a row in place would mean shifting
    /// every column.
    pub fn append_row(&mut self, id: RecordId, rec: &Record) {
        self.append(id, rec)
    }

    fn append(&mut self, id: RecordId, rec: &Record) {
        let row = self.ids.len();
        self.ids.push(id);
        self.row_of.insert(id, row);
        self.dead.push(false);

        for (name, value) in rec.iter() {
            let col = self
                .columns
                .entry(name.to_string())
                .or_insert_with(|| Column::for_value(value));
            // A column that appeared late must be back-filled so every column
            // stays the same length as the row list.
            while col.len() < row {
                col.push_null();
            }
            col.push(Some(value));
            // And the name gets exactly one heap allocation for the life of
            // the store, however many rows reference it afterwards.
            self.arcs
                .entry(name.to_string())
                .or_insert_with(|| std::sync::Arc::from(name));
        }
        // Fields absent from this record still need a slot.
        for col in self.columns.values_mut() {
            while col.len() <= row {
                col.push_null();
            }
        }
    }

    pub fn row_count(&self) -> usize {
        self.ids.len() - self.dead_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count() == 0
    }

    pub fn fields(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.columns.keys().map(|s| s.as_str()).collect();
        v.sort();
        v
    }

    /// The ids of every live row, ascending.
    ///
    /// `pub(crate)` because the consistency checker is the only reader: it
    /// compares this set against the heap's to catch a derived copy that has
    /// drifted from its primary.
    pub(crate) fn live_ids(&self) -> Vec<RecordId> {
        (0..self.ids.len())
            .filter(|row| !self.dead[*row])
            .map(|row| self.ids[row])
            .collect()
    }

    pub fn memory_bytes(&self) -> usize {
        self.columns
            .values()
            .map(|c| c.memory_bytes())
            .sum::<usize>()
            + self.ids.len() * 8
            + self.row_of.len() * 24
            + self.dead.len()
    }

    /// Distinct values in a field, where the encoding knows.
    pub fn cardinality(&self, field: &str) -> Option<usize> {
        self.columns.get(field)?.cardinality()
    }

    /// Fraction of rows that are tombstones. High means a rebuild would help.
    pub fn dead_fraction(&self) -> f64 {
        if self.ids.is_empty() {
            0.0
        } else {
            self.dead_count as f64 / self.ids.len() as f64
        }
    }

    pub fn mark_dead(&mut self, id: RecordId) {
        if let Some(&row) = self.row_of.get(&id) {
            if !self.dead[row] {
                self.dead[row] = true;
                self.dead_count += 1;
            }
            self.row_of.remove(&id);
        }
    }

    /// Read only the named fields, in record-id order.
    ///
    /// The whole point: a query wanting two of twenty fields touches two
    /// columns instead of decoding twenty.
    pub fn project(&self, fields: &[&str]) -> Vec<(RecordId, Record)> {
        let mut rows: Vec<(RecordId, usize)> = self
            .ids
            .iter()
            .enumerate()
            .filter(|(row, _)| !self.dead[*row])
            .map(|(row, id)| (*id, row))
            .collect();
        // Row order is insertion order; the logical contract is id order.
        rows.sort_unstable_by_key(|(id, _)| *id);

        rows.into_iter()
            .map(|(id, row)| {
                let mut rec = Record::new();
                for f in fields {
                    if let Some(col) = self.columns.get(*f) {
                        if let Some(v) = col.get(row) {
                            // Interned name, refcount bump — see `arcs`.
                            match self.arcs.get(*f) {
                                Some(arc) => {
                                    rec.set_shared(std::sync::Arc::clone(arc), v);
                                }
                                None => {
                                    rec.set((*f).to_string(), v);
                                }
                            }
                        }
                    }
                }
                (id, rec)
            })
            .collect()
    }

    /// Every value of one field, skipping absent ones. Feeds aggregation
    /// without materialising a single record.
    pub fn column_values(&self, field: &str) -> Option<Vec<Value>> {
        let col = self.columns.get(field)?;
        Some(
            (0..self.ids.len())
                .filter(|row| !self.dead[*row])
                .filter_map(|row| col.get(row))
                .collect(),
        )
    }

    /// The k record ids whose `field` values are smallest under the executor's
    /// single-key total order — value first, absent last, direction applied to
    /// the value comparison alone, id ascending as the tiebreak — returned in
    /// that order.
    ///
    /// `None` when the store does not hold `field`. Selection happens over
    /// raw column cells: no `Record` is built per row, which is the entire
    /// point — `project` pays a record allocation per row to hand back one
    /// field, and at 100k rows that allocation is the cost of the query.
    ///
    /// Absence follows the heap's meaning, not the cell's: an absent cell
    /// sorts as a missing field does after a fetch, so winners chosen here
    /// are the winners a full sort would choose.
    pub fn topk_ids(&self, field: &str, descending: bool, k: usize) -> Option<Vec<RecordId>> {
        let col = self.columns.get(field)?;
        if k == 0 {
            return Some(Vec::new());
        }
        let mut heap: BinaryHeap<HeapCand> = BinaryHeap::with_capacity(k + 1);
        for row in 0..self.ids.len() {
            if self.dead[row] {
                continue;
            }
            let cand = Candidate {
                id: self.ids[row],
                value: col.get(row),
            };
            if heap.len() < k {
                heap.push(HeapCand(cand, descending));
                continue;
            }
            // `peek` is the worst row held (see HeapCand); a better candidate
            // takes its place.
            let worst = heap.peek().expect("heap at capacity is non-empty");
            if cand.order_vs(&worst.0, descending) == std::cmp::Ordering::Less {
                heap.pop();
                heap.push(HeapCand(cand, descending));
            }
        }
        let mut out: Vec<Candidate> = heap.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| a.order_vs(b, descending));
        Some(out.into_iter().map(|c| c.id).collect())
    }

    pub fn get(&self, id: RecordId, fields: &[&str]) -> Option<Record> {
        let row = *self.row_of.get(&id)?;
        if self.dead[row] {
            return None;
        }
        let mut rec = Record::new();
        for f in fields {
            if let Some(col) = self.columns.get(*f) {
                if let Some(v) = col.get(row) {
                    rec.set((*f).to_string(), v);
                }
            }
        }
        Some(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: u64) -> Vec<(RecordId, Record)> {
        const COUNTRIES: [&str; 4] = ["NO", "SE", "DK", "FI"];
        (0..n)
            .map(|i| {
                (
                    RecordId(i),
                    Record::new()
                        .with("id", i)
                        .with("country", COUNTRIES[(i % 4) as usize])
                        .with("balance", (i * 7 % 1000) as i64)
                        .with("active", i % 2 == 0),
                )
            })
            .collect()
    }

    fn store(n: u64) -> ColumnStore {
        let r = rows(n);
        ColumnStore::build(r.iter().map(|(i, rec)| (*i, rec)))
    }

    #[test]
    fn every_value_round_trips() {
        let src = rows(200);
        let cs = store(200);
        let got = cs.project(&["id", "country", "balance", "active"]);
        assert_eq!(got.len(), src.len());
        for ((gid, grec), (sid, srec)) in got.iter().zip(src.iter()) {
            assert_eq!(gid, sid);
            assert_eq!(grec, srec, "row {gid} differs");
        }
    }

    #[test]
    fn projection_returns_only_the_named_fields() {
        let cs = store(50);
        for (_, rec) in cs.project(&["id", "balance"]) {
            assert_eq!(rec.len(), 2);
            assert!(rec.get("country").is_none());
        }
    }

    #[test]
    fn rows_come_back_in_record_id_order() {
        // Insertion order is not id order in general, and the logical contract
        // is id order.
        let mut src: Vec<(RecordId, Record)> = rows(50);
        src.reverse();
        let cs = ColumnStore::build(src.iter().map(|(i, r)| (*i, r)));
        let ids: Vec<u64> = cs.project(&["id"]).iter().map(|(i, _)| i.0).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn dictionary_encoding_collapses_a_low_cardinality_column() {
        let cs = store(10_000);
        assert_eq!(cs.cardinality("country"), Some(4));
        // 10,000 rows of 2-character country codes must not cost 10,000 strings.
        assert!(
            cs.memory_bytes() < 10_000 * 100,
            "column store is larger than expected: {}",
            cs.memory_bytes()
        );
    }

    #[test]
    fn a_high_cardinality_text_column_still_works() {
        let src: Vec<(RecordId, Record)> = (0..500u64)
            .map(|i| {
                (
                    RecordId(i),
                    Record::new().with("name", format!("unique-{i}")),
                )
            })
            .collect();
        let cs = ColumnStore::build(src.iter().map(|(i, r)| (*i, r)));
        assert_eq!(cs.cardinality("name"), Some(500));
        let got = cs.project(&["name"]);
        assert_eq!(got.len(), 500);
        assert_eq!(got[7].1.get("name"), Some(&Value::Str("unique-7".into())));
    }

    #[test]
    fn column_values_skips_absent_entries() {
        let src: Vec<(RecordId, Record)> = (0..10u64)
            .map(|i| {
                let mut r = Record::new().with("id", i);
                if i % 2 == 0 {
                    r.set("opt", i as i64);
                }
                (RecordId(i), r)
            })
            .collect();
        let cs = ColumnStore::build(src.iter().map(|(i, r)| (*i, r)));
        assert_eq!(cs.column_values("opt").unwrap().len(), 5);
        assert_eq!(cs.column_values("id").unwrap().len(), 10);
        assert!(cs.column_values("nope").is_none());
    }

    #[test]
    fn a_field_that_appears_late_is_backfilled() {
        // Columns must stay the same length as the row list, or every later
        // row would read a shifted value.
        let src: Vec<(RecordId, Record)> = (0..10u64)
            .map(|i| {
                let mut r = Record::new().with("id", i);
                if i >= 5 {
                    r.set("late", i as i64);
                }
                (RecordId(i), r)
            })
            .collect();
        let cs = ColumnStore::build(src.iter().map(|(i, r)| (*i, r)));
        let got = cs.project(&["id", "late"]);
        for (id, rec) in got {
            if id.0 >= 5 {
                assert_eq!(rec.get("late"), Some(&Value::I64(id.0 as i64)), "row {id}");
            } else {
                assert!(
                    rec.get("late").is_none(),
                    "row {id} got a value it never had"
                );
            }
        }
    }

    #[test]
    fn deleted_rows_disappear_from_reads() {
        let mut cs = store(100);
        for i in (0..100u64).step_by(3) {
            cs.mark_dead(RecordId(i));
        }
        let got = cs.project(&["id"]);
        assert_eq!(got.len(), cs.row_count());
        assert!(got.iter().all(|(id, _)| id.0 % 3 != 0));
        assert!(cs.get(RecordId(0), &["id"]).is_none());
        assert!(cs.get(RecordId(1), &["id"]).is_some());
    }

    #[test]
    fn marking_the_same_row_dead_twice_does_not_double_count() {
        let mut cs = store(10);
        cs.mark_dead(RecordId(1));
        cs.mark_dead(RecordId(1));
        assert_eq!(cs.row_count(), 9);
    }

    #[test]
    fn dead_fraction_reports_when_a_rebuild_would_help() {
        let mut cs = store(100);
        assert_eq!(cs.dead_fraction(), 0.0);
        for i in 0..50u64 {
            cs.mark_dead(RecordId(i));
        }
        assert!((cs.dead_fraction() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn point_access_works_and_respects_projection() {
        let cs = store(100);
        let rec = cs.get(RecordId(42), &["country", "balance"]).unwrap();
        assert_eq!(rec.len(), 2);
        assert_eq!(rec.get("country"), Some(&Value::Str("DK".into())));
        assert!(cs.get(RecordId(9_999), &["id"]).is_none());
    }

    #[test]
    fn an_empty_store_behaves() {
        let cs = ColumnStore::build(std::iter::empty());
        assert!(cs.is_empty());
        assert_eq!(cs.row_count(), 0);
        assert!(cs.project(&["anything"]).is_empty());
        assert_eq!(cs.dead_fraction(), 0.0);
        assert!(cs.fields().is_empty());
    }

    #[test]
    fn a_columnar_copy_is_smaller_than_the_rows_it_derives_from() {
        // Not automatic — it holds because the text column is low-cardinality.
        // A second copy that costs more than the original would be a bad trade
        // however fast it made scans.
        let cs = store(20_000);
        // Rows carry a padded fixed layout: header + bitmap + 8 + 8 + 8 + 1.
        let approx_row_bytes = 20_000 * 60;
        assert!(
            cs.memory_bytes() < approx_row_bytes,
            "columnar copy ({}) is not smaller than the rows ({approx_row_bytes})",
            cs.memory_bytes()
        );
    }
}

/// Aggregation computed directly from columns.
///
/// The reason a column store is worth building. `project` reconstructs a
/// `Record` per row, which hands the executor rows again and throws away most
/// of the advantage: measured against a heap scan it was only ~23% faster,
/// because building N records dominated reading two columns.
///
/// Reading the grouping key and the aggregated value straight out of their
/// columns allocates once per *group* instead of once per *row*, which is the
/// difference between a columnar layout and a columnar layout used rowwise.
impl ColumnStore {
    /// Group rows by `group_by` and accumulate `aggs`, without materialising a
    /// record per row.
    ///
    /// `predicate` is evaluated against a scratch record holding only the
    /// fields it references, reused across rows.
    pub fn aggregate(
        &self,
        group_by: &[String],
        aggs: &[ColumnAgg],
        predicate: Option<ColumnPredicate<'_>>,
    ) -> Vec<(Vec<Value>, Vec<ColumnAccum>)> {
        use std::collections::BTreeMap;
        let mut groups: BTreeMap<Vec<Value>, Vec<ColumnAccum>> = BTreeMap::new();
        let mut scratch = Record::new();

        for row in 0..self.ids.len() {
            if self.dead[row] {
                continue;
            }
            if let Some((fields, test)) = &predicate {
                // One record reused for every row, rather than one per row.
                for f in fields.iter() {
                    match self.columns.get(f).and_then(|c| c.get(row)) {
                        Some(v) => {
                            scratch.set(f.clone(), v);
                        }
                        None => {
                            scratch.remove(f);
                        }
                    }
                }
                if !test(&scratch) {
                    continue;
                }
            }

            let key: Vec<Value> = group_by
                .iter()
                .map(|g| {
                    self.columns
                        .get(g)
                        .and_then(|c| c.get(row))
                        .unwrap_or(Value::Null)
                })
                .collect();
            let accs = groups
                .entry(key)
                .or_insert_with(|| vec![ColumnAccum::default(); aggs.len()]);
            for (i, a) in aggs.iter().enumerate() {
                let v = a
                    .field
                    .as_ref()
                    .and_then(|f| self.columns.get(f).and_then(|c| c.get(row)));
                accs[i].observe(a.counts_rows, v.as_ref());
            }
        }
        groups.into_iter().collect()
    }
}

/// Fields a predicate reads, paired with the predicate itself.
///
/// Passed together because the column store must know which columns to load
/// into its scratch record before it can evaluate the test.
pub type ColumnPredicate<'a> = (&'a [String], &'a dyn Fn(&Record) -> bool);

/// What to accumulate over a column.
#[derive(Debug, Clone)]
pub struct ColumnAgg {
    pub field: Option<String>,
    /// True for `COUNT(*)`, which counts rows rather than values.
    pub counts_rows: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ColumnAccum {
    pub count: u64,
    pub sum: f64,
    pub saw_value: bool,
    pub min: Option<Value>,
    pub max: Option<Value>,
}

impl ColumnAccum {
    fn observe(&mut self, counts_rows: bool, v: Option<&Value>) {
        if counts_rows {
            self.count += 1;
            return;
        }
        let Some(v) = v else { return };
        if v.is_null() {
            return;
        }
        self.count += 1;
        self.saw_value = true;
        if let Some(n) = match v {
            Value::I64(n) => Some(*n as f64),
            Value::U64(n) => Some(*n as f64),
            Value::F64(f) => Some(*f),
            _ => None,
        } {
            self.sum += n;
        }
        if self.min.as_ref().is_none_or(|m| v < m) {
            self.min = Some(v.clone());
        }
        if self.max.as_ref().is_none_or(|m| v > m) {
            self.max = Some(v.clone());
        }
    }
}

#[cfg(test)]
mod aggregate_tests {
    use super::*;

    fn store(n: u64) -> ColumnStore {
        const C: [&str; 4] = ["NO", "SE", "DK", "FI"];
        let rows: Vec<(RecordId, Record)> = (0..n)
            .map(|i| {
                (
                    RecordId(i),
                    Record::new()
                        .with("country", C[(i % 4) as usize])
                        .with("balance", (i % 100) as i64)
                        .with("age", (i % 50) as i64),
                )
            })
            .collect();
        ColumnStore::build(rows.iter().map(|(i, r)| (*i, r)))
    }

    #[test]
    fn grouped_counts_are_correct() {
        let cs = store(1_000);
        let out = cs.aggregate(
            &["country".to_string()],
            &[ColumnAgg {
                field: None,
                counts_rows: true,
            }],
            None,
        );
        assert_eq!(out.len(), 4);
        for (_, accs) in &out {
            assert_eq!(accs[0].count, 250);
        }
    }

    #[test]
    fn sums_and_extremes_match_a_manual_pass() {
        let cs = store(500);
        let out = cs.aggregate(
            &[],
            &[ColumnAgg {
                field: Some("balance".into()),
                counts_rows: false,
            }],
            None,
        );
        let expected: i64 = (0..500i64).map(|i| i % 100).sum();
        let acc = &out[0].1[0];
        assert_eq!(acc.sum as i64, expected);
        assert_eq!(acc.min, Some(Value::I64(0)));
        assert_eq!(acc.max, Some(Value::I64(99)));
        assert_eq!(acc.count, 500);
    }

    #[test]
    fn a_predicate_filters_rows_without_a_record_per_row() {
        let cs = store(1_000);
        let fields = vec!["age".to_string()];
        let test = |r: &Record| matches!(r.get("age"), Some(Value::I64(a)) if *a >= 25);
        let out = cs.aggregate(
            &[],
            &[ColumnAgg {
                field: None,
                counts_rows: true,
            }],
            Some((&fields, &test)),
        );
        // age is i % 50, so half the rows qualify.
        assert_eq!(out[0].1[0].count, 500);
    }

    #[test]
    fn dead_rows_are_excluded() {
        let mut cs = store(100);
        for i in 0..50u64 {
            cs.mark_dead(RecordId(i));
        }
        let out = cs.aggregate(
            &[],
            &[ColumnAgg {
                field: None,
                counts_rows: true,
            }],
            None,
        );
        assert_eq!(out[0].1[0].count, 50);
    }

    #[test]
    fn an_ungrouped_aggregate_over_nothing_yields_no_group() {
        // The caller supplies the COUNT(*)-of-nothing-is-zero rule; the column
        // store reports only what it saw.
        let cs = ColumnStore::build(std::iter::empty());
        let out = cs.aggregate(
            &[],
            &[ColumnAgg {
                field: None,
                counts_rows: true,
            }],
            None,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_column_that_does_not_exist_groups_as_null() {
        let cs = store(10);
        let out = cs.aggregate(
            &["nope".to_string()],
            &[ColumnAgg {
                field: None,
                counts_rows: true,
            }],
            None,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, vec![Value::Null]);
    }
}
