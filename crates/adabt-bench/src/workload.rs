//! Workload definitions.
//!
//! These are the shapes the optimization axes are meant to trade off against
//! each other. `PointLookup` is the canonical case the whole project is aimed
//! at — the dense-id, fixed-schema `GET(id)` that Level 10 reduces to an
//! address calculation. `WorkloadShift` exists because the adaptive optimizer
//! must eventually optimize for the workload that exists, not the one that
//! existed at startup.

use adabt_core::ids::RecordId;
use adabt_core::record::Record;
use adabt_core::schema::{FieldDef, FieldType, Schema, SchemaMode};
use adabt_testkit::ops::Op;
use adabt_testkit::rng::Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    /// Almost every request is a get by id. The specialisation target.
    PointLookup,
    ReadWrite8020,
    WriteHeavy,
    RangeScan,
    /// Skewed access: a small hot set inside a large collection.
    ZipfSkew,
    /// Read-mostly for the first half, write-mostly for the second.
    WorkloadShift,
}

impl WorkloadKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "point_lookup" => Self::PointLookup,
            "read_write_80_20" => Self::ReadWrite8020,
            "write_heavy" => Self::WriteHeavy,
            "range_scan" => Self::RangeScan,
            "zipf_skew" => Self::ZipfSkew,
            "workload_shift" => Self::WorkloadShift,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PointLookup => "point_lookup",
            Self::ReadWrite8020 => "read_write_80_20",
            Self::WriteHeavy => "write_heavy",
            Self::RangeScan => "range_scan",
            Self::ZipfSkew => "zipf_skew",
            Self::WorkloadShift => "workload_shift",
        }
    }

    pub const ALL: [WorkloadKind; 6] = [
        Self::PointLookup,
        Self::ReadWrite8020,
        Self::WriteHeavy,
        Self::RangeScan,
        Self::ZipfSkew,
        Self::WorkloadShift,
    ];
}

pub const COLLECTION: &str = "customers";

/// The schema every workload uses: dense `u64` id, fixed width throughout, so
/// that `DirectLookup` is legal against it once that optimization exists.
pub fn workload_schema() -> Schema {
    Schema::new(
        SchemaMode::Fixed,
        vec![
            FieldDef::new("id", FieldType::U64).required(),
            FieldDef::new("balance", FieldType::I64).required(),
            FieldDef::new("name", FieldType::Char(32)).required(),
        ],
    )
    .expect("workload schema is fixed-width by construction")
}

pub fn make_record(id: u64) -> Record {
    Record::new()
        .with("id", id)
        .with("balance", (id as i64).wrapping_mul(7) % 100_000)
        .with("name", format!("customer-{id}"))
}

pub struct Workload {
    kind: WorkloadKind,
    rng: Rng,
    /// Number of records preloaded; ids are dense in `[0, size)`.
    size: u64,
    issued: u64,
    total: u64,
}

impl Workload {
    pub fn new(kind: WorkloadKind, size: u64, total_ops: u64, seed: u64) -> Self {
        assert!(size > 0, "workload needs a non-empty dataset");
        Self {
            kind,
            rng: Rng::new(seed),
            size,
            issued: 0,
            total: total_ops,
        }
    }

    /// The records to load before measurement starts.
    pub fn preload(&self) -> impl Iterator<Item = (RecordId, Record)> + '_ {
        (0..self.size).map(|i| (RecordId(i), make_record(i)))
    }

    /// Zipf-ish skew without a full generator: 90% of picks land in the hottest
    /// 10% of the key space. Cheap, deterministic, and enough to make caching
    /// and hot/cold separation measurably pay off.
    fn skewed_id(&mut self) -> u64 {
        let hot = (self.size / 10).max(1);
        if self.rng.chance(0.9) {
            self.rng.below(hot)
        } else {
            self.rng.below(self.size)
        }
    }

    fn uniform_id(&mut self) -> u64 {
        self.rng.below(self.size)
    }

    pub fn next_op(&mut self) -> Op {
        let progress = if self.total == 0 {
            0.0
        } else {
            self.issued as f64 / self.total as f64
        };
        self.issued += 1;
        let c = COLLECTION.to_string();

        let write_p = match self.kind {
            WorkloadKind::PointLookup => 0.002,
            WorkloadKind::ReadWrite8020 => 0.20,
            WorkloadKind::WriteHeavy => 0.90,
            WorkloadKind::RangeScan => 0.05,
            WorkloadKind::ZipfSkew => 0.10,
            // The point of this workload: the mix inverts halfway through.
            WorkloadKind::WorkloadShift => {
                if progress < 0.5 {
                    0.01
                } else {
                    0.90
                }
            }
        };

        if self.kind == WorkloadKind::RangeScan && self.rng.chance(0.10) {
            return Op::Scan { collection: c };
        }

        let id = if self.kind == WorkloadKind::ZipfSkew {
            self.skewed_id()
        } else {
            self.uniform_id()
        };

        if self.rng.chance(write_p) {
            Op::Update {
                collection: c,
                id: RecordId(id),
                rec: make_record(id),
            }
        } else {
            Op::Get {
                collection: c,
                id: RecordId(id),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mix(kind: WorkloadKind, n: u64) -> HashMap<&'static str, u64> {
        let mut w = Workload::new(kind, 1000, n, 7);
        let mut m = HashMap::new();
        for _ in 0..n {
            *m.entry(w.next_op().name()).or_default() += 1;
        }
        m
    }

    #[test]
    fn workload_schema_is_directly_addressable() {
        let s = workload_schema();
        assert_eq!(s.fixed_record_size(), Some(8 + 8 + 32));
    }

    #[test]
    fn generated_records_satisfy_the_schema() {
        let s = workload_schema();
        for id in [0u64, 1, 999, 123_456] {
            s.validate_record(&make_record(id))
                .unwrap_or_else(|e| panic!("id {id}: {e}"));
        }
    }

    #[test]
    fn point_lookup_is_overwhelmingly_reads() {
        let m = mix(WorkloadKind::PointLookup, 20_000);
        let gets = *m.get("get").unwrap_or(&0);
        assert!(gets as f64 / 20_000.0 > 0.99, "{m:?}");
    }

    #[test]
    fn write_heavy_is_overwhelmingly_writes() {
        let m = mix(WorkloadKind::WriteHeavy, 20_000);
        assert!(
            *m.get("update").unwrap_or(&0) as f64 / 20_000.0 > 0.85,
            "{m:?}"
        );
    }

    #[test]
    fn range_scan_actually_scans() {
        assert!(
            *mix(WorkloadKind::RangeScan, 10_000)
                .get("scan")
                .unwrap_or(&0)
                > 0
        );
    }

    #[test]
    fn workload_shift_inverts_its_mix_halfway_through() {
        let n = 20_000u64;
        let mut w = Workload::new(WorkloadKind::WorkloadShift, 1000, n, 3);
        let ops: Vec<_> = (0..n).map(|_| w.next_op()).collect();
        let writes =
            |s: &[Op]| s.iter().filter(|o| o.name() == "update").count() as f64 / s.len() as f64;
        let (first, second) = ops.split_at(n as usize / 2);
        assert!(writes(first) < 0.05, "first half should be read-mostly");
        assert!(writes(second) > 0.85, "second half should be write-mostly");
    }

    #[test]
    fn zipf_skew_concentrates_on_a_hot_set() {
        let size = 1000u64;
        let mut w = Workload::new(WorkloadKind::ZipfSkew, size, 20_000, 5);
        let hot = (0..20_000)
            .filter_map(|_| match w.next_op() {
                Op::Get { id, .. } | Op::Update { id, .. } => Some(id.0),
                _ => None,
            })
            .filter(|id| *id < size / 10)
            .count();
        assert!(hot as f64 / 20_000.0 > 0.8, "hot fraction too low: {hot}");
    }

    #[test]
    fn preload_covers_the_dense_id_space() {
        let w = Workload::new(WorkloadKind::PointLookup, 100, 0, 1);
        let ids: Vec<u64> = w.preload().map(|(i, _)| i.0).collect();
        assert_eq!(ids, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn every_workload_name_round_trips() {
        for k in WorkloadKind::ALL {
            assert_eq!(WorkloadKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(WorkloadKind::parse("nope"), None);
    }
}
