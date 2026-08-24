//! Prometheus exposition format for a [`Snapshot`].
//!
//! Formatting only — every number here was already being collected for the
//! optimizer's own use (see `collector.rs`); this exists so an operator can
//! point a scraper at the same data an embedded caller could already read
//! through `Snapshot` directly.

use crate::collector::Snapshot;
use std::fmt::Write as _;

/// Escape a label value per the exposition format: backslash, double quote
/// and newline are the only characters that are not already legal inside a
/// quoted label.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Render `snapshot` as Prometheus text exposition format.
///
/// Every metric name is prefixed `adabt_`, per the format's own convention
/// that a name identify its source unambiguously to a scraper pulling from
/// many exporters at once.
pub fn to_prometheus_text(snapshot: &Snapshot) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# HELP adabt_op_calls_total Calls per operation kind.");
    let _ = writeln!(out, "# TYPE adabt_op_calls_total counter");
    for (op, stats) in &snapshot.per_op {
        let _ = writeln!(
            out,
            "adabt_op_calls_total{{op=\"{}\"}} {}",
            op.as_str(),
            stats.calls
        );
    }

    let _ = writeln!(
        out,
        "# HELP adabt_op_rows_total Rows touched per operation kind."
    );
    let _ = writeln!(out, "# TYPE adabt_op_rows_total counter");
    for (op, stats) in &snapshot.per_op {
        let _ = writeln!(
            out,
            "adabt_op_rows_total{{op=\"{}\"}} {}",
            op.as_str(),
            stats.rows
        );
    }

    let _ = writeln!(
        out,
        "# HELP adabt_op_latency_nanos Latency per operation kind, in nanoseconds."
    );
    let _ = writeln!(out, "# TYPE adabt_op_latency_nanos gauge");
    for (op, stats) in &snapshot.per_op {
        let Some(h) = &stats.latency else { continue };
        if h.count() == 0 {
            continue;
        }
        for (label, p) in [("p50", 50.0), ("p99", 99.0)] {
            let _ = writeln!(
                out,
                "adabt_op_latency_nanos{{op=\"{}\",quantile=\"{label}\"}} {}",
                op.as_str(),
                h.percentile(p)
            );
        }
    }

    let _ = writeln!(
        out,
        "# HELP adabt_cache_hit_rate Fraction of lookups served from a named cache."
    );
    let _ = writeln!(out, "# TYPE adabt_cache_hit_rate gauge");
    let mut caches: Vec<&&str> = snapshot
        .cache_hits
        .keys()
        .chain(snapshot.cache_misses.keys())
        .collect();
    caches.sort();
    caches.dedup();
    for cache in caches {
        if let Some(rate) = snapshot.hit_rate(cache) {
            let _ = writeln!(
                out,
                "adabt_cache_hit_rate{{cache=\"{}\"}} {rate}",
                escape_label(cache)
            );
        }
    }

    let _ = writeln!(
        out,
        "# HELP adabt_touches_total Record touches sampled for temperature."
    );
    let _ = writeln!(out, "# TYPE adabt_touches_total counter");
    let _ = writeln!(out, "adabt_touches_total {}", snapshot.touches);

    let _ = writeln!(
        out,
        "# HELP adabt_index_use_total Times a query filtered on an indexed field, by whether the planner used the index."
    );
    let _ = writeln!(out, "# TYPE adabt_index_use_total counter");
    for ((collection, field), n) in &snapshot.field_filters {
        let _ = writeln!(
            out,
            "adabt_index_use_total{{collection=\"{}\",field=\"{}\",used=\"false\"}} {n}",
            escape_label(collection),
            escape_label(field)
        );
    }
    for ((collection, field), n) in &snapshot.index_usage {
        let _ = writeln!(
            out,
            "adabt_index_use_total{{collection=\"{}\",field=\"{}\",used=\"true\"}} {n}",
            escape_label(collection),
            escape_label(field)
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{OpKind, QueryShape};
    use crate::{CollectingProbe, Event, Probe};

    #[test]
    fn every_recorded_call_appears_in_the_output() {
        let probe = CollectingProbe::new();
        for _ in 0..3 {
            probe.record(Event::Op {
                collection: "c",
                kind: OpKind::Get,
                shape: QueryShape::UNKNOWN,
                nanos: 500,
                rows: 1,
            });
        }
        let text = to_prometheus_text(&probe.snapshot());
        assert!(
            text.contains("adabt_op_calls_total{op=\"get\"} 3"),
            "{text}"
        );
        assert!(text.contains("adabt_op_latency_nanos{op=\"get\",quantile=\"p50\"}"));
    }

    #[test]
    fn output_with_no_data_is_still_valid_and_empty_of_series() {
        let probe = CollectingProbe::new();
        let text = to_prometheus_text(&probe.snapshot());
        assert!(!text.contains("op=\""));
        assert!(text.contains("adabt_touches_total 0"));
    }

    #[test]
    fn a_label_value_containing_a_quote_is_escaped() {
        let probe = CollectingProbe::new();
        probe.record(Event::FieldFiltered {
            collection: "weird\"name",
            field: "f",
            equality: true,
        });
        let text = to_prometheus_text(&probe.snapshot());
        assert!(text.contains("collection=\"weird\\\"name\""), "{text}");
    }
}
