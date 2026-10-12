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
    requests: Arc<Requests>,
    connections: Arc<AtomicUsize>,
}

impl Collector {
    fn start() -> Self {
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
                    thread::spawn(move || serve(stream, &requests));
                }
            });
        }
        Self {
            port,
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
fn serve(stream: TcpStream, requests: &Requests) {
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
    requests.lock().unwrap().push((path, body));
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\n\
          content-length: 0\r\nconnection: close\r\n\r\n",
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
