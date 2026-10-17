//! OpenTelemetry instrumentation (design §13).
//!
//! The engine is written against the OpenTelemetry API only; the binary
//! chooses the SDK and the exporters and hands the engine a [`Telemetry`]
//! handle. [`Telemetry::noop`] (the default) builds nothing: no instruments,
//! no spans, and event sinks are passed through unwrapped, so telemetry that
//! is not configured costs a branch per call.
//!
//! Signals come from three places:
//!
//! - **Alarm events.** [`Telemetry::sink`] wraps the process's
//!   [`EventSink`]: each event updates its counter and, for alarms and a
//!   fork's lifecycle ([`Event::is_logged`]), becomes a structured log
//!   record, then goes on to the next sink. The caller
//!   wraps once per process; the engine never wraps, so nothing is counted
//!   twice.
//! - **Direct instrumentation.** The recorder, committer and pushers call
//!   the handle for `agentfs.record.duration`, `agentfs.commit.count` and
//!   the `agentfs.record`, `agentfs.commit`, `agentfs.push`, `agentfs.patch`,
//!   `agentfs.fork.finish` and `agentfs.fork.publish` spans, and the
//!   finisher for `agentfs.fork.publish.duration`. Each
//!   span is current while its operation runs, so alarm logs raised inside
//!   it carry its trace context.
//! - **Observable gauges**, read from state when metrics are collected:
//!   the journal backlog, each remote's push backlog, age and divergence,
//!   the start-branch alarm of a started volume (`agentfs.start.moved`),
//!   misconfigured remotes, the size of the state area and the patch queue
//!   (`agentfs.patch.queue_depth`, `agentfs.patch.blocked`, from
//!   `meta/patch.json`).
//!
//! Metric attributes are short and low-cardinality (`trigger`, `remote`,
//! `reason`, `stage`, `part`); paths never become metric attributes. Span
//! and log attributes are namespaced `agentfs.*`.

use std::fmt;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::global::BoxedTracer;
use opentelemetry::logs::{AnyValue, LogRecord as _, Logger, Severity};
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, ContextGuard, InstrumentationScope, KeyValue};

use crate::config::Settings;
use crate::events::{Event, EventSink, Events};
use crate::journal::Journal;
use crate::push::{self, RemoteBacklog};
use crate::record::Trigger;
use crate::volume::StateDir;

/// Metric names (design §13).
pub mod names {
    pub const RECORD_DURATION: &str = "agentfs.record.duration";
    pub const RECORD_FAILURES: &str = "agentfs.record.failures";
    pub const JOURNAL_BACKLOG: &str = "agentfs.journal.backlog";
    pub const COMMIT_COUNT: &str = "agentfs.commit.count";
    pub const COMMIT_FAILURES: &str = "agentfs.commit.failures";
    pub const REPACK_FAILURES: &str = "agentfs.repack.failures";
    pub const PUSH_UNPUSHED_COMMITS: &str = "agentfs.push.unpushed_commits";
    pub const PUSH_OLDEST_UNPUSHED_AGE: &str = "agentfs.push.oldest_unpushed_age";
    pub const PUSH_FAILURES: &str = "agentfs.push.failures";
    pub const PUSH_LIMITED: &str = "agentfs.push.limited";
    pub const PUSH_DIVERGED: &str = "agentfs.push.diverged";
    pub const START_MOVED: &str = "agentfs.start.moved";
    pub const PRECHECK_REJECTED_PATHS: &str = "agentfs.precheck.rejected_paths";
    pub const STATE_BYTES: &str = "agentfs.state.bytes";
    pub const REMOTE_MISCONFIGURED: &str = "agentfs.remote.misconfigured";
    pub const REMOTES_MODE: &str = "agentfs.remotes.mode";
    pub const PATCH_QUEUE_DEPTH: &str = "agentfs.patch.queue_depth";
    pub const PATCH_BLOCKED: &str = "agentfs.patch.blocked";
    pub const FRONT_ROUND_TRIP: &str = "agentfs.front.round_trip";
    pub const FRONT_SERVICE_FAILURES: &str = "agentfs.front.service_failures";
    pub const HISTORY_SERVICE_FRONTS: &str = "agentfs.history_service.fronts";
    pub const HISTORY_SERVICE_SUPERSEDED: &str = "agentfs.history_service.superseded";
    pub const HISTORY_SERVICE_RESYNCS: &str = "agentfs.history_service.resyncs";
    pub const FUSE_CREATE_AS_CALLER_FALLBACKS: &str = "agentfs.fuse.create_as_caller_fallbacks";
    pub const FUSE_RACY_RECHECKS: &str = "agentfs.fuse.racy_rechecks";
    pub const RECONCILE_RECORDS: &str = "agentfs.reconcile.records";
    pub const RECONCILE_FAILURES: &str = "agentfs.reconcile.failures";
    pub const NESTED_GITDIR_HEURISTIC: &str = "agentfs.nested.gitdir_heuristic";
    pub const FORK_PUBLISH_DURATION: &str = "agentfs.fork.publish.duration";
    /// Recorded by the binary, which owns the exporters (design §13).
    pub const TELEMETRY_EXPORT_FAILURES: &str = "agentfs.telemetry.export_failures";
}

/// Span names (design §13).
pub mod spans {
    pub const RECORD: &str = "agentfs.record";
    pub const COMMIT: &str = "agentfs.commit";
    pub const PUSH: &str = "agentfs.push";
    pub const PATCH: &str = "agentfs.patch";
    pub const FORK_FINISH: &str = "agentfs.fork.finish";
    pub const FORK_PUBLISH: &str = "agentfs.fork.publish";
}

/// Bucket boundaries of `agentfs.record.duration`, in seconds: the
/// synchronous step is usually well under a millisecond, but a slow disk
/// or a large copy can take seconds.
const RECORD_DURATION_BOUNDARIES: [f64; 16] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
];

/// Bucket boundaries of `agentfs.fork.publish.duration`, in seconds: from
/// a publish at once to one waiting a day for its fork remote.
const FORK_PUBLISH_BOUNDARIES: [f64; 12] = [
    0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 3600.0, 21600.0, 86400.0,
];

/// The longest the push gauges reuse one backlog walk (see
/// [`BacklogSnapshot`]).
const BACKLOG_SNAPSHOT_TTL: Duration = Duration::from_secs(1);

/// The instrumentation scope of every agentfs meter, tracer and logger.
pub fn scope() -> InstrumentationScope {
    InstrumentationScope::builder("agentfs")
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

/// Turns alarm events into log records.
pub trait AlarmLog: Send + Sync {
    fn log(&self, event: &Event);
}

/// The process's telemetry. Cheap to clone; [`Telemetry::noop`] when
/// telemetry is not configured.
#[derive(Clone, Default)]
pub struct Telemetry {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    meter: Meter,
    tracer: Option<BoxedTracer>,
    alarms: Option<Box<dyn AlarmLog>>,
    record_duration: Histogram<f64>,
    record_failures: Counter<u64>,
    commit_count: Counter<u64>,
    commit_failures: Counter<u64>,
    repack_failures: Counter<u64>,
    reconcile_failures: Counter<u64>,
    reconcile_records: Counter<u64>,
    push_failures: Counter<u64>,
    push_limited: Counter<u64>,
    rejected_paths: Counter<u64>,
    front_round_trip: Histogram<f64>,
    front_service_failures: Counter<u64>,
    superseded: Counter<u64>,
    resyncs: Counter<u64>,
    create_as_caller_fallbacks: Counter<u64>,
    racy_rechecks: Counter<u64>,
    fork_publish_duration: Histogram<f64>,
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            None => f.write_str("Telemetry(noop)"),
            Some(inner) => f
                .debug_struct("Telemetry")
                .field("traces", &inner.tracer.is_some())
                .field("logs", &inner.alarms.is_some())
                .finish_non_exhaustive(),
        }
    }
}

impl Telemetry {
    /// Telemetry that records nothing and builds nothing.
    pub fn noop() -> Self {
        Self::default()
    }

    /// Telemetry over `meter`, with spans from `tracer` and alarm logs to
    /// `alarms` when given.
    pub fn new(
        meter: &Meter,
        tracer: Option<BoxedTracer>,
        alarms: Option<Box<dyn AlarmLog>>,
    ) -> Self {
        let counter = |name: &'static str, unit: &'static str, description: &'static str| {
            meter
                .u64_counter(name)
                .with_unit(unit)
                .with_description(description)
                .build()
        };
        let inner = Inner {
            record_duration: meter
                .f64_histogram(names::RECORD_DURATION)
                .with_unit("s")
                .with_description("Time spent in the synchronous step of a record, by trigger")
                .with_boundaries(RECORD_DURATION_BOUNDARIES.to_vec())
                .build(),
            record_failures: counter(
                names::RECORD_FAILURES,
                "{record}",
                "Records whose synchronous step failed, by trigger",
            ),
            commit_count: counter(names::COMMIT_COUNT, "{commit}", "Commits made"),
            commit_failures: counter(
                names::COMMIT_FAILURES,
                "{failure}",
                "Commit passes that failed; the records stay journaled",
            ),
            repack_failures: counter(
                names::REPACK_FAILURES,
                "{failure}",
                "Periodic repacks of the history that failed",
            ),
            reconcile_failures: counter(
                names::RECONCILE_FAILURES,
                "{move}",
                "Moved trees given up on after repeated failures to reconcile them (design \
                 §3.1); each also raises ReconcileFailed",
            ),
            reconcile_records: counter(
                names::RECONCILE_RECORDS,
                "{record}",
                "Records made by reconciling moved trees: files the rules decide differently \
                 at the new path, or changed around the move (design §3.1)",
            ),
            push_failures: counter(
                names::PUSH_FAILURES,
                "{failure}",
                "Failed push attempts, by remote, reason and stage (git or lfs)",
            ),
            push_limited: counter(
                names::PUSH_LIMITED,
                "{push}",
                "Pushes that sent only part of the backlog because it was larger than the \
                 remote's agentfsMaxPushSize, by remote (design §11.3)",
            ),
            rejected_paths: counter(
                names::PRECHECK_REJECTED_PATHS,
                "{path}",
                "Paths refused by the pre-commit checks, by stage and reason",
            ),
            front_round_trip: meter
                .f64_histogram(names::FRONT_ROUND_TRIP)
                .with_unit("s")
                .with_description(
                    "Time a front waits for the history service to answer, by request (design §14)",
                )
                .with_boundaries(RECORD_DURATION_BOUNDARIES.to_vec())
                .build(),
            front_service_failures: counter(
                names::FRONT_SERVICE_FAILURES,
                "{failure}",
                "Requests a front could not get answered by the history service, by stage",
            ),
            superseded: counter(
                names::HISTORY_SERVICE_SUPERSEDED,
                "{write}",
                "Front writes the history service skipped because a newer state of the path was \
                 already journaled (design §14.1)",
            ),
            resyncs: counter(
                names::HISTORY_SERVICE_RESYNCS,
                "{record}",
                "Paths the history service recorded again as the backing tree has them, after a \
                 front's change it could not order against another's (design §14.1)",
            ),
            create_as_caller_fallbacks: counter(
                names::FUSE_CREATE_AS_CALLER_FALLBACKS,
                "{object}",
                "Objects a root daemon created as itself and then gave to the caller, because \
                 creating as the caller was refused or could not be set up (design §6)",
            ),
            racy_rechecks: counter(
                names::FUSE_RACY_RECHECKS,
                "{boundary}",
                "Boundaries that would have recorded a file again only because its last record \
                 was racy, by outcome: skipped (its bytes were the frozen copy still last \
                 journaled at its path) or recorded (design §3.1)",
            ),
            fork_publish_duration: meter
                .f64_histogram(names::FORK_PUBLISH_DURATION)
                .with_unit("s")
                .with_description(
                    "Seconds from a fork's finish commit to its fork remote holding the change \
                     and finish refs, by remote; recorded once, when the published mark is \
                     written (design §11.5)",
                )
                .with_boundaries(FORK_PUBLISH_BOUNDARIES.to_vec())
                .build(),
            meter: meter.clone(),
            tracer,
            alarms,
        };
        meter
            .u64_observable_counter(names::NESTED_GITDIR_HEURISTIC)
            .with_unit("{target}")
            .with_description(
                "Absolute gitdir: or .git symlink targets under no known view of the tree, found \
                 by trying their suffixes (design §5); configure --workload-mount (CSI: the workloadMounts \
                 parameter) to map them exactly",
            )
            .with_callback(|observer| observer.observe(crate::nested::heuristic_matches(), &[]))
            .build();
        Self {
            inner: Some(Arc::new(inner)),
        }
    }

    /// True unless this is [`Telemetry::noop`].
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Wraps `next`: each event updates its counter and, if it is logged
    /// ([`Event::is_logged`]: alarms and a fork's lifecycle), becomes a log
    /// record; then it goes on to `next`. Never blocks.
    ///
    /// Wrap once per process and give the result to every engine
    /// constructor: a bare sink loses the alarm metrics, and wrapping twice
    /// counts them twice. A noop handle returns `next` itself.
    pub fn sink(&self, next: Events) -> Events {
        match &self.inner {
            None => next,
            Some(inner) => Arc::new(TelemetrySink {
                inner: inner.clone(),
                next,
            }),
        }
    }

    /// Registers the gauges read from the state area at each collection:
    /// each remote's unpushed commits, oldest unpushed age and divergence,
    /// misconfigured remotes, and the state area's size by part.
    ///
    /// Call once per process: the gauges live as long as the meter
    /// provider, and registering twice reports every series twice. A
    /// callback that cannot read the state observes nothing.
    pub fn observe_volume(&self, state: &StateDir, settings: &Settings) {
        let Some(inner) = &self.inner else {
            return;
        };
        let backlog = Arc::new(BacklogSnapshot {
            state: state.clone(),
            settings: settings.clone(),
            cached: Mutex::new(None),
        });
        let snapshot = backlog.clone();
        inner
            .meter
            .u64_observable_gauge(names::PUSH_UNPUSHED_COMMITS)
            .with_unit("{commit}")
            .with_description("Commits on the branch the remote is not known to have")
            .with_callback(move |observer| {
                for remote in snapshot.get(0) {
                    observer.observe(remote.unpushed as u64, &[remote_attr(&remote.remote)]);
                }
            })
            .build();
        let snapshot = backlog.clone();
        inner
            .meter
            .f64_observable_gauge(names::PUSH_OLDEST_UNPUSHED_AGE)
            .with_unit("s")
            .with_description(
                "Seconds since the oldest unpushed commit was made; 0 when nothing is unpushed",
            )
            .with_callback(move |observer| {
                let now = unix_now_s();
                for remote in snapshot.get(1) {
                    let age = remote
                        .oldest_unpushed_s
                        .map_or(0.0, |t| (now - t as f64).max(0.0));
                    observer.observe(age, &[remote_attr(&remote.remote)]);
                }
            })
            .build();
        let snapshot = backlog;
        inner
            .meter
            .u64_observable_gauge(names::PUSH_DIVERGED)
            .with_unit("{remote}")
            .with_description("1 while the remote branch has diverged (design §11.3)")
            .with_callback(move |observer| {
                for remote in snapshot.get(2) {
                    observer.observe(u64::from(remote.diverged), &[remote_attr(&remote.remote)]);
                }
            })
            .build();
        if let Some(start) = &settings.start {
            // As the last attempt to the start remote saw it: 0 before the
            // first one, and after one that did not raise it.
            let remote = start.remote.clone();
            let push_state = state.push_state(&remote);
            inner
                .meter
                .u64_observable_gauge(names::START_MOVED)
                .with_unit("{remote}")
                .with_description(
                    "1 while the branch this volume started from holds commits it does not \
                     have (StartBranchMoved, F8)",
                )
                .with_callback(move |observer| {
                    let moved = push::PushState::load(&push_state).start_moved.is_some();
                    observer.observe(u64::from(moved), &[remote_attr(&remote)]);
                })
                .build();
        }
        let invalid: Vec<String> = settings
            .invalid_remotes
            .iter()
            .map(|r| r.name.clone())
            .collect();
        inner
            .meter
            .u64_observable_gauge(names::REMOTE_MISCONFIGURED)
            .with_unit("{remote}")
            .with_description("1 for each remote section that is invalid and never pushed")
            .with_callback(move |observer| {
                for name in &invalid {
                    observer.observe(1, &[remote_attr(name)]);
                }
            })
            .build();
        let mode = settings.remotes.mode();
        inner
            .meter
            .u64_observable_gauge(names::REMOTES_MODE)
            .with_unit("{volume}")
            .with_description(
                "1, with the volume's remotes mode: pushed, none_valid, local_only or undeclared \
                 (F8 S3)",
            )
            .with_callback(move |observer| observer.observe(1, &[KeyValue::new("mode", mode)]))
            .build();
        let patch_state = state.patch_state();
        inner
            .meter
            .u64_observable_gauge(names::PATCH_QUEUE_DEPTH)
            .with_unit("{patch}")
            .with_description("Patches queued and not applied, as of the last mount (design §12)")
            .with_callback(move |observer| {
                let state = crate::patch::status::PatchState::load(&patch_state);
                observer.observe(state.queue_depth, &[]);
            })
            .build();
        let patch_state = state.patch_state();
        inner
            .meter
            .u64_observable_gauge(names::PATCH_BLOCKED)
            .with_unit("{queue}")
            .with_description("1 while the patch queue is blocked (design §12)")
            .with_callback(move |observer| {
                let state = crate::patch::status::PatchState::load(&patch_state);
                observer.observe(u64::from(state.blocked.is_some()), &[]);
            })
            .build();
        let state = state.clone();
        inner
            .meter
            .u64_observable_gauge(names::STATE_BYTES)
            .with_unit("By")
            .with_description("Apparent size of the state area, by part")
            .with_callback(move |observer| {
                for (part, bytes) in state.usage() {
                    observer.observe(bytes, &[KeyValue::new("part", part)]);
                }
            })
            .build();
    }

    /// Registers `agentfs.journal.backlog`, read from `journal` while it is
    /// open. Called by [`crate::volume::open`].
    pub(crate) fn observe_journal(&self, journal: Weak<Journal>) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner
            .meter
            .u64_observable_gauge(names::JOURNAL_BACKLOG)
            .with_unit("{record}")
            .with_description("Records durable in the journal but not yet committed")
            .with_callback(move |observer| {
                if let Some(journal) = journal.upgrade() {
                    observer.observe(journal.backlog() as u64, &[]);
                }
            })
            .build();
    }

    /// Records one call of the synchronous step.
    pub(crate) fn record_duration(&self, trigger: Trigger, elapsed: Duration) {
        if let Some(inner) = &self.inner {
            inner.record_duration.record(
                elapsed.as_secs_f64(),
                &[KeyValue::new("trigger", trigger.as_str())],
            );
        }
    }

    /// Records one answered request of a front (design §14): `request` is
    /// `record`, `tracked` or `ping`.
    pub(crate) fn front_round_trip(&self, request: &'static str, elapsed: Duration) {
        if let Some(inner) = &self.inner {
            inner
                .front_round_trip
                .record(elapsed.as_secs_f64(), &[KeyValue::new("request", request)]);
        }
    }

    /// Counts one request a front could not get answered: `stage` is
    /// `connect`, `request`, `timeout`, `refused` or `service`.
    pub(crate) fn front_service_failure(&self, stage: &'static str) {
        if let Some(inner) = &self.inner {
            inner
                .front_service_failures
                .add(1, &[KeyValue::new("stage", stage)]);
        }
    }

    /// Counts one front write skipped as superseded (design §14.1).
    pub(crate) fn superseded(&self) {
        if let Some(inner) = &self.inner {
            inner.superseded.add(1, &[]);
        }
    }

    /// Counts one path recorded again as the backing tree has it (design
    /// §14.1).
    pub(crate) fn resynced(&self) {
        if let Some(inner) = &self.inner {
            inner.resyncs.add(1, &[]);
        }
    }

    /// Counts one object created as the daemon, then given to its caller,
    /// because creating it as the caller failed (design §6).
    pub fn create_as_caller_fallback(&self) {
        if let Some(inner) = &self.inner {
            inner.create_as_caller_fallbacks.add(1, &[]);
        }
    }

    /// Counts a record made by reconciling a moved tree (design §3.1).
    pub fn reconcile_record(&self) {
        if let Some(inner) = &self.inner {
            inner.reconcile_records.add(1, &[]);
        }
    }

    /// Counts one boundary that found a file unchanged but racy (design
    /// §3.1): `skipped` if its bytes were proven recorded, else `recorded`.
    pub fn racy_recheck(&self, skipped: bool) {
        if let Some(inner) = &self.inner {
            let outcome = if skipped { "skipped" } else { "recorded" };
            inner
                .racy_rechecks
                .add(1, &[KeyValue::new("outcome", outcome)]);
        }
    }

    /// Registers `agentfs.history_service.fronts`, the number of distinct
    /// fronts connected, read from `fronts` while the server runs.
    pub(crate) fn observe_fronts(&self, fronts: Weak<dyn Fn() -> usize + Send + Sync>) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner
            .meter
            .u64_observable_gauge(names::HISTORY_SERVICE_FRONTS)
            .with_unit("{front}")
            .with_description("Fronts connected to this history service (design §14)")
            .with_callback(move |observer| {
                if let Some(fronts) = fronts.upgrade() {
                    observer.observe(fronts() as u64, &[]);
                }
            })
            .build();
    }

    /// Records one fork published to `remote`, `elapsed` after its finish
    /// commit (design §11.5).
    pub(crate) fn fork_published(&self, remote: &str, elapsed: Duration) {
        if let Some(inner) = &self.inner {
            inner
                .fork_publish_duration
                .record(elapsed.as_secs_f64(), &[remote_attr(remote)]);
        }
    }

    /// Counts one commit made.
    pub(crate) fn commit_made(&self) {
        if let Some(inner) = &self.inner {
            inner.commit_count.add(1, &[]);
        }
    }

    /// Starts span `name` and makes it current until it ends. `attrs` is
    /// only called when tracing is on.
    pub(crate) fn span(
        &self,
        name: &'static str,
        attrs: impl FnOnce() -> Vec<KeyValue>,
    ) -> ActiveSpan {
        let Some(tracer) = self.inner.as_ref().and_then(|i| i.tracer.as_ref()) else {
            return ActiveSpan { active: None };
        };
        let span = tracer
            .span_builder(name)
            .with_kind(SpanKind::Internal)
            .with_attributes(attrs())
            .start(tracer);
        let cx = Context::current_with_span(span);
        let guard = cx.clone().attach();
        ActiveSpan {
            active: Some((cx, guard)),
        }
    }
}

fn remote_attr(remote: &str) -> KeyValue {
    KeyValue::new("remote", remote.to_owned())
}

fn unix_now_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Every remote's push backlog, shared by the three push gauges so one
/// collection walks each branch once.
///
/// Each gauge is a reader. A reader asking again for a snapshot it has
/// already used means a new collection has started, so the walk is redone;
/// so is a snapshot older than [`BACKLOG_SNAPSHOT_TTL`]. The result is the
/// same whatever order the SDK calls the gauges in.
struct BacklogSnapshot {
    state: StateDir,
    settings: Settings,
    cached: Mutex<Option<Snapshot>>,
}

struct Snapshot {
    at: Instant,
    backlog: Vec<RemoteBacklog>,
    /// One bit per reader that has used this snapshot.
    used: u8,
}

impl BacklogSnapshot {
    fn get(&self, reader: u8) -> Vec<RemoteBacklog> {
        let bit = 1u8 << reader;
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = cached
            .as_ref()
            .is_some_and(|s| s.used & bit == 0 && s.at.elapsed() < BACKLOG_SNAPSHOT_TTL);
        if !fresh {
            // An unreadable repository observes nothing this time.
            *cached = Some(Snapshot {
                at: Instant::now(),
                backlog: push::backlog(&self.state, &self.settings).unwrap_or_default(),
                used: 0,
            });
        }
        match cached.as_mut() {
            Some(snapshot) => {
                snapshot.used |= bit;
                snapshot.backlog.clone()
            }
            None => Vec::new(),
        }
    }
}

/// A span that is current until it ends. Does nothing when tracing is off.
/// Dropping it ends the span.
#[must_use = "the span ends when this is dropped"]
pub(crate) struct ActiveSpan {
    active: Option<(Context, ContextGuard)>,
}

impl ActiveSpan {
    /// Sets one attribute.
    pub(crate) fn set(&self, attr: KeyValue) {
        if let Some((cx, _)) = &self.active {
            cx.span().set_attribute(attr);
        }
    }

    /// Sets one attribute, built only if the span is recording.
    pub(crate) fn set_with(&self, attr: impl FnOnce() -> KeyValue) {
        if let Some((cx, _)) = &self.active {
            let span = cx.span();
            if span.is_recording() {
                span.set_attribute(attr());
            }
        }
    }

    /// Marks the span as failed.
    pub(crate) fn error(&self, message: impl fmt::Display) {
        if let Some((cx, _)) = &self.active {
            cx.span().set_status(Status::error(message.to_string()));
        }
    }

    /// Ends the span and stops it being current.
    pub(crate) fn end(self) {}
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        if let Some((cx, guard)) = self.active.take() {
            cx.span().end();
            drop(guard);
        }
    }
}

/// The sink [`Telemetry::sink`] returns.
struct TelemetrySink {
    inner: Arc<Inner>,
    next: Events,
}

impl EventSink for TelemetrySink {
    fn emit(&self, event: Event) {
        let inner = &self.inner;
        match &event {
            Event::PathRefused { reason, stage, .. } => inner.rejected_paths.add(
                1,
                &[
                    KeyValue::new("stage", stage.as_str()),
                    KeyValue::new("reason", reason.label()),
                ],
            ),
            Event::RecordFailed { trigger, .. } => inner
                .record_failures
                .add(1, &[KeyValue::new("trigger", trigger.as_str())]),
            Event::CommitFailed { .. } => inner.commit_failures.add(1, &[]),
            Event::RepackFailed { .. } => inner.repack_failures.add(1, &[]),
            Event::ReconcileFailed { .. } => inner.reconcile_failures.add(1, &[]),
            Event::PushFailed {
                remote,
                kind,
                stage,
                ..
            } => inner.push_failures.add(
                1,
                &[
                    remote_attr(remote),
                    KeyValue::new("reason", kind.as_str()),
                    KeyValue::new("stage", stage.as_str()),
                ],
            ),
            Event::PushLimited { remote, .. } => inner.push_limited.add(1, &[remote_attr(remote)]),
            // Their gauges are read from state.
            Event::Diverged { .. }
            | Event::StartBranchMoved { .. }
            | Event::RemoteMisconfigured { .. }
            | Event::RemotesUndeclared { .. }
            | Event::PushSucceeded { .. }
            | Event::PatchApplied { .. }
            | Event::PatchDuplicate { .. }
            | Event::PatchBlocked { .. }
            | Event::PatchIgnored { .. }
            | Event::PatchFetchFailed { .. }
            | Event::ForkFinished { .. }
            | Event::ForkPublished { .. }
            | Event::ForkRefused { .. }
            | Event::ForkAbandoned { .. }
            | Event::HistoryServiceUnavailable { .. }
            | Event::HistoryServiceRestored { .. }
            | Event::IdentityFallback { .. } => {}
        }
        if event.is_logged()
            && let Some(alarms) = &inner.alarms
        {
            alarms.log(&event);
        }
        self.next.emit(event);
    }
}

/// [`AlarmLog`] over an OpenTelemetry [`Logger`]: one log record per
/// alarm, with the event's name, a severity, its message as the body and
/// its fields as `agentfs.*` attributes.
pub struct OtelAlarmLog<L>(L);

impl<L: Logger + Send + Sync> OtelAlarmLog<L> {
    pub fn new(logger: L) -> Self {
        Self(logger)
    }
}

impl<L> fmt::Debug for OtelAlarmLog<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OtelAlarmLog")
    }
}

/// The log record's event name for `event`.
fn event_name(event: &Event) -> &'static str {
    match event {
        Event::PathRefused { .. } => "agentfs.path_refused",
        Event::RecordFailed { .. } => "agentfs.record_failed",
        Event::CommitFailed { .. } => "agentfs.commit_failed",
        Event::RepackFailed { .. } => "agentfs.repack_failed",
        Event::ReconcileFailed { .. } => "agentfs.reconcile_failed",
        Event::PushFailed { .. } => "agentfs.push_failed",
        Event::Diverged { .. } => "agentfs.diverged",
        Event::StartBranchMoved { .. } => "agentfs.start_branch_moved",
        Event::PushLimited { .. } => "agentfs.push_limited",
        Event::RemoteMisconfigured { .. } => "agentfs.remote_misconfigured",
        Event::RemotesUndeclared { .. } => "agentfs.remotes_undeclared",
        Event::PushSucceeded { .. } => "agentfs.push_succeeded",
        Event::PatchApplied { .. } => "agentfs.patch_applied",
        Event::PatchDuplicate { .. } => "agentfs.patch_duplicate",
        Event::PatchBlocked { .. } => "agentfs.patch_blocked",
        Event::PatchIgnored { .. } => "agentfs.patch_ignored",
        Event::PatchFetchFailed { .. } => "agentfs.patch_fetch_failed",
        Event::ForkFinished { .. } => "agentfs.fork_finished",
        Event::ForkPublished { .. } => "agentfs.fork_published",
        Event::ForkRefused { .. } => "agentfs.fork_refused",
        Event::ForkAbandoned { .. } => "agentfs.fork_abandoned",
        Event::HistoryServiceUnavailable { .. } => "agentfs.history_service_unavailable",
        Event::HistoryServiceRestored { .. } => "agentfs.history_service_restored",
        Event::IdentityFallback { .. } => "agentfs.identity_fallback",
    }
}

/// `event`'s fields as log attributes.
fn log_attributes(event: &Event) -> Vec<(&'static str, AnyValue)> {
    let s = |v: &str| AnyValue::from(v.to_owned());
    let oid = |o: &git2::Oid| AnyValue::from(o.to_string());
    let mut out = Vec::new();
    match event {
        Event::PathRefused {
            path,
            reason,
            stage,
        } => {
            out.push(("agentfs.path", s(path)));
            out.push(("agentfs.reason", AnyValue::from(reason.label())));
            if let Some(id) = reason.fsck_id() {
                out.push(("agentfs.fsck_id", AnyValue::from(id)));
            }
            out.push(("agentfs.stage", AnyValue::from(stage.as_str())));
        }
        Event::RecordFailed {
            trigger,
            paths,
            error,
        } => {
            out.push(("agentfs.trigger", AnyValue::from(trigger.as_str())));
            let paths: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
            out.push(("agentfs.paths", AnyValue::from(paths.join(", "))));
            out.push(("agentfs.error", s(error)));
        }
        Event::CommitFailed { error, backlog } => {
            out.push(("agentfs.error", s(error)));
            out.push(("agentfs.backlog", AnyValue::Int(*backlog as i64)));
        }
        Event::RepackFailed { error } => out.push(("agentfs.error", s(error))),
        Event::ReconcileFailed { from, to, error } => {
            out.push(("agentfs.from", s(from.as_str())));
            out.push(("agentfs.path", s(to.as_str())));
            out.push(("agentfs.error", s(error)));
        }
        Event::PushFailed {
            remote,
            kind,
            message,
            head,
            last_pushed,
            consecutive_failures,
            retry_in,
            stage,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.kind", AnyValue::from(kind.as_str())));
            out.push(("agentfs.push.stage", AnyValue::from(stage.as_str())));
            out.push(("agentfs.message", s(message)));
            if let Some(head) = head {
                out.push(("agentfs.head", oid(head)));
            }
            if let Some(last) = last_pushed {
                out.push(("agentfs.last_pushed", oid(last)));
            }
            out.push((
                "agentfs.consecutive_failures",
                AnyValue::from(*consecutive_failures),
            ));
            out.push((
                "agentfs.retry_in_seconds",
                AnyValue::from(retry_in.as_secs_f64()),
            ));
        }
        Event::Diverged {
            remote,
            local_head,
            remote_head,
            diverged_ref,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.local_head", oid(local_head)));
            if let Some(r) = remote_head {
                out.push(("agentfs.remote_head", oid(r)));
            }
            out.push(("agentfs.diverged_ref", s(diverged_ref)));
        }
        Event::StartBranchMoved {
            remote,
            refname,
            pinned,
            tip,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.start.ref", s(refname)));
            out.push(("agentfs.start.pinned", oid(pinned)));
            out.push(("agentfs.start.tip", oid(tip)));
        }
        Event::PushLimited {
            remote,
            limit,
            pushed,
            head,
            commits_left,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.push.limit", AnyValue::Int(*limit as i64)));
            out.push(("agentfs.push.pushed", oid(pushed)));
            out.push(("agentfs.head", oid(head)));
            out.push((
                "agentfs.push.commits_left",
                AnyValue::Int(*commits_left as i64),
            ));
        }
        Event::RemoteMisconfigured { remote, error } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.error", s(error)));
        }
        Event::RemotesUndeclared { beside } => {
            if let Some(remote) = beside {
                out.push(("agentfs.remote", s(remote)));
            }
        }
        Event::PushSucceeded {
            remote,
            head,
            previous,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.head", oid(head)));
            if let Some(p) = previous {
                out.push(("agentfs.last_pushed", oid(p)));
            }
        }
        Event::PatchApplied {
            inbox,
            patch,
            commit,
        } => {
            out.push(("agentfs.patch.inbox", AnyValue::Int(*inbox as i64)));
            out.push(("agentfs.patch.oid", oid(patch)));
            out.push(("agentfs.patch.commit", oid(commit)));
        }
        Event::PatchDuplicate {
            inbox,
            patch,
            applied_as,
        } => {
            out.push(("agentfs.patch.inbox", AnyValue::Int(*inbox as i64)));
            out.push(("agentfs.patch.oid", oid(patch)));
            out.push((
                "agentfs.patch.applied_as",
                AnyValue::Int(*applied_as as i64),
            ));
        }
        Event::PatchBlocked {
            inbox,
            patch,
            reason,
            message,
            queued,
        } => {
            out.push(("agentfs.patch.inbox", AnyValue::Int(*inbox as i64)));
            if let Some(p) = patch {
                out.push(("agentfs.patch.oid", oid(p)));
            }
            out.push(("agentfs.patch.reason", AnyValue::from(*reason)));
            out.push(("agentfs.message", s(message)));
            out.push(("agentfs.patch.queued", AnyValue::Int(*queued as i64)));
        }
        Event::PatchIgnored {
            ref_name,
            inbox,
            patch,
            why,
        } => {
            out.push(("agentfs.patch.ref", s(ref_name)));
            if let Some(n) = inbox {
                out.push(("agentfs.patch.inbox", AnyValue::Int(*n as i64)));
            }
            if let Some(p) = patch {
                out.push(("agentfs.patch.oid", oid(p)));
            }
            out.push(("agentfs.patch.why", s(why)));
        }
        Event::PatchFetchFailed {
            remote,
            kind,
            message,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.kind", AnyValue::from(kind.as_str())));
            out.push(("agentfs.message", s(message)));
        }
        Event::ForkFinished { fork, change } => {
            out.push(("agentfs.fork", s(fork)));
            if let Some(change) = change {
                out.push(("agentfs.fork.change", oid(change)));
            }
        }
        Event::ForkPublished { fork, remote } => {
            out.push(("agentfs.fork", s(fork)));
            out.push(("agentfs.remote", s(remote)));
        }
        Event::ForkRefused {
            fork,
            remote,
            reason,
            message,
        } => {
            out.push(("agentfs.fork", s(fork)));
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.fork.reason", AnyValue::from(*reason)));
            out.push(("agentfs.message", s(message)));
        }
        Event::ForkAbandoned { fork, reason } => {
            out.push(("agentfs.fork", s(fork)));
            out.push(("agentfs.fork.reason", s(reason)));
        }
        Event::HistoryServiceUnavailable { address, error } => {
            out.push(("agentfs.history_service", s(address)));
            out.push(("agentfs.error", s(error)));
        }
        Event::HistoryServiceRestored { address } => {
            out.push(("agentfs.history_service", s(address)));
        }
        Event::IdentityFallback { replica, error } => {
            out.push(("agentfs.replica", s(replica)));
            out.push(("agentfs.error", s(error)));
        }
    }
    out
}

impl<L: Logger + Send + Sync> AlarmLog for OtelAlarmLog<L> {
    fn log(&self, event: &Event) {
        let (severity, text) = match event {
            Event::PathRefused { .. } => (Severity::Warn, "WARN"),
            Event::PushSucceeded { .. }
            | Event::PatchApplied { .. }
            | Event::PatchDuplicate { .. }
            | Event::ForkFinished { .. }
            | Event::ForkPublished { .. }
            | Event::ForkAbandoned { .. }
            | Event::HistoryServiceRestored { .. } => (Severity::Info, "INFO"),
            Event::PatchFetchFailed { .. }
            | Event::PatchIgnored { .. }
            | Event::IdentityFallback { .. }
            | Event::PushLimited { .. } => (Severity::Warn, "WARN"),
            _ => (Severity::Error, "ERROR"),
        };
        let name = event_name(event);
        if !self.0.event_enabled(severity, "agentfs", Some(name)) {
            return;
        }
        let mut record = self.0.create_log_record();
        let now = SystemTime::now();
        record.set_event_name(name);
        record.set_timestamp(now);
        record.set_observed_timestamp(now);
        record.set_severity_number(severity);
        record.set_severity_text(text);
        record.set_body(AnyValue::from(event.to_string()));
        record.add_attributes(log_attributes(event));
        self.0.emit(record);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A [`Telemetry`] over the SDK's in-memory exporters, for tests.

    use opentelemetry::logs::LoggerProvider as _;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLogRecord, SdkLoggerProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

    use super::*;

    pub(crate) struct Harness {
        pub(crate) telemetry: Telemetry,
        meters: SdkMeterProvider,
        metrics: InMemoryMetricExporter,
        tracers: SdkTracerProvider,
        spans: InMemorySpanExporter,
        loggers: SdkLoggerProvider,
        logs: InMemoryLogExporter,
    }

    /// One metric data point, flattened.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Point {
        U64(u64),
        F64(f64),
        I64(i64),
        /// A histogram's count.
        Count(u64),
    }

    impl Harness {
        pub(crate) fn new() -> Self {
            let metrics = InMemoryMetricExporter::default();
            let meters = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(metrics.clone()).build())
                .build();
            let spans = InMemorySpanExporter::default();
            let tracers = SdkTracerProvider::builder()
                .with_simple_exporter(spans.clone())
                .build();
            let logs = InMemoryLogExporter::default();
            let loggers = SdkLoggerProvider::builder()
                .with_simple_exporter(logs.clone())
                .build();
            let tracer = {
                use opentelemetry::global::ObjectSafeTracerProvider as _;
                BoxedTracer::new(tracers.boxed_tracer(scope()))
            };
            let telemetry = Telemetry::new(
                &meters.meter_with_scope(scope()),
                Some(tracer),
                Some(Box::new(OtelAlarmLog::new(
                    loggers.logger_with_scope(scope()),
                ))),
            );
            Self {
                telemetry,
                meters,
                metrics,
                tracers,
                spans,
                loggers,
                logs,
            }
        }

        /// Collects metrics now and returns the export (cumulative), or
        /// `None` if there was nothing to export.
        pub(crate) fn collect(&self) -> Option<ResourceMetrics> {
            self.metrics.reset();
            self.meters.force_flush().unwrap();
            self.metrics.get_finished_metrics().unwrap().pop()
        }

        /// The data point of metric `name` whose attributes are exactly
        /// `attrs`, from a fresh collection.
        pub(crate) fn point(&self, name: &str, attrs: &[(&str, &str)]) -> Option<Point> {
            let collected = self.collect()?;
            find_point(&collected, name, attrs)
        }

        pub(crate) fn sum(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64> {
            match self.point(name, attrs)? {
                Point::U64(v) => Some(v),
                other => panic!("{name} is {other:?}"),
            }
        }

        pub(crate) fn gauge_u64(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64> {
            self.sum(name, attrs)
        }

        pub(crate) fn gauge_f64(&self, name: &str, attrs: &[(&str, &str)]) -> Option<f64> {
            match self.point(name, attrs)? {
                Point::F64(v) => Some(v),
                other => panic!("{name} is {other:?}"),
            }
        }

        pub(crate) fn histogram_count(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64> {
            match self.point(name, attrs)? {
                Point::Count(v) => Some(v),
                other => panic!("{name} is {other:?}"),
            }
        }

        /// Every finished span called `name`, in the order they ended.
        pub(crate) fn spans(&self, name: &str) -> Vec<SpanData> {
            self.tracers.force_flush().unwrap();
            self.spans
                .get_finished_spans()
                .unwrap()
                .into_iter()
                .filter(|s| s.name == name)
                .collect()
        }

        /// Every log record emitted so far.
        pub(crate) fn logs(&self) -> Vec<SdkLogRecord> {
            self.loggers.force_flush().unwrap();
            self.logs
                .get_emitted_logs()
                .unwrap()
                .into_iter()
                .map(|l| l.record)
                .collect()
        }
    }

    fn attrs_match<'a>(got: impl Iterator<Item = &'a KeyValue>, want: &[(&str, &str)]) -> bool {
        let mut got: Vec<(String, String)> = got
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        got.sort();
        let mut want: Vec<(String, String)> = want
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        want.sort();
        got == want
    }

    fn find_point(
        collected: &ResourceMetrics,
        name: &str,
        attrs: &[(&str, &str)],
    ) -> Option<Point> {
        let metric = collected
            .scope_metrics()
            .flat_map(|s| s.metrics())
            .find(|m| m.name() == name)?;
        macro_rules! scan {
            ($data:expr, $wrap:path) => {
                match $data {
                    MetricData::Sum(sum) => sum
                        .data_points()
                        .find(|p| attrs_match(p.attributes(), attrs))
                        .map(|p| $wrap(p.value())),
                    MetricData::Gauge(gauge) => gauge
                        .data_points()
                        .find(|p| attrs_match(p.attributes(), attrs))
                        .map(|p| $wrap(p.value())),
                    MetricData::Histogram(h) => h
                        .data_points()
                        .find(|p| attrs_match(p.attributes(), attrs))
                        .map(|p| Point::Count(p.count())),
                    MetricData::ExponentialHistogram(_) => None,
                }
            };
        }
        match metric.data() {
            AggregatedMetrics::U64(data) => scan!(data, Point::U64),
            AggregatedMetrics::F64(data) => scan!(data, Point::F64),
            AggregatedMetrics::I64(data) => scan!(data, Point::I64),
        }
    }

    /// Span attribute `key`, as a string.
    pub(crate) fn span_attr(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    }

    /// Log attribute `key`.
    pub(crate) fn log_attr(record: &SdkLogRecord, key: &str) -> Option<AnyValue> {
        record
            .attributes_iter()
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v.clone())
    }

    /// The string inside an [`AnyValue::String`].
    pub(crate) fn any_str(value: &AnyValue) -> String {
        match value {
            AnyValue::String(s) => s.to_string(),
            other => format!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use opentelemetry::trace::Status;

    use super::testing::{Harness, any_str, log_attr};
    use super::*;
    use crate::checks::Refusal;
    use crate::events::{CheckStage, MemorySink, PushFailureKind, PushStage};
    use crate::paths::RepoPath;

    fn alarms() -> Vec<Event> {
        let failed = |remote: &str, kind, stage| Event::PushFailed {
            remote: remote.into(),
            kind,
            message: "nope".into(),
            head: Some(git2::Oid::ZERO_SHA1),
            last_pushed: None,
            consecutive_failures: 1,
            retry_in: Duration::from_secs(60),
            stage,
        };
        vec![
            Event::PathRefused {
                path: "big".into(),
                reason: Refusal::BlobTooLarge {
                    bytes: 10,
                    limit: 5,
                    remote: "r".into(),
                },
                stage: CheckStage::Record,
            },
            Event::PathRefused {
                path: "link".into(),
                reason: Refusal::SymlinkIntoGitDir {
                    target: ".git/x".into(),
                },
                stage: CheckStage::Commit,
            },
            Event::RecordFailed {
                trigger: Trigger::Close,
                paths: vec![RepoPath::new("a").unwrap(), RepoPath::new("b").unwrap()],
                error: "disk full".into(),
            },
            Event::CommitFailed {
                error: "locked".into(),
                backlog: 3,
            },
            Event::RepackFailed {
                error: "git: not found".into(),
            },
            failed("origin", PushFailureKind::Auth, PushStage::Git),
            failed("origin", PushFailureKind::Auth, PushStage::Git),
            failed("backup", PushFailureKind::Unreachable, PushStage::Git),
            failed("origin", PushFailureKind::Auth, PushStage::Lfs),
            Event::PushSucceeded {
                remote: "backup".into(),
                head: git2::Oid::ZERO_SHA1,
                previous: None,
            },
        ]
    }

    #[test]
    fn sink_counts_alarms_by_label() {
        let h = Harness::new();
        let memory = Arc::new(MemorySink::new());
        let sink = h.telemetry.sink(memory.clone());
        for event in alarms() {
            sink.emit(event);
        }
        let rejected = names::PRECHECK_REJECTED_PATHS;
        assert_eq!(
            h.sum(
                rejected,
                &[("stage", "record"), ("reason", "blob_too_large")]
            ),
            Some(1)
        );
        assert_eq!(
            h.sum(
                rejected,
                &[("stage", "commit"), ("reason", "symlink_into_git_dir")]
            ),
            Some(1)
        );
        assert_eq!(
            h.sum(names::RECORD_FAILURES, &[("trigger", "close")]),
            Some(1)
        );
        assert_eq!(h.sum(names::COMMIT_FAILURES, &[]), Some(1));
        assert_eq!(h.sum(names::REPACK_FAILURES, &[]), Some(1));
        assert_eq!(
            h.sum(
                names::PUSH_FAILURES,
                &[("remote", "origin"), ("reason", "auth"), ("stage", "git")]
            ),
            Some(2)
        );
        assert_eq!(
            h.sum(
                names::PUSH_FAILURES,
                &[("remote", "origin"), ("reason", "auth"), ("stage", "lfs")]
            ),
            Some(1)
        );
        assert_eq!(
            h.sum(
                names::PUSH_FAILURES,
                &[
                    ("remote", "backup"),
                    ("reason", "unreachable"),
                    ("stage", "git")
                ]
            ),
            Some(1)
        );
        assert_eq!(memory.take(), alarms(), "every event is forwarded in order");
    }

    #[test]
    fn sink_logs_each_alarm() {
        let h = Harness::new();
        let sink = h.telemetry.sink(Arc::new(MemorySink::new()));
        for event in alarms() {
            sink.emit(event);
        }
        let logs = h.logs();
        let expected: Vec<Event> = alarms().into_iter().filter(Event::is_alarm).collect();
        assert_eq!(logs.len(), 9, "no record for the successful push");
        for (record, event) in logs.iter().zip(&expected) {
            assert_eq!(
                record.body().map(any_str),
                Some(event.to_string()),
                "body of {}",
                event.name()
            );
            assert_eq!(record.event_name(), Some(event_name(event)));
        }

        let push = logs
            .iter()
            .find(|r| r.event_name() == Some("agentfs.push_failed"))
            .unwrap();
        assert_eq!(push.severity_number(), Some(Severity::Error));
        assert_eq!(push.severity_text(), Some("ERROR"));
        assert_eq!(
            log_attr(push, "agentfs.remote").map(|v| any_str(&v)),
            Some("origin".into())
        );
        assert_eq!(
            log_attr(push, "agentfs.kind").map(|v| any_str(&v)),
            Some("auth".into())
        );
        assert_eq!(
            log_attr(push, "agentfs.head").map(|v| any_str(&v)),
            Some(git2::Oid::ZERO_SHA1.to_string())
        );
        assert!(log_attr(push, "agentfs.last_pushed").is_none());
        assert_eq!(
            log_attr(push, "agentfs.push.stage").map(|v| any_str(&v)),
            Some("git".into())
        );
        let lfs = logs
            .iter()
            .rfind(|r| r.event_name() == Some("agentfs.push_failed"))
            .unwrap();
        assert_eq!(
            log_attr(lfs, "agentfs.push.stage").map(|v| any_str(&v)),
            Some("lfs".into())
        );
        assert!(
            lfs.body()
                .map(any_str)
                .unwrap()
                .contains("failed (auth, stage lfs)"),
            "{:?}",
            lfs.body()
        );

        let refused = &logs[0];
        assert_eq!(refused.event_name(), Some("agentfs.path_refused"));
        assert_eq!(refused.severity_number(), Some(Severity::Warn));
        assert_eq!(
            log_attr(refused, "agentfs.path").map(|v| any_str(&v)),
            Some("big".into())
        );
        assert_eq!(
            log_attr(refused, "agentfs.reason").map(|v| any_str(&v)),
            Some("blob_too_large".into())
        );
        assert_eq!(
            log_attr(refused, "agentfs.stage").map(|v| any_str(&v)),
            Some("record".into())
        );

        let failed = &logs[2];
        assert_eq!(
            log_attr(failed, "agentfs.paths").map(|v| any_str(&v)),
            Some("a, b".into())
        );
        // No span was current, so the records carry no trace context.
        assert!(logs.iter().all(|r| r.trace_context().is_none()));
    }

    #[test]
    fn noop_forwards_and_records_nothing() {
        let noop = Telemetry::noop();
        assert!(!noop.is_enabled());
        let memory = Arc::new(MemorySink::new());
        let sink = noop.sink(memory.clone());
        for event in alarms() {
            sink.emit(event);
        }
        assert_eq!(memory.take(), alarms());

        let called = AtomicBool::new(false);
        let span = noop.span(spans::RECORD, || {
            called.store(true, Ordering::Relaxed);
            vec![KeyValue::new("x", 1)]
        });
        span.set(KeyValue::new("y", 2));
        span.set_with(|| {
            called.store(true, Ordering::Relaxed);
            KeyValue::new("z", 3)
        });
        span.error("boom");
        span.end();
        assert!(
            !called.load(Ordering::Relaxed),
            "attributes are never built"
        );
        noop.record_duration(Trigger::Close, Duration::from_millis(1));
        noop.commit_made();
        assert_eq!(format!("{noop:?}"), "Telemetry(noop)");
    }

    #[test]
    fn spans_are_current_and_carry_status() {
        let h = Harness::new();
        let sink = h.telemetry.sink(Arc::new(MemorySink::new()));
        let span = h
            .telemetry
            .span(spans::COMMIT, || vec![KeyValue::new("agentfs.records", 2)]);
        span.set(KeyValue::new("agentfs.refused", 0));
        sink.emit(Event::CommitFailed {
            error: "x".into(),
            backlog: 1,
        });
        span.error("x");
        span.end();
        // A span dropped without end() still ends.
        drop(h.telemetry.span(spans::PUSH, Vec::new));

        let commit = h.spans(spans::COMMIT);
        assert_eq!(commit.len(), 1);
        assert!(matches!(commit[0].status, Status::Error { .. }));
        assert_eq!(
            testing::span_attr(&commit[0], "agentfs.records").as_deref(),
            Some("2")
        );
        assert_eq!(h.spans(spans::PUSH).len(), 1);
        let logs = h.logs();
        let trace = logs[0].trace_context().expect("trace context");
        assert_eq!(trace.trace_id, commit[0].span_context.trace_id());
        assert_eq!(trace.span_id, commit[0].span_context.span_id());
        // Ended spans are no longer current.
        assert!(!Context::current().has_active_span());
    }
}
