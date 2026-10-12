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
//!   [`EventSink`]: each event updates its counter and, for alarms, becomes
//!   a structured log record, then goes on to the next sink. The caller
//!   wraps once per process; the engine never wraps, so nothing is counted
//!   twice.
//! - **Direct instrumentation.** The recorder, committer and pushers call
//!   the handle for `agentfs.record.duration`, `agentfs.commit.count` and
//!   the `agentfs.record`, `agentfs.commit` and `agentfs.push` spans. Each
//!   span is current while its operation runs, so alarm logs raised inside
//!   it carry its trace context.
//! - **Observable gauges**, read from state when metrics are collected:
//!   the journal backlog, each remote's push backlog, age and divergence,
//!   misconfigured remotes and the size of the state area.
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
    pub const PUSH_UNPUSHED_COMMITS: &str = "agentfs.push.unpushed_commits";
    pub const PUSH_OLDEST_UNPUSHED_AGE: &str = "agentfs.push.oldest_unpushed_age";
    pub const PUSH_FAILURES: &str = "agentfs.push.failures";
    pub const PUSH_DIVERGED: &str = "agentfs.push.diverged";
    pub const PRECHECK_REJECTED_PATHS: &str = "agentfs.precheck.rejected_paths";
    pub const STATE_BYTES: &str = "agentfs.state.bytes";
    pub const REMOTE_MISCONFIGURED: &str = "agentfs.remote.misconfigured";
}

/// Span names (design §13).
pub mod spans {
    pub const RECORD: &str = "agentfs.record";
    pub const COMMIT: &str = "agentfs.commit";
    pub const PUSH: &str = "agentfs.push";
}

/// Bucket boundaries of `agentfs.record.duration`, in seconds: the
/// synchronous step is usually well under a millisecond, but a slow disk
/// or a large copy can take seconds.
const RECORD_DURATION_BOUNDARIES: [f64; 16] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
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
    push_failures: Counter<u64>,
    rejected_paths: Counter<u64>,
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
            push_failures: counter(
                names::PUSH_FAILURES,
                "{failure}",
                "Failed push attempts, by remote and reason",
            ),
            rejected_paths: counter(
                names::PRECHECK_REJECTED_PATHS,
                "{path}",
                "Paths refused by the pre-commit checks, by stage and reason",
            ),
            meter: meter.clone(),
            tracer,
            alarms,
        };
        Self {
            inner: Some(Arc::new(inner)),
        }
    }

    /// True unless this is [`Telemetry::noop`].
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Wraps `next`: each event updates its counter and, if it is an alarm,
    /// becomes a log record; then it goes on to `next`. Never blocks.
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
            Event::PushFailed { remote, kind, .. } => inner.push_failures.add(
                1,
                &[remote_attr(remote), KeyValue::new("reason", kind.as_str())],
            ),
            // Their gauges are read from state.
            Event::Diverged { .. }
            | Event::RemoteMisconfigured { .. }
            | Event::PushSucceeded { .. } => {}
        }
        if event.is_alarm()
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
        Event::PushFailed { .. } => "agentfs.push_failed",
        Event::Diverged { .. } => "agentfs.diverged",
        Event::RemoteMisconfigured { .. } => "agentfs.remote_misconfigured",
        Event::PushSucceeded { .. } => "agentfs.push_succeeded",
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
        Event::PushFailed {
            remote,
            kind,
            message,
            head,
            last_pushed,
            consecutive_failures,
            retry_in,
        } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.kind", AnyValue::from(kind.as_str())));
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
        Event::RemoteMisconfigured { remote, error } => {
            out.push(("agentfs.remote", s(remote)));
            out.push(("agentfs.error", s(error)));
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
    }
    out
}

impl<L: Logger + Send + Sync> AlarmLog for OtelAlarmLog<L> {
    fn log(&self, event: &Event) {
        let (severity, text) = match event {
            Event::PathRefused { .. } => (Severity::Warn, "WARN"),
            Event::PushSucceeded { .. } => (Severity::Info, "INFO"),
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
    use crate::events::{CheckStage, MemorySink, PushFailureKind};
    use crate::paths::RepoPath;

    fn alarms() -> Vec<Event> {
        let failed = |remote: &str, kind| Event::PushFailed {
            remote: remote.into(),
            kind,
            message: "nope".into(),
            head: Some(git2::Oid::ZERO_SHA1),
            last_pushed: None,
            consecutive_failures: 1,
            retry_in: Duration::from_secs(60),
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
            failed("origin", PushFailureKind::Auth),
            failed("origin", PushFailureKind::Auth),
            failed("backup", PushFailureKind::Unreachable),
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
        assert_eq!(
            h.sum(
                names::PUSH_FAILURES,
                &[("remote", "origin"), ("reason", "auth")]
            ),
            Some(2)
        );
        assert_eq!(
            h.sum(
                names::PUSH_FAILURES,
                &[("remote", "backup"), ("reason", "unreachable")]
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
        assert_eq!(logs.len(), 7, "no record for the successful push");
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
