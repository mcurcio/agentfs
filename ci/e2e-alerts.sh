#!/usr/bin/env bash
# The example alert rules (deploy/alerts, docs/deployment.md "Alerts")
# against the real binary: agentfs exports its metrics over OTLP straight
# into a real Prometheus (its OTLP receiver, default translation) that has
# deploy/alerts/agentfs.rules.yml loaded. Checks that
#
# - every metric the rules name exists in Prometheus under that name, but
#   the three this script cannot cause cheaply (below);
# - a promoted resource attribute (agentfs.volume) becomes a label;
# - real failures fire their alerts: a record and a telemetry export that
#   fail, a diverged remote and a misconfigured one.
#
# Needs PROMETHEUS (the prometheus binary, ci/install-prometheus.sh), curl
# and jq. No FUSE and no root: runs as a user other than root, so that
# permissions stop it.

. "$(dirname "$0")/lib.sh"

: "${PROMETHEUS:?path of the prometheus binary}"
RULES=$PWD/deploy/alerts/agentfs.rules.yml
PROM=http://127.0.0.1:9090
# Named by the rules, not caused here: a repack failure needs a repack
# interval (at least 1 minute) to pass, a reconcile failure 30 s of failed
# directory-move reconciliations, and a front failure FUSE and a history
# service. ci/e2e-telemetry.sh causes all three and checks their OTLP names.
NOT_CAUSED="agentfs_repack_failures_total agentfs_reconcile_failures_total agentfs_front_service_failures_total"

PROM_DIR=$(mktemp -d)
cat >"$PROM_DIR/prometheus.yml" <<EOF
global:
  evaluation_interval: 1s
rule_files: ["$RULES"]
otlp:
  promote_resource_attributes: [agentfs.volume]
EOF
PROM_LOG=$PROM_DIR/prometheus.log
"$PROMETHEUS" --config.file="$PROM_DIR/prometheus.yml" --storage.tsdb.path="$PROM_DIR/data" \
  --web.listen-address=127.0.0.1:9090 --web.enable-otlp-receiver >"$PROM_LOG" 2>&1 &
PROM_PID=$!
trap 'kill "$PROM_PID" 2>/dev/null || true' EXIT
wait_for 60 curl -fs "$PROM/-/ready" || { cat "$PROM_LOG"; fail "prometheus never became ready"; }
pass "prometheus is ready with the rules loaded"

# Only metrics have an endpoint, so only metrics are exported.
export OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=$PROM/api/v1/otlp/v1/metrics
export OTEL_METRIC_EXPORT_INTERVAL=500
export OTEL_SERVICE_NAME=agentfs

query() { # promql: prints the result vector as JSON
  curl -fsS "$PROM/api/v1/query" --data-urlencode "query=$1" | jq -c '.data.result'
}
has_series() { [ "$(query "$1" | jq length)" -gt 0 ]; }
alert_active() { # alertname: pending or firing
  curl -fsS "$PROM/api/v1/alerts" |
    jq -e --arg name "$1" '[.data.alerts[] | select(.labels.alertname == $name)] | length > 0' >/dev/null
}

new_volume alerts
REMOTE=$(mktemp -d)/remote.git
git init -q --bare "$REMOTE"
volume_config remote.good.url "$REMOTE"
volume_config remote.gone.url "$VOL/does-not-exist.git"
volume_config remote.broken.url "$REMOTE"
volume_config remote.broken.agentfsMaxBlobSize lots
volume_config agentfs.pushInterval 1s

step "cause failures"
echo ok >"$TREE/ok.txt"
agentfs record ok.txt >/dev/null
agentfs push --remote good --remote gone >/dev/null 2>&1 || true
# A diverged remote: another writer moves the branch.
OTHER=$(mktemp -d)
git clone -q "$REMOTE" "$OTHER/c" && git -C "$OTHER/c" checkout -q -b w origin/agentfs/alerts
echo x >"$OTHER/c/x" && git -C "$OTHER/c" add x
git -C "$OTHER/c" -c user.name=o -c user.email=o@example.com commit -q -m other
git -C "$OTHER/c" push -q origin w:refs/heads/agentfs/alerts
echo more >"$TREE/more.txt"
agentfs record more.txt >/dev/null
# A record that fails: the file cannot be read.
echo s >"$TREE/secret.txt"
chmod 000 "$TREE/secret.txt"
if agentfs record secret.txt 2>/dev/null; then
  fail "an unreadable file was recorded"
fi
chmod 644 "$TREE/secret.txt" && rm "$TREE/secret.txt"
# A commit that fails: the object store cannot be written.
chmod -R a-w "$STATE/git/objects"
echo c >"$TREE/commit-fails.txt"
agentfs record commit-fails.txt >/dev/null 2>&1 || true
chmod -R u+w "$STATE/git/objects"
# A telemetry export that fails: traces to a port nothing listens on.
echo again >"$TREE/again.txt"
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:9/v1/traces \
  agentfs record again.txt >/dev/null 2>&1
# The long-running gauges: backlog, push state, patch queue, state size.
RUN_LOG=$VOL/run.log
agentfs_start "$RUN_LOG" run
RUN=$AGENTFS_PID

step "every metric the rules name is in Prometheus"
NAMES=$(grep -o 'agentfs_[a-z_]*' "$RULES" | sort -u)
[ -n "$NAMES" ] || fail "no metric names found in $RULES"
for name in $NAMES; do
  case " $NOT_CAUSED " in
    *" $name "*) echo "skip $name (not caused here; ci/e2e-telemetry.sh)"; continue ;;
  esac
  wait_for 30 has_series "$name" || {
    echo "agentfs series in Prometheus:"
    query 'count by (__name__) ({__name__=~"agentfs_.*"})' | jq -r '.[].metric.__name__'
    fail "metric $name is not in Prometheus"
  }
  pass "metric $name"
done

step "a promoted resource attribute is a label"
wait_for 30 has_series 'agentfs_remote_misconfigured{agentfs_volume="alerts",remote="broken",job="agentfs",instance="ci-0"}' ||
  fail "no series labelled agentfs_volume, job and instance: $(query agentfs_remote_misconfigured)"
pass "agentfs.volume became agentfs_volume; service.name and the replica became job and instance"

step "real failures fire their alerts"
for alert in AgentfsRecordFailed AgentfsTelemetryExportFailing AgentfsPushDiverged \
  AgentfsRemoteMisconfigured; do
  wait_for 30 alert_active "$alert" || {
    curl -fsS "$PROM/api/v1/alerts" | jq -c '.data.alerts[].labels'
    fail "$alert did not fire"
  }
  pass "$alert"
done

kill -TERM "$RUN"
wait "$RUN" || { cat "$RUN_LOG"; fail "run did not exit cleanly"; }
