use crate::event::Event;

/// Sink for observations.
///
/// Implementations must be cheap and non-blocking: probes sit on the hot path,
/// and telemetry that perturbs the workload cannot be used to decide how to
/// optimize that workload.
pub trait Probe: Send + Sync {
    fn record(&self, ev: Event<'_>);
}

/// Discards everything. The default, and what the `off` configuration reduces to.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopProbe;

impl Probe for NoopProbe {
    #[inline(always)]
    fn record(&self, _ev: Event<'_>) {}
}

impl<P: Probe + ?Sized> Probe for std::sync::Arc<P> {
    #[inline]
    fn record(&self, ev: Event<'_>) {
        (**self).record(ev)
    }
}

/// Whether observation is compiled in.
///
/// This constant, not a `cfg` inside the macro, is what makes the feature gate
/// work. `#[cfg(...)]` written in a `macro_rules!` body is evaluated where the
/// macro is *called*, against the calling crate's feature set — so a `cfg` on
/// `feature = "counters"` inside `probe!` would test a feature that
/// `adabt-engine` does not have and silently disable telemetry everywhere.
/// Evaluating `cfg!` here resolves it against this crate, once, correctly.
pub const ENABLED: bool = cfg!(feature = "counters");

/// Whether per-query-shape detail and sampled traces are compiled in.
pub const DETAILED: bool = cfg!(feature = "full");

// `full` enables `counters`, so detailed observation can never be on while basic
// observation is off. Checked at compile time rather than in a test, because a
// feature-flag mistake should fail the build, not one configuration's test run.
const _: () = assert!(
    !DETAILED || ENABLED,
    "feature `full` must enable `counters`"
);

/// Emit an event, skipping the work entirely when telemetry is disabled.
///
/// Expands to a branch on a `const bool`, which the optimizer folds away along
/// with the argument expressions when it is false.
#[macro_export]
macro_rules! probe {
    ($probe:expr, $ev:expr) => {{
        if $crate::probe::ENABLED {
            $crate::probe::Probe::record(&$probe, $ev);
        }
    }};
}

/// Emit an event only under the `full` feature, for observations too expensive
/// to take on every operation.
#[macro_export]
macro_rules! probe_detailed {
    ($probe:expr, $ev:expr) => {{
        if $crate::probe::DETAILED {
            $crate::probe::Probe::record(&$probe, $ev);
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::CollectingProbe;
    use crate::event::{OpKind, QueryShape};

    fn ev() -> Event<'static> {
        Event::Op {
            collection: "c",
            kind: OpKind::Get,
            shape: QueryShape::UNKNOWN,
            nanos: 1,
            rows: 1,
        }
    }

    #[test]
    fn enabled_tracks_this_crates_features_not_the_callers() {
        assert_eq!(ENABLED, cfg!(feature = "counters"));
    }

    #[test]
    fn probe_macro_delivers_events_when_enabled() {
        let p = CollectingProbe::new();
        probe!(p, ev());
        let expected = if ENABLED { 1 } else { 0 };
        assert_eq!(p.snapshot().total_calls(), expected);
    }

    #[test]
    fn noop_probe_discards_everything() {
        let p = NoopProbe;
        probe!(p, ev());
        // Nothing to assert beyond not panicking; the point is that the call
        // type-checks and costs nothing.
    }
}
