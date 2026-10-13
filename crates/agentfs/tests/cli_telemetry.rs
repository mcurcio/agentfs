//! End-to-end tests of telemetry export (design §13): the `agentfs`
//! binary against a fake OTLP/HTTP collector.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use prost::Message;

/// `(path, body)` of each request received.
type Requests = Mutex<Vec<(String, Vec<u8>)>>;

/// Accepts OTLP/HTTP requests on a local port, answers 200 and keeps
/// `(path, body)` of each.
struct Collector {
    port: u16,
    /// Requests to this path are answered 500 (and still kept).
    failing: Option<&'static str>,
    requests: Arc<Requests>,
    connections: Arc<AtomicUsize>,
}

impl Collector {
    fn start() -> Self {
        Self::start_failing(None)
    }

    fn start_failing(failing: Option<&'static str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        {
            let requests = requests.clone();
            let connections = connections.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    connections.fetch_add(1, Ordering::SeqCst);
                    let requests = requests.clone();
                    thread::spawn(move || serve(stream, &requests, failing));
                }
            });
        }
        Self {
            port,
            failing,
            requests,
            connections,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn bodies(&self, path: &str) -> Vec<Vec<u8>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, b)| b.clone())
            .collect()
    }

    fn paths(&self) -> BTreeSet<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }
}

/// Serves one HTTP/1.1 request, then closes the connection.
fn serve(stream: TcpStream, requests: &Requests, failing: Option<&str>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let status = if failing == Some(path.as_str()) {
        "500 Internal Server Error"
    } else {
        "200 OK"
    };
    requests.lock().unwrap().push((path, body));
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/x-protobuf\r\n\
          content-length: 0\r\nconnection: close\r\n\r\n"
        )
        .as_bytes(),
    );
}

struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(v.tree()).unwrap();
        fs::create_dir_all(v.dir.path().join("home")).unwrap();
        let out = v.run(&["init", "--volume", "demo"], &[]);
        assert!(out.status.success(), "{}", stderr(&out));
        v
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    /// Runs `agentfs` with only the given `OTEL_*` variables.
    fn run(&self, args: &[&str], otel: &[(&str, &str)]) -> Output {
        let home = self.dir.path().join("home");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.dir.path().join("state"))
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env("AGENTFS_REPLICA", "test-0")
            .env("AGENTFS_NAMESPACE", "agents")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .envs(otel.iter().copied())
            .output()
            .unwrap()
    }

    /// Records a plain file and a symlinked `.gitmodules` (refused).
    fn record(&self, otel: &[(&str, &str)]) -> Output {
        fs::write(self.tree().join("a.txt"), "hello").unwrap();
        let gitmodules = self.tree().join(".gitmodules");
        if fs::symlink_metadata(&gitmodules).is_err() {
            std::os::unix::fs::symlink("elsewhere", gitmodules).unwrap();
        }
        let out = self.run(&["record", "a.txt", ".gitmodules"], otel);
        assert!(out.status.success(), "{}", stderr(&out));
        out
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn string_attr(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match &v.value {
            Some(any_value::Value::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
}

#[test]
fn exports_over_otlp_http() {
    let collector = Collector::start();
    let v = Volume::new();
    let endpoint = collector.endpoint();
    let out = v.record(&[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)]);
    let err = stderr(&out);
    assert!(!err.contains("warning: telemetry"), "{err}");
    assert!(
        err.contains("alarm: path refused"),
        "still on stderr: {err}"
    );

    let paths = collector.paths();
    for path in ["/v1/metrics", "/v1/traces", "/v1/logs"] {
        assert!(paths.contains(path), "{path} missing from {paths:?}");
    }

    let mut metric_names = BTreeSet::new();
    let mut resource_attrs = Vec::new();
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(&body[..]).unwrap();
        for rm in request.resource_metrics {
            resource_attrs.extend(rm.resource.into_iter().flat_map(|r| r.attributes));
            for sm in rm.scope_metrics {
                assert_eq!(sm.scope.as_ref().map(|s| s.name.as_str()), Some("agentfs"));
                metric_names.extend(sm.metrics.into_iter().map(|m| m.name));
            }
        }
    }
    for name in [
        "agentfs.record.duration",
        "agentfs.commit.count",
        "agentfs.precheck.rejected_paths",
        "agentfs.journal.backlog",
        "agentfs.state.bytes",
        "agentfs.patch.queue_depth",
        "agentfs.patch.blocked",
    ] {
        assert!(
            metric_names.contains(name),
            "{name} missing: {metric_names:?}"
        );
    }
    assert_eq!(
        string_attr(&resource_attrs, "service.name").as_deref(),
        Some("agentfs")
    );
    assert_eq!(
        string_attr(&resource_attrs, "agentfs.volume").as_deref(),
        Some("demo")
    );
    assert_eq!(
        string_attr(&resource_attrs, "service.instance.id").as_deref(),
        Some("test-0")
    );
    assert_eq!(
        string_attr(&resource_attrs, "k8s.namespace.name").as_deref(),
        Some("agents")
    );

    let mut span_names = BTreeSet::new();
    for body in collector.bodies("/v1/traces") {
        let request = ExportTraceServiceRequest::decode(&body[..]).unwrap();
        for rs in request.resource_spans {
            for ss in rs.scope_spans {
                span_names.extend(ss.spans.into_iter().map(|s| s.name));
            }
        }
    }
    assert!(span_names.contains("agentfs.record"), "{span_names:?}");
    assert!(span_names.contains("agentfs.commit"), "{span_names:?}");

    let mut events = Vec::new();
    for body in collector.bodies("/v1/logs") {
        let request = ExportLogsServiceRequest::decode(&body[..]).unwrap();
        for rl in request.resource_logs {
            for sl in rl.scope_logs {
                events.extend(sl.log_records);
            }
        }
    }
    let refused = events
        .iter()
        .find(|r| r.event_name == "agentfs.path_refused")
        .unwrap_or_else(|| panic!("no path_refused log: {events:?}"));
    assert_eq!(
        string_attr(&refused.attributes, "agentfs.path").as_deref(),
        Some(".gitmodules")
    );
    assert_eq!(refused.trace_id.len(), 16, "raised inside the record span");
}

#[test]
fn no_endpoint_means_no_export() {
    let collector = Collector::start();
    let v = Volume::new();
    let endpoint = collector.endpoint();
    for otel in [
        vec![],
        vec![
            ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_SDK_DISABLED", "true"),
        ],
        vec![
            ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_TRACES_EXPORTER", "none"),
            ("OTEL_METRICS_EXPORTER", "none"),
            ("OTEL_LOGS_EXPORTER", "none"),
        ],
    ] {
        let out = v.record(&otel);
        let err = stderr(&out);
        assert!(!err.contains("telemetry"), "{otel:?}: {err}");
    }
    thread::sleep(Duration::from_millis(500));
    assert_eq!(collector.connections.load(Ordering::SeqCst), 0);
}

#[test]
fn unsupported_settings_warn_and_carry_on() {
    let collector = Collector::start();
    let v = Volume::new();
    let endpoint = collector.endpoint();
    let https = format!("https://127.0.0.1:{}", collector.port);
    for (otel, expect) in [
        (
            vec![
                ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "thrift"),
            ],
            "OTEL_EXPORTER_OTLP_PROTOCOL=thrift is not an OTLP protocol",
        ),
        (
            vec![("OTEL_EXPORTER_OTLP_ENDPOINT", https.as_str())],
            "needs TLS, which is not built",
        ),
    ] {
        let out = v.record(&otel);
        let err = stderr(&out);
        assert!(err.contains("warning: telemetry: "), "{err}");
        assert!(err.contains(expect), "{expect}: {err}");
        assert_eq!(
            err.matches("warning: telemetry").count(),
            1,
            "one line for all signals: {err}"
        );
    }
    thread::sleep(Duration::from_millis(500));
    assert_eq!(collector.connections.load(Ordering::SeqCst), 0);
}

#[test]
fn push_exports_its_span() {
    let collector = Collector::start();
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    let mut config = git2::Config::open(&v.dir.path().join("state/git/config")).unwrap();
    config
        .set_str("remote.origin.url", &remote.display().to_string())
        .unwrap();
    v.record(&[]);
    let out = v.run(
        &["push"],
        &[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            &format!("{}/v1/traces", collector.endpoint()),
        )],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        collector.paths(),
        BTreeSet::from(["/v1/traces".to_owned()]),
        "only the configured signal is exported"
    );
    let mut pushes = Vec::new();
    for body in collector.bodies("/v1/traces") {
        let request = ExportTraceServiceRequest::decode(&body[..]).unwrap();
        for rs in request.resource_spans {
            for ss in rs.scope_spans {
                pushes.extend(ss.spans.into_iter().filter(|s| s.name == "agentfs.push"));
            }
        }
    }
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        string_attr(&pushes[0].attributes, "agentfs.push.outcome").as_deref(),
        Some("pushed")
    );
    assert_eq!(
        string_attr(&pushes[0].attributes, "agentfs.remote").as_deref(),
        Some("origin")
    );
}

#[test]
fn a_collector_that_is_down_never_fails_the_command() {
    // A port nothing listens on: connections are refused.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let v = Volume::new();
    let start = std::time::Instant::now();
    let out = v.record(&[(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        &format!("http://127.0.0.1:{port}"),
    )]);
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 record(s) into 1 commit(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The value of `name`'s data points with `attr` = `value`, summed over
/// every metrics request (cumulative sums report their running total, so
/// the largest is the total).
fn counter_max(collector: &Collector, name: &str, attr: (&str, &str)) -> Option<u64> {
    use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
    let mut best = None;
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(&body[..]).unwrap();
        for rm in request.resource_metrics {
            for sm in rm.scope_metrics {
                for m in sm.metrics.into_iter().filter(|m| m.name == name) {
                    let Some(metric::Data::Sum(sum)) = m.data else {
                        continue;
                    };
                    for point in sum.data_points {
                        if string_attr(&point.attributes, attr.0).as_deref() != Some(attr.1) {
                            continue;
                        }
                        if let Some(number_data_point::Value::AsInt(v)) = point.value {
                            best = best.max(Some(v as u64));
                        }
                    }
                }
            }
        }
    }
    best
}

#[test]
fn failed_exports_are_visible() {
    // Traces are refused with HTTP 500; metrics and logs are accepted.
    let collector = Collector::start_failing(Some("/v1/traces"));
    assert_eq!(collector.failing, Some("/v1/traces"));
    let v = Volume::new();
    let endpoint = collector.endpoint();
    let out = v.record(&[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)]);
    let err = stderr(&out);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 record(s) into 1 commit(s)"),
        "the command carries on"
    );
    assert_eq!(
        err.matches("warning: telemetry: traces export failed")
            .count(),
        1,
        "{err}"
    );
    assert!(!err.contains("metrics export failed"), "{err}");
    assert!(
        collector.paths().contains("/v1/traces"),
        "traces were tried"
    );
    assert_eq!(
        counter_max(
            &collector,
            "agentfs.telemetry.export_failures",
            ("signal", "traces")
        ),
        Some(1),
        "the failure is counted and exported over metrics"
    );
}

/// The last value of gauge `name`.
fn gauge_last(collector: &Collector, name: &str) -> Option<i64> {
    use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
    let mut last = None;
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(&body[..]).unwrap();
        for rm in request.resource_metrics {
            for sm in rm.scope_metrics {
                for m in sm.metrics.into_iter().filter(|m| m.name == name) {
                    let Some(metric::Data::Gauge(gauge)) = m.data else {
                        continue;
                    };
                    for point in gauge.data_points {
                        if let Some(number_data_point::Value::AsInt(v)) = point.value {
                            last = Some(v);
                        }
                    }
                }
            }
        }
    }
    last
}

/// A patch on `parent` in the volume's own repository, queued as `n`.
fn queue_patch(git: &std::path::Path, n: u64, parent: git2::Oid, path: &str, content: &str) {
    let repo = git2::Repository::open_bare(git).unwrap();
    let parent = repo.find_commit(parent).unwrap();
    let mut index = git2::Index::new().unwrap();
    index.read_tree(&parent.tree().unwrap()).unwrap();
    let blob = repo.blob(content.as_bytes()).unwrap();
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: blob,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        })
        .unwrap();
    let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
    let sig = git2::Signature::now("Ada", "ada@example.com").unwrap();
    let oid = repo
        .commit(None, &sig, &sig, &format!("patch {n}"), &tree, &[&parent])
        .unwrap();
    repo.reference(
        &format!("refs/agentfs/demo/inbox/{n:010}"),
        oid,
        true,
        "test",
    )
    .unwrap();
}

#[test]
fn a_blocked_patch_queue_exports_gauges_span_and_alarm() {
    let collector = Collector::start();
    let v = Volume::new();
    let git = v.dir.path().join("state/git");
    fs::write(v.tree().join("a.txt"), "a0\n").unwrap();
    fs::write(v.tree().join("b.txt"), "b0\n").unwrap();
    let out = v.run(&["record", "a.txt", "b.txt"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let base = git2::Repository::open_bare(&git)
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .unwrap();
    queue_patch(&git, 1, base, "a.txt", "a1\n");
    queue_patch(&git, 2, base, "b.txt", "b-from-iac\n");
    queue_patch(&git, 3, base, "c.txt", "c\n");
    fs::write(v.tree().join("b.txt"), "b-from-workload\n").unwrap();
    let out = v.run(&["record", "b.txt"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = v.run(
        &["patch", "apply", "--no-fetch"],
        &[("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.endpoint())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("alarm: "), "{}", stderr(&out));

    assert_eq!(gauge_last(&collector, "agentfs.patch.queue_depth"), Some(2));
    assert_eq!(gauge_last(&collector, "agentfs.patch.blocked"), Some(1));

    let mut patches = Vec::new();
    for body in collector.bodies("/v1/traces") {
        let request = ExportTraceServiceRequest::decode(&body[..]).unwrap();
        for rs in request.resource_spans {
            for ss in rs.scope_spans {
                patches.extend(ss.spans.into_iter().filter(|s| s.name == "agentfs.patch"));
            }
        }
    }
    let outcomes: Vec<Option<String>> = patches
        .iter()
        .map(|s| string_attr(&s.attributes, "agentfs.patch.outcome"))
        .collect();
    assert_eq!(
        outcomes,
        [Some("applied".to_owned()), Some("blocked".to_owned())]
    );
    assert_eq!(
        string_attr(&patches[1].attributes, "agentfs.patch.reason").as_deref(),
        Some("conflict")
    );

    let mut logs = Vec::new();
    for body in collector.bodies("/v1/logs") {
        let request = ExportLogsServiceRequest::decode(&body[..]).unwrap();
        for rl in request.resource_logs {
            for sl in rl.scope_logs {
                logs.extend(sl.log_records);
            }
        }
    }
    let blocked = logs
        .iter()
        .find(|r| r.event_name == "agentfs.patch_blocked")
        .unwrap_or_else(|| panic!("no patch_blocked log: {logs:?}"));
    assert_eq!(
        string_attr(&blocked.attributes, "agentfs.patch.reason").as_deref(),
        Some("conflict")
    );
    assert!(
        !logs.iter().any(|r| r.event_name == "agentfs.patch_applied"),
        "applied is not an alarm"
    );
}

/// A patch phase that cannot run at all reads as blocked on the gauge,
/// not as the previous run's state.
#[test]
fn a_queue_that_cannot_run_sets_the_blocked_gauge() {
    let collector = Collector::start();
    let v = Volume::new();
    fs::write(v.tree().join("a.txt"), "a0\n").unwrap();
    let out = v.run(&["record", "a.txt"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = v.run(&["patch", "apply", "--no-fetch"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    // The status ref cannot be written (a ref sits beneath its name; a
    // lock file the open would remove as a crash's): the run fails as a
    // whole.
    let repo = git2::Repository::open_bare(v.dir.path().join("state/git")).unwrap();
    let head = repo.refname_to_id("refs/heads/agentfs/demo").unwrap();
    if let Ok(mut r) = repo.find_reference("refs/agentfs/demo/status") {
        r.delete().unwrap();
    }
    repo.reference("refs/agentfs/demo/status/x", head, true, "block")
        .unwrap();
    let out = v.run(
        &["patch", "apply", "--no-fetch"],
        &[("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.endpoint())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(gauge_last(&collector, "agentfs.patch.blocked"), Some(1));
}

/// Every value of gauge `name`, in the order the collector got them.
fn gauge_values(collector: &Collector, name: &str) -> Vec<i64> {
    use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
    let mut out = Vec::new();
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(&body[..]).unwrap();
        for rm in request.resource_metrics {
            for sm in rm.scope_metrics {
                for m in sm.metrics.into_iter().filter(|m| m.name == name) {
                    let Some(metric::Data::Gauge(gauge)) = m.data else {
                        continue;
                    };
                    for point in gauge.data_points {
                        if let Some(number_data_point::Value::AsInt(v)) = point.value {
                            out.push(v);
                        }
                    }
                }
            }
        }
    }
    out
}

/// The largest total of counter `name` over its data points, whatever
/// their attributes (cumulative: the largest is the total).
fn counter_any(collector: &Collector, name: &str) -> Option<u64> {
    use opentelemetry_proto::tonic::metrics::v1::{metric, number_data_point};
    let mut best = None;
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(&body[..]).unwrap();
        for rm in request.resource_metrics {
            for sm in rm.scope_metrics {
                for m in sm.metrics.into_iter().filter(|m| m.name == name) {
                    let Some(metric::Data::Sum(sum)) = m.data else {
                        continue;
                    };
                    for point in sum.data_points {
                        if let Some(number_data_point::Value::AsInt(v)) = point.value {
                            best = best.max(Some(v as u64));
                        }
                    }
                }
            }
        }
    }
    best
}

/// The largest count of histogram `name`'s data points with `attr`.
fn histogram_count(collector: &Collector, name: &str, attr: (&str, &str)) -> u64 {
    use opentelemetry_proto::tonic::metrics::v1::metric;
    let mut best = 0;
    for body in collector.bodies("/v1/metrics") {
        let request = ExportMetricsServiceRequest::decode(&body[..]).unwrap();
        for rm in request.resource_metrics {
            for sm in rm.scope_metrics {
                for m in sm.metrics.into_iter().filter(|m| m.name == name) {
                    let Some(metric::Data::Histogram(h)) = m.data else {
                        continue;
                    };
                    for point in h.data_points {
                        if string_attr(&point.attributes, attr.0).as_deref() == Some(attr.1) {
                            best = best.max(point.count);
                        }
                    }
                }
            }
        }
    }
    best
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "never {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Real failures, not synthetic events: a file the caller cannot read
/// fails its record (`agentfs.record.failures`, by trigger), and a history
/// whose object directories cannot be written fails the commit
/// (`agentfs.commit.failures`) while the record stays journaled. Skipped
/// as root, which permissions do not stop.
#[test]
fn real_record_and_commit_failures_are_counted() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: permissions do not stop root");
        return;
    }
    let collector = Collector::start();
    let endpoint = collector.endpoint();
    let otel = [("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str())];
    let v = Volume::new();

    let secret = v.tree().join("secret.txt");
    fs::write(&secret, "s").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    let out = v.run(&["record", "secret.txt"], &otel);
    assert!(!out.status.success(), "an unreadable file fails its record");
    assert_eq!(
        counter_max(&collector, "agentfs.record.failures", ("trigger", "close")),
        Some(1),
        "{}",
        stderr(&out)
    );

    // Objects cannot be written: the commit fails, the record stays.
    let objects = v.dir.path().join("state/git/objects");
    let mut dirs = vec![objects.clone()];
    for entry in fs::read_dir(&objects).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    for d in &dirs {
        fs::set_permissions(d, fs::Permissions::from_mode(0o555)).unwrap();
    }
    fs::write(v.tree().join("a.txt"), "a").unwrap();
    let out = v.run(&["record", "a.txt"], &otel);
    for d in &dirs {
        fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(
        counter_any(&collector, "agentfs.commit.failures"),
        Some(1),
        "{}",
        stderr(&out)
    );
    let out = v.run(&["flush"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let status = v.run(&["status"], &[]);
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("backlog:   0 record(s)"),
        "the journaled record was committed later"
    );
}

/// The history service's `agentfs.history_service.fronts` gauge follows
/// the fronts connected (0, 1, 2, then 1), and a `record
/// --history-service` front exports its `agentfs.front.round_trip`.
#[test]
fn the_fronts_gauge_and_a_fronts_round_trip_are_exported() {
    use agentfs_core::events::NullSink;
    use agentfs_core::history_service::protocol::FrontInfo;
    use agentfs_core::history_service::{Address, ClientConfig, FrontLease, HistoryClient};
    use agentfs_core::record::Origin;
    use agentfs_core::telemetry::Telemetry;
    use agentfs_core::volume::StateDir;

    let collector = Collector::start();
    let endpoint = collector.endpoint();
    let otel = [
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "100"),
    ];
    let v = Volume::new();
    let state = StateDir::new(v.dir.path().join("state"));
    let log = v.dir.path().join("serve.log");
    let home = v.dir.path().join("home");
    let mut service = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
            service.env_remove(name);
        }
    }
    let mut service = service
        .arg("--state")
        .arg(state.root())
        .arg("--root")
        .arg(v.tree())
        .arg("serve-history")
        .env("AGENTFS_REPLICA", "service-0")
        .env("HOME", &home)
        .envs(otel.iter().copied())
        .stdout(std::process::Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    struct Kill<'a>(&'a mut std::process::Child);
    impl Drop for Kill<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let service = Kill(&mut service);
    wait_until("served", || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("serving history for volume demo")
    });
    let fronts = |want: i64| {
        let seen = gauge_values(&collector, "agentfs.history_service.fronts");
        seen.last() == Some(&want)
    };
    wait_until("exported 0 fronts", || fronts(0));

    let connect = |pod: &str| {
        let lease = FrontLease::acquire(&state, &format!("command=test pod={pod}")).unwrap();
        let client = HistoryClient::connect(
            ClientConfig {
                address: Address::Unix(state.history_socket()),
                timeout: Duration::from_secs(10),
                connect_wait: Duration::from_secs(10),
            },
            "demo",
            "refs/heads/agentfs/demo",
            FrontInfo {
                origin: Origin {
                    namespace: "agents".into(),
                    pod: pod.into(),
                    pod_uid: format!("{pod}-uid"),
                    node: "node-1".into(),
                    replica: pod.into(),
                },
                slot: lease.slot(),
                instance: u64::from(std::process::id()) << 16 | u64::from(lease.slot()),
                pid: std::process::id(),
                version: "test".into(),
                views: Vec::new(),
            },
            Arc::new(NullSink),
            Telemetry::noop(),
        )
        .unwrap();
        client.ping().unwrap();
        (lease, client)
    };
    let a = connect("pod-a");
    wait_until("exported 1 front", || fronts(1));
    let b = connect("pod-b");
    wait_until("exported 2 fronts", || fronts(2));
    drop(b);
    wait_until("exported 1 front again", || fronts(1));
    drop(a);

    fs::write(v.tree().join("f.txt"), "f").unwrap();
    let out = v.run(
        &[
            "record",
            "--history-service",
            state.history_socket().to_str().unwrap(),
            "--history-wait",
            "10s",
            "f.txt",
        ],
        &otel,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        histogram_count(
            &collector,
            "agentfs.front.round_trip",
            ("request", "record")
        ) >= 1,
        "the front's round trip was exported"
    );
    drop(service);
}

#[test]
fn exports_over_otlp_http_json() {
    let collector = Collector::start();
    let v = Volume::new();
    let endpoint = collector.endpoint();
    let out = v.record(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
    ]);
    let err = stderr(&out);
    assert!(!err.contains("warning: telemetry"), "{err}");

    let mut metric_names = BTreeSet::new();
    for body in collector.bodies("/v1/metrics") {
        let request: ExportMetricsServiceRequest = serde_json::from_slice(&body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&body)));
        for rm in request.resource_metrics {
            for sm in rm.scope_metrics {
                metric_names.extend(sm.metrics.into_iter().map(|m| m.name));
            }
        }
    }
    assert!(
        metric_names.contains("agentfs.record.duration"),
        "{metric_names:?}"
    );
    let mut span_names = BTreeSet::new();
    for body in collector.bodies("/v1/traces") {
        let request: ExportTraceServiceRequest = serde_json::from_slice(&body).unwrap();
        for rs in request.resource_spans {
            for ss in rs.scope_spans {
                span_names.extend(ss.spans.into_iter().map(|s| s.name));
            }
        }
    }
    assert!(span_names.contains("agentfs.record"), "{span_names:?}");
    let mut events = Vec::new();
    for body in collector.bodies("/v1/logs") {
        let request: ExportLogsServiceRequest = serde_json::from_slice(&body).unwrap();
        for rl in request.resource_logs {
            for sl in rl.scope_logs {
                events.extend(sl.log_records.into_iter().map(|r| r.event_name));
            }
        }
    }
    assert!(
        events.iter().any(|e| e == "agentfs.path_refused"),
        "{events:?}"
    );
}

/// A fake OTLP/gRPC collector on a local port: keeps every request, with
/// the value of its `x-test-header` metadata.
mod grpc {
    use std::sync::{Arc, Mutex};

    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
        LogsService, LogsServiceServer,
    };
    use opentelemetry_proto::tonic::collector::logs::v1::{
        ExportLogsServiceRequest, ExportLogsServiceResponse,
    };
    use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
        MetricsService, MetricsServiceServer,
    };
    use opentelemetry_proto::tonic::collector::metrics::v1::{
        ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    };
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
        TraceService, TraceServiceServer,
    };
    use opentelemetry_proto::tonic::collector::trace::v1::{
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    };
    use tonic::{Request, Response, Status};

    #[derive(Default)]
    pub struct Received {
        pub traces: Vec<ExportTraceServiceRequest>,
        pub metrics: Vec<ExportMetricsServiceRequest>,
        pub logs: Vec<ExportLogsServiceRequest>,
        pub headers: Vec<Option<String>>,
    }

    #[derive(Clone, Default)]
    pub struct Collector(pub Arc<Mutex<Received>>);

    impl Collector {
        fn note<T>(&self, request: &Request<T>) {
            let header = request
                .metadata()
                .get("x-test-header")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            self.0.lock().unwrap().headers.push(header);
        }
    }

    #[tonic::async_trait]
    impl TraceService for Collector {
        async fn export(
            &self,
            request: Request<ExportTraceServiceRequest>,
        ) -> Result<Response<ExportTraceServiceResponse>, Status> {
            self.note(&request);
            self.0.lock().unwrap().traces.push(request.into_inner());
            Ok(Response::new(ExportTraceServiceResponse::default()))
        }
    }

    #[tonic::async_trait]
    impl MetricsService for Collector {
        async fn export(
            &self,
            request: Request<ExportMetricsServiceRequest>,
        ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
            self.note(&request);
            self.0.lock().unwrap().metrics.push(request.into_inner());
            Ok(Response::new(ExportMetricsServiceResponse::default()))
        }
    }

    #[tonic::async_trait]
    impl LogsService for Collector {
        async fn export(
            &self,
            request: Request<ExportLogsServiceRequest>,
        ) -> Result<Response<ExportLogsServiceResponse>, Status> {
            self.note(&request);
            self.0.lock().unwrap().logs.push(request.into_inner());
            Ok(Response::new(ExportLogsServiceResponse::default()))
        }
    }

    /// Starts the collector on its own runtime thread; returns it and its
    /// port.
    pub fn start() -> (Collector, u16) {
        let collector = Collector::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let served = collector.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap().port()).unwrap();
                tonic::transport::Server::builder()
                    .add_service(TraceServiceServer::new(served.clone()))
                    .add_service(MetricsServiceServer::new(served.clone()))
                    .add_service(LogsServiceServer::new(served))
                    .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
                    .await
                    .unwrap();
            });
        });
        (collector, rx.recv().unwrap())
    }
}

#[test]
fn exports_over_otlp_grpc_with_headers() {
    let (collector, port) = grpc::start();
    let v = Volume::new();
    let endpoint = format!("http://127.0.0.1:{port}");
    let out = v.record(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ("OTEL_EXPORTER_OTLP_HEADERS", "x-test-header=secret%20value"),
    ]);
    let err = stderr(&out);
    assert!(!err.contains("warning: telemetry"), "{err}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 record(s) into 1 commit(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let received = collector.0.lock().unwrap();
    let metric_names: BTreeSet<String> = received
        .metrics
        .iter()
        .flat_map(|r| &r.resource_metrics)
        .flat_map(|rm| &rm.scope_metrics)
        .flat_map(|sm| &sm.metrics)
        .map(|m| m.name.clone())
        .collect();
    assert!(
        metric_names.contains("agentfs.record.duration"),
        "{metric_names:?}"
    );
    let span_names: BTreeSet<String> = received
        .traces
        .iter()
        .flat_map(|r| &r.resource_spans)
        .flat_map(|rs| &rs.scope_spans)
        .flat_map(|ss| &ss.spans)
        .map(|s| s.name.clone())
        .collect();
    assert!(span_names.contains("agentfs.record"), "{span_names:?}");
    let events: Vec<&str> = received
        .logs
        .iter()
        .flat_map(|r| &r.resource_logs)
        .flat_map(|rl| &rl.scope_logs)
        .flat_map(|sl| &sl.log_records)
        .map(|r| r.event_name.as_str())
        .collect();
    assert!(events.contains(&"agentfs.path_refused"), "{events:?}");
    assert!(!received.headers.is_empty());
    assert!(
        received
            .headers
            .iter()
            .all(|h| h.as_deref() == Some("secret value")),
        "OTEL_EXPORTER_OTLP_HEADERS arrives as metadata, decoded: {:?}",
        received.headers
    );
}

#[test]
fn a_grpc_collector_that_is_down_never_fails_the_command() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let v = Volume::new();
    let start = std::time::Instant::now();
    let out = v.record(&[
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            &format!("http://127.0.0.1:{port}"),
        ),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
    ]);
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 record(s) into 1 commit(s)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
