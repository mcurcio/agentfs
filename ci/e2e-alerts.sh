#!/usr/bin/env bash
# The example alert rules (deploy/alerts, docs/deployment.md "Alerts")
# against the real binary: a tree mount exports its metrics over OTLP
# straight into a real Prometheus (its OTLP receiver, default translation)
# that has deploy/alerts/agentfs.rules.yml loaded. Checks that
#
# - every metric the rules name exists in Prometheus under that name, but
#   those listed in NOT_CAUSED (below, each with its reason);
# - a promoted resource attribute (agentfs.volume) becomes a label;
# - AgentfsNotBuilt goes pending or firing for a volume whose operator
#   settings name a remote.
#
# Needs PROMETHEUS (the prometheus binary, ci/install-prometheus.sh), curl,
# jq, /dev/fuse and fusermount3. Runs as any user: the mount is
# --no-allow-other.

. "$(dirname "$0")/lib.sh"

: "${PROMETHEUS:?path of the prometheus binary}"
RULES=$PWD/deploy/alerts/agentfs.rules.yml
PROM=http://127.0.0.1:9090
# Named by the rules, not caused here. The recording engine's metrics: only
# the CSI driver runs that engine, and promtool's unit tests
# (deploy/alerts/agentfs.rules.test.yml) check their rules. And
# agentfs_telemetry_export_failures_total: a tree mount exports metrics only,
# so the one export that could fail is the one that would carry the count.
RECORDING="agentfs_record_failures_total agentfs_commit_failures_total agentfs_journal_backlog
agentfs_repack_failures_total agentfs_reconcile_failures_total agentfs_front_service_failures_total
agentfs_push_oldest_unpushed_age_seconds agentfs_push_diverged agentfs_remote_misconfigured
agentfs_patch_blocked agentfs_nested_gitdir_heuristic_total"
NOT_CAUSED=$(echo $RECORDING agentfs_telemetry_export_failures_total)
for name in $NOT_CAUSED; do
  grep -w "$name" "$RULES" >/dev/null || fail "NOT_CAUSED names $name, which no rule names"
done

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
on_exit 'kill "$PROM_PID" 2>/dev/null || true'
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

WORK=$(mktemp -d)
STATE=$WORK/state
printf '[remote "origin"]\n\turl = https://git.example.com/acme/history.git\n' >"$WORK/settings"
"$AGENTFS_BIN" --state "$STATE" init --volume alerts --include "$WORK/settings" >/dev/null
mkdir "$WORK/mnt"

step "a tree mount whose settings name a remote"
MOUNT_LOG=$WORK/mount.log
"$AGENTFS_BIN" --state "$STATE" mount "$WORK/mnt" --no-allow-other >"$MOUNT_LOG" 2>&1 &
MOUNT=$!
on_exit 'kill "$MOUNT" "$PROM_PID" 2>/dev/null || true'
wait_for 30 grep '^mounted volume alerts' "$MOUNT_LOG" || { cat "$MOUNT_LOG"; fail "the mount never served"; }
assert_contains "$(cat "$MOUNT_LOG")" "not built: remote.origin.url (push): " "the mount logs the remote as not built"

step "every metric the rules name is in Prometheus"
# agentfs_volume is a label (the promoted agentfs.volume), checked below.
NAMES=$(grep -o 'agentfs_[a-z_]*' "$RULES" | grep -v -x agentfs_volume | sort -u)
[ -n "$NAMES" ] || fail "no metric names found in $RULES"
for name in $NAMES; do
  case " $NOT_CAUSED " in
    *" $name "*) echo "skip $name (not caused here: see NOT_CAUSED)"; continue ;;
  esac
  wait_for 30 has_series "$name" || {
    echo "agentfs series in Prometheus:"
    query 'count by (__name__) ({__name__=~"agentfs_.*"})' | jq -r '.[].metric.__name__'
    fail "metric $name is not in Prometheus"
  }
  pass "metric $name"
done

step "a promoted resource attribute is a label"
wait_for 30 has_series 'agentfs_not_built{agentfs_volume="alerts",capability="push",job="agentfs"}' ||
  fail "no series labelled agentfs_volume and job: $(query agentfs_not_built)"
pass "agentfs.volume became agentfs_volume; service.name became job"

step "AgentfsNotBuilt is active"
wait_for 30 alert_active AgentfsNotBuilt || {
  curl -fsS "$PROM/api/v1/alerts" | jq -c '.data.alerts[].labels'
  fail "AgentfsNotBuilt did not fire"
}
pass "AgentfsNotBuilt"

kill -TERM "$MOUNT"
wait "$MOUNT" || { cat "$MOUNT_LOG"; fail "the mount did not exit cleanly"; }
on_exit 'kill "$PROM_PID" 2>/dev/null || true'
pass "the mount drained"
