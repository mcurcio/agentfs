//! The tree engine's instruments, read through the in-memory metric reader.

use std::sync::Arc;

use agentfs_test_metrics::MetricReader;
use opentelemetry::InstrumentationScope;

use super::*;

/// The scope of the instruments under test: any, the reader reads them all.
fn scope() -> InstrumentationScope {
    InstrumentationScope::builder("agentfs").build()
}

/// One point per capability with settings not built, their count; none
/// for a capability with nothing configured.
#[test]
fn not_built_settings_are_counted_by_capability() {
    let metrics = MetricReader::new();
    let setting = |key: &str, capability| NotBuilt {
        key: key.to_owned(),
        capability,
    };
    observe_process(
        &metrics.meter(scope()),
        &[
            setting("remote.origin.url", Capability::Push),
            setting("remote.origin.push", Capability::Push),
            setting("agentfs.patchremote", Capability::Patches),
        ],
    );
    let gauge = |capability| metrics.gauge_u64(names::NOT_BUILT, &[("capability", capability)]);
    assert_eq!(gauge("push"), Some(2));
    assert_eq!(gauge("patches"), Some(1));
    assert_eq!(gauge("lfs"), None);
    assert_eq!(gauge("other"), None);
}

/// The latency figures: every kind of hold, gate cause and request op
/// observed, zeros included, and a hold and a wait through a real
/// volume counted.
#[test]
fn the_latency_figures_are_observed_by_attribute() {
    use crate::tree_actor::Recording;
    let metrics = MetricReader::new();
    observe_process(&metrics.meter(scope()), &[]);
    assert!(metrics.sum(names::JOURNAL_CHECKPOINTS, &[]).is_some());
    for cause in ["lag", "overlay"] {
        assert!(
            metrics
                .sum(names::GATE_WAITS, &[("cause", cause)])
                .is_some(),
            "{cause}"
        );
    }
    for op in ["lookup", "getattr", "open", "read", "flush", "other"] {
        assert!(
            metrics.sum(names::REQUESTS, &[("op", op)]).is_some(),
            "{op}"
        );
        assert!(
            metrics
                .gauge_f64(names::REQUEST_LONGEST, &[("op", op)])
                .is_some()
        );
    }
    let holds = |kind| metrics.sum(names::LOCK_HOLDS, &[("kind", kind)]);
    let kinds = [
        "compaction",
        "catch_up",
        "drops",
        "close",
        "take",
        "fork_copy",
        "page_image",
        "other",
    ];
    for kind in kinds {
        assert!(holds(kind).is_some(), "{kind} observed");
        assert!(
            metrics
                .gauge_f64(names::LOCK_HOLD_LONGEST, &[("kind", kind)])
                .is_some()
        );
    }
    let (other, waits) = (
        holds("other").unwrap(),
        metrics.sum(names::LOCK_WAITS, &[]).unwrap(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let identity = agentfs_contracts::volume::VolumeIdentity {
        name: agentfs_contracts::name::VolumeName::parse("test").unwrap(),
        id: agentfs_contracts::volume::VolumeId(uuid::Uuid::from_u128(1)),
    };
    let rec = Recording::create(
        &tmp.path().join("state"),
        &identity,
        None,
        &agentfs_contracts::pod::PodContext::default(),
    )
    .unwrap();
    drop(rec.actor().unwrap());
    assert!(holds("other").unwrap() > other);
    assert!(metrics.sum(names::LOCK_WAITS, &[]).unwrap() > waits);
    assert!(
        metrics
            .point(names::LOCK_HOLD, &[("kind", "other")])
            .is_some()
            && metrics.point(names::LOCK_WAIT, &[]).is_some()
    );
}

/// A11: both verdicts are observed at every collection, zeros included,
/// read from the counts as they are then.
#[test]
fn unverified_saves_are_observed_by_verdict_zeros_included() {
    let metrics = MetricReader::new();
    let counts = Arc::new(std::sync::Mutex::new(UnverifiedCounts::default()));
    let read = Arc::clone(&counts);
    let gauge: Arc<dyn Fn() -> UnverifiedCounts + Send + Sync> =
        Arc::new(move || *read.lock().unwrap());
    observe_unverified(&metrics.meter(scope()), Arc::downgrade(&gauge));
    let point = |verdict| metrics.gauge_u64(names::HISTORY_UNVERIFIED, &[("verdict", verdict)]);
    assert_eq!(point("unresolved"), Some(0));
    assert_eq!(point("mismatch_confirmed"), Some(0));
    *counts.lock().unwrap() = UnverifiedCounts {
        unresolved: 2,
        mismatch_confirmed: 1,
    };
    assert_eq!(point("unresolved"), Some(2));
    assert_eq!(point("mismatch_confirmed"), Some(1));
}
