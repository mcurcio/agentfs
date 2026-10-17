# Telemetry

agentfs exports OpenTelemetry metrics, traces and logs over OTLP,
configured with the standard `OTEL_*` environment variables. With no
endpoint set, telemetry is off and costs nothing. Every metric, span, log
record and alarm event, with its attributes, is listed in
[design §13](design.md#13-observability-opentelemetry); this page is how
to turn it on and what to expect.

`mount`, `run`, `record`, `flush`, `push`, `serve-history`, `patch apply`,
`fork finish`, `fork abandon`, and `restore` and `revert` without a mount
export telemetry; a front (`mount` or `record` with `--history-service`)
exports its own. Under the CSI driver, the release scan's `agentfs push`
of each worktree no pod stages exports its own too, so its failures reach
the collector as `PushFailed` with the node plugin's `OTEL_*` settings.

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318
agentfs run
```

- **Variables:** an endpoint enables export, per signal or for all;
  `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none` or `OTEL_SDK_DISABLED=true`
  turn it off. `OTEL_EXPORTER_OTLP_PROTOCOL` picks **http/protobuf** (the
  default), **http/json** or **grpc** (per signal too). An `https://`
  endpoint needs a binary built with `otlp-tls` (the image is): it
  verifies the collector against `OTEL_EXPORTER_OTLP_CERTIFICATE` or the
  system store, and sends a client certificate for mutual TLS; a bearer
  token goes in `OTEL_EXPORTER_OTLP_HEADERS`. Anything else is reported
  with a `warning: telemetry:` line on stderr and that signal stays off.
  Telemetry problems never stop a command. Every variable agentfs and the
  OpenTelemetry SDK read, including sampling (`OTEL_TRACES_SAMPLER`: on a
  busy volume, sample), is in the
  [configuration reference](configuration.md#environment-variables).
- **Failed exports** (collector down, an HTTP error) print
  `warning: telemetry: <signal> export failed: ...` on stderr, at once and
  then at most once a minute while they keep failing, and a
  `telemetry: <signal> export recovered` line when they work again. Each
  failure is also counted in `agentfs.telemetry.export_failures`.
- **Resource:** `service.name`, `service.version`, `service.instance.id`
  (the replica), `process.pid`, `agentfs.volume`, and `k8s.*` from the pod
  flags (`AGENTFS_NAMESPACE`, `AGENTFS_POD`, `AGENTFS_POD_UID`,
  `AGENTFS_NODE`).
- **Shutdown** flushes telemetry, waiting up to 5s, so a collector that is
  down can add up to 5s to a short command.

## Signals

- **Metrics:** the journal and push backlogs, failures by stage and
  reason, divergence, a started volume's start-branch alarm
  (`agentfs.start.moved{remote}`, 1 while its start branch holds commits
  it has not taken in or accepted), how the volume declares its remotes
  (`agentfs.remotes.mode{mode}`, 1, with `mode` one of `pushed`,
  `none_valid`, `local_only`, `undeclared`;
  [configuration](configuration.md#declaring-remotes)), the patch queue,
  the state area's size, the history service and its fronts, and fork
  publishing: the table in
  [design §13](design.md#13-observability-opentelemetry).
- **Spans:** `agentfs.record`, `agentfs.commit`, `agentfs.push`,
  `agentfs.patch`, `agentfs.fork.finish` and `agentfs.fork.publish`,
  carrying journal sequence numbers, commit ids and outcomes.
- **Logs:** every alarm is a log record (`agentfs.path_refused`,
  `agentfs.push_failed`, `agentfs.start_branch_moved`,
  `agentfs.remotes_undeclared`, ...) with its
  details as `agentfs.*` attributes
  and the trace of the operation that raised it; a fork's lifecycle is
  logged at INFO. Alarms are printed to stderr as well, with telemetry on
  or off. The alarms and when each is raised: the events table in
  [design §13](design.md#13-observability-opentelemetry).

## Alerts

Example Prometheus alert rules on these metrics, each linking its
runbook, are in [`deploy/alerts`](../deploy/alerts), as a rules file and a
`PrometheusRule` ([deployment.md](deployment.md#alerts)); the CSI chart
installs them with `alerts.enabled=true` ([helm.md](helm.md#3-values)).
