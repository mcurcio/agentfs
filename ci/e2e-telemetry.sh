#!/usr/bin/env bash
# End-to-end test of a tree mount's telemetry against a real OpenTelemetry
# Collector: a volume whose operator settings (docs/trees.md, "Operator
# settings") name a remote and a patch source is mounted with OTEL_* set,
# runs one command and drains. Its stderr must carry one `not built:` line
# per key, and the collector must get `agentfs.not_built` points for
# capability=push and capability=patches, and none for a capability no key
# names, from a resource carrying agentfs.volume and service.name.
#
# Needs OTEL_OUT, the file the collector from ci/otelcol.yaml writes, the
# collector listening on localhost:4318, jq, /dev/fuse and fusermount3. Runs
# as any user: the mount is --no-allow-other.

. "$(dirname "$0")/lib.sh"

: "${OTEL_OUT:?path of the collector output file}"

export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_METRIC_EXPORT_INTERVAL=500
export OTEL_SERVICE_NAME=agentfs-ci-telemetry

WORK=$(mktemp -d)
STATE=$WORK/state
VOLUME=telem

step "a tree volume whose settings name a remote and a patch source"
cat >"$WORK/settings" <<'EOF'
[remote "origin"]
	url = https://git.example.com/acme/history.git
[agentfs]
	patchRemote = origin
EOF
"$AGENTFS_BIN" --state "$STATE" init --volume "$VOLUME" --include "$WORK/settings" >/dev/null
pass "init --include"

step "mount, run a command, drain"
mkdir "$WORK/mnt"
"$AGENTFS_BIN" --state "$STATE" mount "$WORK/mnt" --no-allow-other -- \
  sh -c 'echo ok >"$1/ok.txt"' sh "$WORK/mnt" 2>"$WORK/mount.err" ||
  { cat "$WORK/mount.err"; fail "the mount failed"; }
ERR=$(cat "$WORK/mount.err")
assert_contains "$ERR" "not built: remote.origin.url (push): " "stderr names the remote as not built"
assert_contains "$ERR" "not built: agentfs.patchremote (patches): " "stderr names the patch source as not built"
case "$ERR" in *"warning: telemetry"*) echo "$ERR"; fail "telemetry warned" ;; esac
pass "no telemetry warning"

step "agentfs.not_built reached the collector"
# The points of agentfs.not_built from this volume's resource, one per line:
# the value, then the capability.
not_built() {
  jq -rR --arg volume "$VOLUME" '
    fromjson? | .resourceMetrics[]?
    | select(any(.resource.attributes[]?; .key == "agentfs.volume" and .value.stringValue == $volume))
    | .scopeMetrics[]?.metrics[]? | select(.name == "agentfs.not_built")
    | .gauge.dataPoints[]?
    | "\(.asInt // .asDouble) \(.attributes[]? | select(.key == "capability") | .value.stringValue)"' "$OTEL_OUT"
}
has_point() { not_built | grep -x -- "$1" >/dev/null; }
for capability in push patches; do
  wait_for 20 has_point "1 $capability" || {
    not_built
    fail "no agentfs.not_built point of 1 for capability=$capability from agentfs.volume=$VOLUME"
  }
  pass "agentfs.not_built{capability=$capability} = 1, agentfs.volume=$VOLUME"
done
POINTS=$(not_built)
case "$POINTS" in *" lfs"* | *" other"*) echo "$POINTS"; fail "a capability no key names has a point" ;; esac
pass "no point for lfs or other"
assert_contains "$(cat "$OTEL_OUT")" "\"$OTEL_SERVICE_NAME\"" "service.name is set"

step "telemetry passed"
