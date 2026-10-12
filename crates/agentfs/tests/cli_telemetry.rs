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
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ],
            "OTEL_EXPORTER_OTLP_PROTOCOL=grpc is not built",
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
    // The status ref is locked: the run fails as a whole.
    let refs = v.dir.path().join("state/git/refs/agentfs/demo");
    fs::create_dir_all(&refs).unwrap();
    fs::write(refs.join("status.lock"), "").unwrap();
    let out = v.run(
        &["patch", "apply", "--no-fetch"],
        &[("OTEL_EXPORTER_OTLP_ENDPOINT", &collector.endpoint())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(gauge_last(&collector, "agentfs.patch.blocked"), Some(1));
}
