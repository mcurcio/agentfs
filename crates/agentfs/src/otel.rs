//! Telemetry export for the `agentfs` process (design §13).
//!
//! Export is configured with the standard `OTEL_*` environment variables.
//! A signal (traces, metrics, logs) is exported only when an OTLP endpoint
//! is set for it (`OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT`, else
//! `OTEL_EXPORTER_OTLP_ENDPOINT`); with none, or with
//! `OTEL_SDK_DISABLED=true` or `OTEL_<SIGNAL>_EXPORTER=none`, the engine gets
//! [`Telemetry::noop`] and nothing is built.
//!
//! Each signal is exported over the protocol its
//! `OTEL_EXPORTER_OTLP_<SIGNAL>_PROTOCOL`, else `OTEL_EXPORTER_OTLP_PROTOCOL`,
//! names ([`Protocol`]): `http/protobuf` (the default), `http/json`, or
//! `grpc`. gRPC runs on a one-worker tokio runtime (`agentfs-otlp`) started
//! only when a signal uses it; the SDK's batch threads drive it through
//! [`OnRuntime`]. This build speaks plain http only: TLS is not built. A
//! setting this build cannot serve turns its signal off with a warning on
//! stderr; telemetry never stops a command.
//!
//! Every exporter is wrapped in [`Watched`], so a failed export is visible
//! ([`ExportHealth`]): a rate-limited warning on stderr and the counter
//! `agentfs.telemetry.export_failures{signal}`.
//!
//! The rest of the configuration (headers, timeouts, compression, batch
//! sizes, `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_TRACES_SAMPLER`,
//! `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`) is read by the
//! OpenTelemetry SDK and exporter themselves.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use opentelemetry::global::{BoxedTracer, ObjectSafeTracerProvider as _};
use opentelemetry::logs::{LoggerProvider as _, Severity};
use opentelemetry::metrics::{Counter, Meter, MeterProvider as _};
use opentelemetry::{Key, KeyValue};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter, SdkLoggerProvider};
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{SdkMeterProvider, Temporality};
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};

use agentfs_core::identity::Context;
use agentfs_core::telemetry::{self, OtelAlarmLog, Telemetry, names};

/// How long exit waits for telemetry to flush.
pub const TELEMETRY_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// One OpenTelemetry signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    const ALL: [Signal; 3] = [Signal::Traces, Signal::Metrics, Signal::Logs];

    fn upper(self) -> &'static str {
        match self {
            Self::Traces => "TRACES",
            Self::Metrics => "METRICS",
            Self::Logs => "LOGS",
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Traces => "traces",
            Self::Metrics => "metrics",
            Self::Logs => "logs",
        })
    }
}

/// An OTLP transport (`OTEL_EXPORTER_OTLP_PROTOCOL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OTLP/gRPC, on a one-worker tokio runtime started for it.
    Grpc,
    /// OTLP/HTTP with protobuf bodies: the default.
    HttpProtobuf,
    /// OTLP/HTTP with JSON bodies.
    HttpJson,
}

impl Protocol {
    /// The protocol a `*_PROTOCOL` value names, spelled as the
    /// specification does.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "grpc" => Some(Self::Grpc),
            "http/protobuf" => Some(Self::HttpProtobuf),
            "http/json" => Some(Self::HttpJson),
            _ => None,
        }
    }

    fn otlp(self) -> opentelemetry_otlp::Protocol {
        match self {
            Self::Grpc => opentelemetry_otlp::Protocol::Grpc,
            Self::HttpProtobuf => opentelemetry_otlp::Protocol::HttpBinary,
            Self::HttpJson => opentelemetry_otlp::Protocol::HttpJson,
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Grpc => "grpc",
            Self::HttpProtobuf => "http/protobuf",
            Self::HttpJson => "http/json",
        })
    }
}

/// Whether one signal is exported, and over which protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Enabled(Protocol),
    /// Off; `warning` says why when the configuration asked for something
    /// this build cannot do.
    Disabled {
        warning: Option<String>,
    },
}

impl Decision {
    fn off() -> Self {
        Self::Disabled { warning: None }
    }

    fn warn(message: String) -> Self {
        Self::Disabled {
            warning: Some(message),
        }
    }
}

/// What to export, per signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub traces: Decision,
    pub metrics: Decision,
    pub logs: Decision,
}

impl Plan {
    pub fn get(&self, signal: Signal) -> &Decision {
        match signal {
            Signal::Traces => &self.traces,
            Signal::Metrics => &self.metrics,
            Signal::Logs => &self.logs,
        }
    }

    /// The warnings, one line each, merging signals that share one.
    pub fn warnings(&self) -> Vec<String> {
        let mut out: Vec<(String, Vec<Signal>)> = Vec::new();
        for signal in Signal::ALL {
            if let Decision::Disabled {
                warning: Some(message),
            } = self.get(signal)
            {
                match out.iter_mut().find(|(m, _)| m == message) {
                    Some((_, signals)) => signals.push(signal),
                    None => out.push((message.clone(), vec![signal])),
                }
            }
        }
        out.into_iter()
            .map(|(message, signals)| {
                let signals: Vec<String> = signals.iter().map(Signal::to_string).collect();
                format!("{message}; {} not exported", signals.join(", "))
            })
            .collect()
    }
}

/// The environment variables agentfs itself reads to decide what to export
/// ([`plan`]). docs/configuration.md lists each (checked by
/// `reference_tests`).
pub const ENV_VARS: &[&str] = &[
    "OTEL_SDK_DISABLED",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
    "OTEL_TRACES_EXPORTER",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
];

/// Variables the OpenTelemetry SDK and exporter read themselves, not
/// agentfs. Listed in docs/configuration.md for operators (checked by
/// `reference_tests`); not exhaustive (the per-signal forms of the headers
/// and timeout, and the batch settings, are documented as families).
#[cfg_attr(not(test), allow(dead_code))]
pub const SDK_ENV_VARS: &[&str] = &[
    "OTEL_SERVICE_NAME",
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_TIMEOUT",
    "OTEL_METRIC_EXPORT_INTERVAL",
    "OTEL_TRACES_SAMPLER",
    "OTEL_TRACES_SAMPLER_ARG",
];

/// Decides, from the environment as read by `env`, which signals to
/// export. Empty values count as unset, as the OpenTelemetry spec says.
/// Only [`ENV_VARS`] are read.
pub fn plan(env: &dyn Fn(&str) -> Option<String>) -> Plan {
    let get = |name: &str| {
        debug_assert!(ENV_VARS.contains(&name), "{name} is not in ENV_VARS");
        env(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let disabled = get("OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let decide = |signal: Signal| -> Decision {
        if disabled {
            return Decision::off();
        }
        let upper = signal.upper();
        let exporter_var = format!("OTEL_{upper}_EXPORTER");
        if let Some(exporters) = get(&exporter_var) {
            let exporters: Vec<String> = exporters
                .split(',')
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect();
            if !exporters.iter().any(|e| e == "otlp") {
                if exporters.iter().all(|e| e == "none") {
                    return Decision::off();
                }
                return Decision::warn(format!(
                    "{exporter_var}={} is not supported (only otlp is built)",
                    exporters.join(",")
                ));
            }
        }
        let signal_endpoint = format!("OTEL_EXPORTER_OTLP_{upper}_ENDPOINT");
        let Some(endpoint) = get(&signal_endpoint).or_else(|| get("OTEL_EXPORTER_OTLP_ENDPOINT"))
        else {
            return Decision::off();
        };
        let signal_protocol = format!("OTEL_EXPORTER_OTLP_{upper}_PROTOCOL");
        let protocol = get(&signal_protocol)
            .map(|p| (signal_protocol.clone(), p))
            .or_else(|| {
                get("OTEL_EXPORTER_OTLP_PROTOCOL")
                    .map(|p| ("OTEL_EXPORTER_OTLP_PROTOCOL".into(), p))
            });
        let protocol = match protocol {
            None => Protocol::HttpProtobuf,
            Some((var, value)) => match Protocol::parse(&value) {
                Some(protocol) => protocol,
                None => {
                    return Decision::warn(format!(
                        "{var}={value} is not an OTLP protocol (grpc, http/protobuf or \
                         http/json)"
                    ));
                }
            },
        };
        if endpoint
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        {
            return Decision::warn(format!(
                "endpoint {endpoint} needs TLS, which is not built (use http://)"
            ));
        }
        Decision::Enabled(protocol)
    };
    Plan {
        traces: decide(Signal::Traces),
        metrics: decide(Signal::Metrics),
        logs: decide(Signal::Logs),
    }
}

/// How often a signal that keeps failing is reported again on stderr.
pub const EXPORT_WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// Failed exports, made visible (design §13): each failure counts in
/// `agentfs.telemetry.export_failures{signal}` (once metrics are set up)
/// and is reported on stderr, at once for the first failure, then at most
/// once per [`EXPORT_WARNING_INTERVAL`] with the count, and once more
/// when exports succeed again. Telemetry still never stops a command.
pub struct ExportHealth {
    signals: [SignalHealth; 3],
    counter: OnceLock<Counter<u64>>,
    warn: Box<dyn Fn(&str) + Send + Sync>,
    interval: Duration,
}

#[derive(Default)]
struct SignalHealth {
    /// Failures since the last success.
    failing: AtomicU64,
    /// When the last warning for the current run of failures was printed.
    warned: Mutex<Option<Instant>>,
}

impl fmt::Debug for ExportHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportHealth").finish_non_exhaustive()
    }
}

impl ExportHealth {
    /// Reports to stderr.
    pub fn new() -> Arc<Self> {
        Self::with_warn(
            Box::new(|line| eprintln!("{line}")),
            EXPORT_WARNING_INTERVAL,
        )
    }

    /// Reports through `warn`, repeating at most every `interval`.
    pub fn with_warn(warn: Box<dyn Fn(&str) + Send + Sync>, interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            signals: Default::default(),
            counter: OnceLock::new(),
            warn,
            interval,
        })
    }

    /// Counts failures in `agentfs.telemetry.export_failures` from now on.
    pub fn count_with(&self, meter: &Meter) {
        let _ = self.counter.set(
            meter
                .u64_counter(names::TELEMETRY_EXPORT_FAILURES)
                .with_description("Telemetry exports that failed, by signal")
                .build(),
        );
    }

    fn slot(&self, signal: Signal) -> &SignalHealth {
        &self.signals[match signal {
            Signal::Traces => 0,
            Signal::Metrics => 1,
            Signal::Logs => 2,
        }]
    }

    /// Notes the outcome of one export of `signal`.
    pub fn observe(&self, signal: Signal, result: &OTelSdkResult) {
        let slot = self.slot(signal);
        let mut warned = slot
            .warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(()) => {
                let failed = slot.failing.swap(0, Ordering::SeqCst);
                *warned = None;
                if failed > 0 {
                    (self.warn)(&format!(
                        "telemetry: {signal} export recovered after {failed} failure(s)"
                    ));
                }
            }
            Err(e) => {
                let failed = slot.failing.fetch_add(1, Ordering::SeqCst) + 1;
                if let Some(counter) = self.counter.get() {
                    counter.add(1, &[KeyValue::new("signal", signal.to_string())]);
                }
                if warned.is_none_or(|at| at.elapsed() >= self.interval) {
                    *warned = Some(Instant::now());
                    let count = if failed == 1 {
                        String::new()
                    } else {
                        format!(" ({failed} failures in a row)")
                    };
                    (self.warn)(&format!(
                        "warning: telemetry: {signal} export failed: {e}{count}"
                    ));
                }
            }
        }
    }
}

/// An exporter whose outcomes go to an [`ExportHealth`]; every other call
/// is passed through.
#[derive(Debug)]
pub struct Watched<E> {
    inner: E,
    signal: Signal,
    health: Arc<ExportHealth>,
}

impl<E> Watched<E> {
    pub fn new(inner: E, signal: Signal, health: Arc<ExportHealth>) -> Self {
        Self {
            inner,
            signal,
            health,
        }
    }
}

impl<E: SpanExporter> SpanExporter for Watched<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.health.observe(self.signal, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: PushMetricExporter> PushMetricExporter for Watched<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        self.health.observe(self.signal, &result);
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

impl<E: LogExporter> LogExporter for Watched<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.health.observe(self.signal, &result);
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// An exporter driven to completion on a tokio runtime: the gRPC exporter
/// needs one, and the SDK's batch threads run none. Without a handle,
/// calls are passed through as they are.
#[derive(Debug)]
pub struct OnRuntime<E> {
    inner: E,
    handle: Option<tokio::runtime::Handle>,
}

impl<E> OnRuntime<E> {
    fn new(inner: E, handle: Option<tokio::runtime::Handle>) -> Self {
        Self { inner, handle }
    }

    /// Runs `future` on the runtime, or here without one. The SDK calls
    /// exporters from its own threads, never from inside a runtime.
    async fn run<F: Future<Output = OTelSdkResult>>(&self, future: F) -> OTelSdkResult {
        match &self.handle {
            Some(handle) => handle.block_on(future),
            None => future.await,
        }
    }
}

impl<E: SpanExporter> SpanExporter for OnRuntime<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.run(self.inner.export(batch)).await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: PushMetricExporter> PushMetricExporter for OnRuntime<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        self.run(self.inner.export(metrics)).await
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

impl<E: LogExporter> LogExporter for OnRuntime<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        self.run(self.inner.export(batch)).await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.inner.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// The process's telemetry: the handle given to the engine, and the SDK
/// providers behind it. Flushes and shuts down when dropped, waiting at
/// most [`TELEMETRY_SHUTDOWN_GRACE`].
pub struct Otel {
    telemetry: Telemetry,
    providers: Option<Providers>,
}

struct Providers {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
    /// The gRPC exporters' runtime, stopped after them.
    runtime: Option<tokio::runtime::Runtime>,
}

impl Otel {
    /// No telemetry at all.
    pub fn noop() -> Self {
        Self {
            telemetry: Telemetry::noop(),
            providers: None,
        }
    }

    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    /// Flushes and shuts down every provider, waiting at most `grace`.
    /// Failures are printed as warnings; a stuck exporter is left behind on
    /// its thread.
    pub fn shutdown(&mut self, grace: Duration) {
        let Some(providers) = self.providers.take() else {
            return;
        };
        // Callbacks and spans hold provider references; drop ours first.
        self.telemetry = Telemetry::noop();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("agentfs-otel-shutdown".into())
            .spawn(move || {
                let mut errors = Vec::new();
                // Metrics last, so they see everything recorded.
                if let Some(tracer) = providers.tracer
                    && let Err(e) = tracer.shutdown()
                {
                    errors.push(format!("traces: {e}"));
                }
                if let Some(logger) = providers.logger
                    && let Err(e) = logger.shutdown()
                {
                    errors.push(format!("logs: {e}"));
                }
                if let Some(meter) = providers.meter
                    && let Err(e) = meter.shutdown()
                {
                    errors.push(format!("metrics: {e}"));
                }
                if let Some(runtime) = providers.runtime {
                    // A DNS lookup still blocking is left behind.
                    runtime.shutdown_background();
                }
                let _ = tx.send(errors);
            });
        if let Err(e) = spawned {
            eprintln!("warning: telemetry: not flushed: {e}");
            return;
        }
        match rx.recv_timeout(grace) {
            Ok(errors) => {
                for error in errors {
                    eprintln!("warning: telemetry: flushing {error}");
                }
            }
            Err(_) => {
                eprintln!("warning: telemetry: still flushing after {grace:?}; exiting without it")
            }
        }
    }
}

impl Drop for Otel {
    fn drop(&mut self) {
        self.shutdown(TELEMETRY_SHUTDOWN_GRACE);
    }
}

/// Sets up export for `volume` from the process environment.
pub fn init(volume: &str, ctx: &Context) -> Otel {
    let plan = plan(&|name| std::env::var(name).ok());
    for warning in plan.warnings() {
        eprintln!("warning: telemetry: {warning}");
    }
    let protocol = |s: Signal| match plan.get(s) {
        Decision::Enabled(protocol) => Some(*protocol),
        Decision::Disabled { .. } => None,
    };
    if Signal::ALL.iter().all(|s| protocol(*s).is_none()) {
        return Otel::noop();
    }
    let warn = |signal: Signal, e: &dyn fmt::Display| {
        eprintln!("warning: telemetry: {signal} exporter: {e}; {signal} not exported");
    };
    // gRPC needs a runtime: one worker, started only when a signal uses it.
    let mut runtime = None;
    if Signal::ALL
        .iter()
        .any(|s| protocol(*s) == Some(Protocol::Grpc))
    {
        match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("agentfs-otlp")
            .enable_all()
            .build()
        {
            Ok(rt) => runtime = Some(rt),
            Err(e) => eprintln!("warning: telemetry: no runtime for gRPC export: {e}"),
        }
    }
    let resource = resource(volume, ctx);
    let health = ExportHealth::new();
    // The handle a signal's exporter runs on, or `Err` when it needs the
    // runtime that could not be started.
    let handle = |signal: Signal| -> Result<Option<tokio::runtime::Handle>, ()> {
        match (protocol(signal), &runtime) {
            (Some(Protocol::Grpc), Some(rt)) => Ok(Some(rt.handle().clone())),
            (Some(Protocol::Grpc), None) => Err(()),
            _ => Ok(None),
        }
    };
    // tonic's channel is created inside the runtime.
    let _entered = runtime.as_ref().map(tokio::runtime::Runtime::enter);

    let tracer = protocol(Signal::Traces).and_then(|p| {
        let Ok(handle) = handle(Signal::Traces) else {
            return None;
        };
        let built = match p {
            Protocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .build(),
            _ => opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(p.otlp())
                .build(),
        };
        match built {
            Ok(exporter) => Some(
                SdkTracerProvider::builder()
                    .with_batch_exporter(Watched::new(
                        OnRuntime::new(exporter, handle),
                        Signal::Traces,
                        health.clone(),
                    ))
                    .with_resource(resource.clone())
                    .build(),
            ),
            Err(e) => {
                warn(Signal::Traces, &e);
                None
            }
        }
    });
    let meter = protocol(Signal::Metrics).and_then(|p| {
        let Ok(handle) = handle(Signal::Metrics) else {
            return None;
        };
        let built = match p {
            Protocol::Grpc => opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .build(),
            _ => opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .with_protocol(p.otlp())
                .build(),
        };
        match built {
            Ok(exporter) => Some(
                SdkMeterProvider::builder()
                    .with_periodic_exporter(Watched::new(
                        OnRuntime::new(exporter, handle),
                        Signal::Metrics,
                        health.clone(),
                    ))
                    .with_resource(resource.clone())
                    .build(),
            ),
            Err(e) => {
                warn(Signal::Metrics, &e);
                None
            }
        }
    });
    let logger = protocol(Signal::Logs).and_then(|p| {
        let Ok(handle) = handle(Signal::Logs) else {
            return None;
        };
        let built = match p {
            Protocol::Grpc => opentelemetry_otlp::LogExporter::builder()
                .with_tonic()
                .build(),
            _ => opentelemetry_otlp::LogExporter::builder()
                .with_http()
                .with_protocol(p.otlp())
                .build(),
        };
        match built {
            Ok(exporter) => Some(
                SdkLoggerProvider::builder()
                    .with_batch_exporter(Watched::new(
                        OnRuntime::new(exporter, handle),
                        Signal::Logs,
                        health.clone(),
                    ))
                    .with_resource(resource.clone())
                    .build(),
            ),
            Err(e) => {
                warn(Signal::Logs, &e);
                None
            }
        }
    });
    drop(_entered);
    if tracer.is_none() && meter.is_none() && logger.is_none() {
        return Otel::noop();
    }

    // With metrics off the engine still gets a meter, from the API's no-op
    // provider, so the handle is uniform.
    let meter_handle = match &meter {
        Some(provider) => provider.meter_with_scope(telemetry::scope()),
        None => {
            opentelemetry::metrics::NoopMeterProvider::new().meter_with_scope(telemetry::scope())
        }
    };
    if meter.is_some() {
        health.count_with(&meter_handle);
    }
    let tracer_handle = tracer
        .as_ref()
        .map(|p| BoxedTracer::new(p.boxed_tracer(telemetry::scope())));
    let alarms = logger.as_ref().map(|p| {
        Box::new(OtelAlarmLog::new(p.logger_with_scope(telemetry::scope())))
            as Box<dyn telemetry::AlarmLog>
    });
    Otel {
        telemetry: Telemetry::new(&meter_handle, tracer_handle, alarms),
        providers: Some(Providers {
            tracer,
            meter,
            logger,
            runtime,
        }),
    }
}

/// The resource every signal carries. `OTEL_SERVICE_NAME` and
/// `OTEL_RESOURCE_ATTRIBUTES` win over what agentfs sets.
fn resource(volume: &str, ctx: &Context) -> Resource {
    let detected = Resource::builder().build();
    let service_name = Key::from_static_str("service.name");
    let named = detected
        .get(&service_name)
        .is_some_and(|name| !name.as_str().starts_with("unknown_service"));
    let mut ours = vec![
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("service.instance.id", ctx.replica.clone()),
        KeyValue::new("process.pid", i64::from(std::process::id())),
        KeyValue::new("agentfs.volume", volume.to_owned()),
    ];
    for (key, value) in [
        ("k8s.namespace.name", &ctx.namespace),
        ("k8s.pod.name", &ctx.pod),
        ("k8s.pod.uid", &ctx.pod_uid),
        ("k8s.node.name", &ctx.node),
    ] {
        if !value.is_empty() {
            ours.push(KeyValue::new(key, value.clone()));
        }
    }
    let mut builder = Resource::builder();
    if !named {
        builder = builder.with_service_name("agentfs");
    }
    builder
        .with_attributes(
            ours.into_iter()
                .filter(|kv| detected.get(&kv.key).is_none()),
        )
        .build()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn plan_of(vars: &[(&str, &str)]) -> Plan {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        plan(&|name| map.get(name).cloned())
    }

    fn all(decision: Decision) -> Plan {
        Plan {
            traces: decision.clone(),
            metrics: decision.clone(),
            logs: decision,
        }
    }

    const ENDPOINT: (&str, &str) = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
    const HTTP: Protocol = Protocol::HttpProtobuf;

    /// [`plan`] reads exactly [`ENV_VARS`]: with every endpoint and
    /// exporter set it asks for each, and it never asks for another.
    #[test]
    fn plan_reads_only_env_vars() {
        let asked = std::cell::RefCell::new(std::collections::BTreeSet::new());
        let _ = plan(&|name| {
            asked.borrow_mut().insert(name.to_owned());
            match name {
                "OTEL_SDK_DISABLED" => Some("false".to_owned()),
                n if n.ends_with("_EXPORTER") => Some("otlp".to_owned()),
                n if n.ends_with("_PROTOCOL") => Some("http/protobuf".to_owned()),
                n if n.ends_with("_ENDPOINT") => Some("http://collector:4318".to_owned()),
                _ => None,
            }
        });
        // Without the per-signal forms, the general protocol is read.
        let _ = plan(&|name| {
            asked.borrow_mut().insert(name.to_owned());
            (name == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://collector:4318".to_owned())
        });
        let expected: std::collections::BTreeSet<String> =
            ENV_VARS.iter().map(|v| (*v).to_owned()).collect();
        assert_eq!(asked.into_inner(), expected);
        assert!(SDK_ENV_VARS.iter().all(|v| !ENV_VARS.contains(v)));
    }

    #[test]
    fn plan_reads_otel_env() {
        // Nothing set: nothing exported, nothing said.
        assert_eq!(plan_of(&[]), all(Decision::off()));
        assert!(plan_of(&[]).warnings().is_empty());
        assert_eq!(
            plan_of(&[("OTEL_EXPORTER_OTLP_ENDPOINT", " ")]),
            all(Decision::off()),
            "empty counts as unset"
        );

        // A generic endpoint enables every signal.
        assert_eq!(plan_of(&[ENDPOINT]), all(Decision::Enabled(HTTP)));
        assert_eq!(
            plan_of(&[ENDPOINT, ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")]),
            all(Decision::Enabled(HTTP))
        );

        // A signal-specific endpoint enables only that signal.
        let only = plan_of(&[("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://c/v1/metrics")]);
        assert_eq!(only.metrics, Decision::Enabled(HTTP));
        assert_eq!(only.traces, Decision::off());
        assert_eq!(only.logs, Decision::off());

        // `none` turns a signal off silently.
        let p = plan_of(&[ENDPOINT, ("OTEL_TRACES_EXPORTER", "none")]);
        assert_eq!(p.traces, Decision::off());
        assert_eq!(p.metrics, Decision::Enabled(HTTP));
        assert!(p.warnings().is_empty());

        // Other exporters are not built.
        let p = plan_of(&[ENDPOINT, ("OTEL_METRICS_EXPORTER", "prometheus")]);
        assert!(
            matches!(&p.metrics, Decision::Disabled { warning: Some(w) } if w.contains("prometheus")),
            "{p:?}"
        );
        assert_eq!(p.logs, Decision::Enabled(HTTP));
        assert_eq!(
            plan_of(&[ENDPOINT, ("OTEL_LOGS_EXPORTER", "otlp,console")]).logs,
            Decision::Enabled(HTTP)
        );

        // Each protocol the specification names is built.
        for (value, protocol) in [
            ("grpc", Protocol::Grpc),
            ("http/protobuf", Protocol::HttpProtobuf),
            ("http/json", Protocol::HttpJson),
        ] {
            let p = plan_of(&[ENDPOINT, ("OTEL_EXPORTER_OTLP_PROTOCOL", value)]);
            assert_eq!(p, all(Decision::Enabled(protocol)), "{value}");
            assert_eq!(protocol.to_string(), value);
        }
        // Anything else is not, with one warning for all signals.
        for protocol in ["http", "GRPC", "thrift"] {
            let p = plan_of(&[ENDPOINT, ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol)]);
            for signal in Signal::ALL {
                assert!(
                    matches!(p.get(signal), Decision::Disabled { warning: Some(w) } if w.contains(protocol) && w.contains("not an OTLP protocol")),
                    "{protocol}: {p:?}"
                );
            }
            let warnings = p.warnings();
            assert_eq!(warnings.len(), 1, "merged: {warnings:?}");
            assert!(
                warnings[0].ends_with("traces, metrics, logs not exported"),
                "{warnings:?}"
            );
        }

        // TLS is not built.
        let p = plan_of(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "HTTPS://collector:4318")]);
        assert!(
            matches!(&p.traces, Decision::Disabled { warning: Some(w) } if w.contains("TLS")),
            "{p:?}"
        );

        // OTEL_SDK_DISABLED wins, silently.
        let p = plan_of(&[
            ENDPOINT,
            ("OTEL_SDK_DISABLED", "TRUE"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ]);
        assert_eq!(p, all(Decision::off()));
        assert_eq!(
            plan_of(&[ENDPOINT, ("OTEL_SDK_DISABLED", "false")]),
            all(Decision::Enabled(HTTP))
        );

        // A signal-specific protocol overrides the generic one.
        let p = plan_of(&[
            ENDPOINT,
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/protobuf"),
        ]);
        assert_eq!(p.traces, Decision::Enabled(HTTP));
        assert_eq!(p.metrics, Decision::Enabled(Protocol::Grpc));
        let p = plan_of(&[ENDPOINT, ("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/json")]);
        assert_eq!(p.logs, Decision::Enabled(Protocol::HttpJson));
        assert_eq!(p.traces, Decision::Enabled(HTTP));
        let p = plan_of(&[ENDPOINT, ("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "bogus")]);
        assert!(matches!(p.logs, Decision::Disabled { warning: Some(_) }));
        assert_eq!(p.traces, Decision::Enabled(HTTP));

        // A signal-specific endpoint is checked, not the generic one.
        let p = plan_of(&[
            ENDPOINT,
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://secure/v1/traces",
            ),
        ]);
        assert!(matches!(p.traces, Decision::Disabled { warning: Some(_) }));
        assert_eq!(p.metrics, Decision::Enabled(HTTP));
    }

    #[test]
    fn noop_shutdown_is_instant() {
        let mut otel = Otel::noop();
        assert!(!otel.telemetry().is_enabled());
        let start = std::time::Instant::now();
        otel.shutdown(Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn resource_names_the_service_and_volume() {
        let ctx = Context {
            volume: "demo".into(),
            replica: "pod-0".into(),
            pod: "pod-0".into(),
            namespace: "ns".into(),
            ..Context::default()
        };
        let r = resource("demo", &ctx);
        let get = |k: &'static str| r.get(&Key::from_static_str(k)).map(|v| v.to_string());
        // The test runner does not set OTEL_SERVICE_NAME.
        if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
            assert_eq!(get("service.name").as_deref(), Some("agentfs"));
        }
        assert_eq!(get("agentfs.volume").as_deref(), Some("demo"));
        assert_eq!(get("service.instance.id").as_deref(), Some("pod-0"));
        assert_eq!(get("k8s.namespace.name").as_deref(), Some("ns"));
        assert_eq!(get("k8s.node.name"), None, "empty values are left out");
    }

    #[test]
    fn export_failures_warn_once_then_rate_limited_then_recover() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let health = ExportHealth::with_warn(
            Box::new(move |line| sink.lock().unwrap().push(line.to_owned())),
            Duration::from_millis(200),
        );
        let failed = || -> OTelSdkResult {
            Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                "HTTP 500".into(),
            ))
        };
        let take = || std::mem::take(&mut *lines.lock().unwrap());

        health.observe(Signal::Traces, &Ok(()));
        assert!(take().is_empty(), "success says nothing");
        health.observe(Signal::Traces, &failed());
        let first = take();
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            first[0].starts_with("warning: telemetry: traces export failed: ")
                && first[0].contains("HTTP 500"),
            "{first:?}"
        );
        for _ in 0..5 {
            health.observe(Signal::Traces, &failed());
        }
        assert!(take().is_empty(), "rate limited");
        // Another signal is reported on its own.
        health.observe(Signal::Logs, &failed());
        assert_eq!(take().len(), 1);
        std::thread::sleep(Duration::from_millis(250));
        health.observe(Signal::Traces, &failed());
        let again = take();
        assert!(
            again.len() == 1 && again[0].contains("(7 failures in a row)"),
            "{again:?}"
        );
        health.observe(Signal::Traces, &Ok(()));
        assert_eq!(
            take(),
            ["telemetry: traces export recovered after 7 failure(s)"]
        );
        // A new run of failures warns at once.
        health.observe(Signal::Traces, &failed());
        assert_eq!(take().len(), 1);
    }
}
