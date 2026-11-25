//! The tree engine's OpenTelemetry instruments, registered on a [`Meter`]
//! its caller makes (the `agentfs` binary): `agentfs.not_built`, the
//! latency figures of the volume lock, the lag gate, the tree views' FUSE
//! requests and the journal's checkpoints, and `agentfs.history.unverified`.
//! docs/telemetry.md lists them. Telemetry that is off is the API's no-op
//! meter, which drops every callback: no caller branches.

use std::collections::BTreeMap;
use std::sync::Weak;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

use crate::operator_settings::{Capability, NotBuilt};
use crate::tree_actor::UnverifiedCounts;
use crate::tree_actor::stages::{Stage, Stages};

/// Metric names.
mod names {
    pub const NOT_BUILT: &str = "agentfs.not_built";
    pub const HISTORY_UNVERIFIED: &str = "agentfs.history.unverified";
    pub const LOCK_HOLD: &str = "agentfs.lock.hold";
    pub const LOCK_HOLDS: &str = "agentfs.lock.holds";
    pub const LOCK_HOLD_LONGEST: &str = "agentfs.lock.hold.longest";
    pub const LOCK_WAIT: &str = "agentfs.lock.wait";
    pub const LOCK_WAITS: &str = "agentfs.lock.waits";
    pub const LOCK_WAIT_LONGEST: &str = "agentfs.lock.wait.longest";
    pub const GATE_WAIT: &str = "agentfs.gate.wait";
    pub const GATE_WAITS: &str = "agentfs.gate.waits";
    pub const GATE_WAIT_LONGEST: &str = "agentfs.gate.wait.longest";
    pub const JOURNAL_CHECKPOINTS: &str = "agentfs.journal.checkpoints";
    pub const REQUEST_TIME: &str = "agentfs.request.time";
    pub const REQUESTS: &str = "agentfs.requests";
    pub const REQUEST_LONGEST: &str = "agentfs.request.longest";
}

/// Registers `agentfs.not_built` and the latency figures (lock, gate,
/// requests, checkpoints) on `meter`. Call once per process: the
/// instruments live as long as the meter provider, and registering twice
/// reports every series twice.
///
/// `agentfs.not_built` (unit `{setting}`): one point per capability with
/// configured settings not built, their count, attribute `capability` =
/// [`Capability::as_str`].
///
/// The latency figures are read from the process's [`Stages`] at each
/// collection, each family as seconds, runs and the longest run: the
/// volume lock's holds by `kind` of work (`agentfs.lock.hold`, `.holds`,
/// `.hold.longest`) and the waits to take it (`agentfs.lock.wait`,
/// `.waits`, `.wait.longest`); the lag gate's waits by `cause`
/// (`agentfs.gate.wait`, `.waits`, `.wait.longest`); and the tree views'
/// FUSE requests by `op` (`agentfs.request.time`, `agentfs.requests`,
/// `agentfs.request.longest`); and the journal checkpoints history's
/// compactors published (`agentfs.journal.checkpoints`). Every attribute
/// value is observed, zero included.
pub fn observe_process(meter: &Meter, not_built: &[NotBuilt]) {
    observe_not_built(meter, not_built);
    observe_latency(meter);
}

fn observe_not_built(meter: &Meter, not_built: &[NotBuilt]) {
    let mut counts = BTreeMap::<Capability, u64>::new();
    for setting in not_built {
        *counts.entry(setting.capability).or_default() += 1;
    }
    meter
        .u64_observable_gauge(names::NOT_BUILT)
        .with_unit("{setting}")
        .with_description(
            "Configured operator settings whose capability this volume does not build yet",
        )
        .with_callback(move |observer| {
            for (capability, count) in &counts {
                observer.observe(*count, &[KeyValue::new("capability", capability.as_str())]);
            }
        })
        .build();
}

fn observe_latency(meter: &Meter) {
    let seconds = |nanos: u64| nanos as f64 / 1e9;
    meter
        .u64_observable_counter(names::JOURNAL_CHECKPOINTS)
        .with_unit("{checkpoint}")
        .with_description("Journal checkpoints history's compactors published")
        .with_callback(|observer| observer.observe(Stages::current().checkpoints, &[]))
        .build();
    type Read = fn(&Stages) -> Vec<(Option<KeyValue>, Stage)>;
    let holds: Read = |stages| {
        stages
            .lock_hold
            .by_kind()
            .into_iter()
            .map(|(kind, stage)| (Some(KeyValue::new("kind", kind)), stage))
            .collect()
    };
    let waits: Read = |stages| vec![(None, stages.lock_wait)];
    let gate: Read = |stages| {
        stages
            .gate_wait
            .by_cause()
            .into_iter()
            .map(|(cause, stage)| (Some(KeyValue::new("cause", cause)), stage))
            .collect()
    };
    let requests: Read = |stages| {
        stages
            .requests
            .by_op()
            .into_iter()
            .map(|(op, latency)| (Some(KeyValue::new("op", op)), latency.stage()))
            .collect()
    };
    for (read, time, count, unit, longest, what) in [
        (
            holds,
            names::LOCK_HOLD,
            names::LOCK_HOLDS,
            "{hold}",
            names::LOCK_HOLD_LONGEST,
            "the tree volume's lock held",
        ),
        (
            waits,
            names::LOCK_WAIT,
            names::LOCK_WAITS,
            "{wait}",
            names::LOCK_WAIT_LONGEST,
            "waits to take the tree volume's lock",
        ),
        (
            gate,
            names::GATE_WAIT,
            names::GATE_WAITS,
            "{wait}",
            names::GATE_WAIT_LONGEST,
            "calls held at the lag gate",
        ),
        (
            requests,
            names::REQUEST_TIME,
            names::REQUESTS,
            "{request}",
            names::REQUEST_LONGEST,
            "the tree views' FUSE requests",
        ),
    ] {
        meter
            .f64_observable_counter(time)
            .with_unit("s")
            .with_description(format!("Seconds of {what}"))
            .with_callback(move |observer| {
                for (attr, stage) in read(&Stages::current()) {
                    observer.observe(seconds(stage.nanos), attr.as_slice());
                }
            })
            .build();
        meter
            .u64_observable_counter(count)
            .with_unit(unit)
            .with_description(format!("Runs of {what}"))
            .with_callback(move |observer| {
                for (attr, stage) in read(&Stages::current()) {
                    observer.observe(stage.count, attr.as_slice());
                }
            })
            .build();
        meter
            .f64_observable_gauge(longest)
            .with_unit("s")
            .with_description(format!("The longest run of {what}"))
            .with_callback(move |observer| {
                for (attr, stage) in read(&Stages::current()) {
                    observer.observe(seconds(stage.max_nanos), attr.as_slice());
                }
            })
            .build();
    }
}

/// Registers `agentfs.history.unverified` (unit `{step}`) on `meter`: a
/// tree volume's saves history flagged because the builder could not check
/// them (docs/trees.md, "When history cannot check a save"), read from
/// `counts` at each collection while it lives, one point per `verdict`:
/// `unresolved` (no verdict yet) and `mismatch_confirmed` (checked again
/// and still disagreeing), both always observed, zero included. Call once
/// per served volume.
pub fn observe_unverified(meter: &Meter, counts: Weak<dyn Fn() -> UnverifiedCounts + Send + Sync>) {
    meter
        .u64_observable_gauge(names::HISTORY_UNVERIFIED)
        .with_unit("{step}")
        .with_description("Saves history flagged because the builder could not check them")
        .with_callback(move |observer| {
            if let Some(counts) = counts.upgrade() {
                let counts = counts();
                observer.observe(counts.unresolved, &[KeyValue::new("verdict", "unresolved")]);
                observer.observe(
                    counts.mismatch_confirmed,
                    &[KeyValue::new("verdict", "mismatch_confirmed")],
                );
            }
        })
        .build();
}

#[cfg(test)]
mod tests;
