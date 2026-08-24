//! Record batches.
//!
//! Operators pass batches, not single rows. The cost is a little complexity
//! now; the reason is that every later specialisation depends on it. A column
//! store hands back columns for a whole batch at once, a compiled hot path
//! amortises its dispatch over a batch, and a vectorised filter evaluates a
//! predicate across one. A scalar-at-a-time executor would have to be rewritten
//! before any of those become possible, so it is not worth building one.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;

/// Rows per batch. Large enough to amortise per-batch overhead, small enough
/// that a batch and the values it touches stay in cache.
pub const BATCH_SIZE: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordBatch {
    pub ids: Vec<RecordId>,
    pub records: Vec<Record>,
}

impl RecordBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self {
            ids: Vec::with_capacity(n),
            records: Vec::with_capacity(n),
        }
    }

    pub fn push(&mut self, id: RecordId, rec: Record) {
        self.ids.push(id);
        self.records.push(rec);
    }

    pub fn len(&self) -> usize {
        debug_assert_eq!(
            self.ids.len(),
            self.records.len(),
            "batch columns out of step"
        );
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (RecordId, &Record)> {
        self.ids.iter().copied().zip(self.records.iter())
    }

    pub fn into_rows(self) -> Vec<(RecordId, Record)> {
        self.ids.into_iter().zip(self.records).collect()
    }

    pub fn from_rows(rows: Vec<(RecordId, Record)>) -> Self {
        let mut b = RecordBatch::with_capacity(rows.len());
        for (id, r) in rows {
            b.push(id, r);
        }
        b
    }

    /// Keep only the rows whose corresponding flag is set.
    ///
    /// Taking a mask rather than a closure is what lets a filter be evaluated
    /// across a whole batch — and later, over a column instead of over records.
    pub fn retain_mask(&mut self, keep: &[bool]) {
        debug_assert_eq!(keep.len(), self.len());
        let mut i = 0;
        self.ids.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
        let mut j = 0;
        self.records.retain(|_| {
            let k = keep[j];
            j += 1;
            k
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(n: usize) -> RecordBatch {
        let mut b = RecordBatch::new();
        for i in 0..n {
            b.push(RecordId(i as u64), Record::new().with("i", i as i64));
        }
        b
    }

    #[test]
    fn push_keeps_ids_and_records_in_step() {
        let b = batch(5);
        assert_eq!(b.len(), 5);
        assert_eq!(b.ids.len(), b.records.len());
    }

    #[test]
    fn round_trips_through_rows() {
        let b = batch(10);
        let rows = b.clone().into_rows();
        assert_eq!(RecordBatch::from_rows(rows), b);
    }

    #[test]
    fn retain_mask_keeps_exactly_the_flagged_rows() {
        let mut b = batch(6);
        b.retain_mask(&[true, false, true, false, true, false]);
        assert_eq!(b.len(), 3);
        assert_eq!(
            b.ids,
            vec![RecordId(0), RecordId(2), RecordId(4)],
            "mask kept the wrong rows"
        );
        // Records must have been filtered identically, not just the ids.
        for (id, rec) in b.iter() {
            assert_eq!(
                rec.get("i"),
                Some(&adabt_core::value::Value::I64(id.0 as i64))
            );
        }
    }

    #[test]
    fn an_all_false_mask_empties_the_batch() {
        let mut b = batch(4);
        b.retain_mask(&[false; 4]);
        assert!(b.is_empty());
    }

    #[test]
    fn an_all_true_mask_changes_nothing() {
        let mut b = batch(4);
        let before = b.clone();
        b.retain_mask(&[true; 4]);
        assert_eq!(b, before);
    }
}
