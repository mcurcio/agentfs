#!/usr/bin/env bash
# End-to-end test of telemetry (design §13) against a real OpenTelemetry
# Collector: every metric, span and alarm log the README lists must arrive.
#
# Needs OTEL_OUT, the file the collector from ci/otelcol.yaml writes, and the
# collector listening on localhost:4318.

. "$(dirname "$0")/lib.sh"

: "${OTEL_OUT:?path of the collector output file}"

export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export OTEL_METRIC_EXPORT_INTERVAL=500
export OTEL_SERVICE_NAME=agentfs-ci

new_volume telem
REMOTE=$(mktemp -d)/remote.git
git init -q --bare "$REMOTE"
volume_config remote.good.url "$REMOTE"
volume_config remote.gone.url "$VOL/does-not-exist.git"
SMALL=$(mktemp -d)/small.git
git init -q --bare "$SMALL"
volume_config remote.small.url "$SMALL"
volume_config remote.small.agentfsMaxBlobSize 8
volume_config remote.broken.url "$REMOTE"
volume_config remote.broken.agentfsMaxBlobSize lots
volume_config agentfs.pushInterval 1s

step "generate every signal"
echo ok >"$TREE/small.txt"
echo "this file is over eight bytes" >"$TREE/big.txt"
agentfs record small.txt big.txt || true # big.txt is refused by "small"
set +e
agentfs push --remote good --remote gone
set -e
# A diverged remote: another writer moves the branch.
OTHER=$(mktemp -d)
git clone -q "$REMOTE" "$OTHER/c" && git -C "$OTHER/c" checkout -q -b w origin/agentfs/telem
echo x >"$OTHER/c/x" && git -C "$OTHER/c" add x
git -C "$OTHER/c" -c user.name=o -c user.email=o@example.com commit -q -m other
git -C "$OTHER/c" push -q origin w:refs/heads/agentfs/telem
echo more >"$TREE/more.txt"
agentfs record more.txt
LOG=$VOL/run.log
agentfs_start "$LOG" run
RUN=$AGENTFS_PID
sleep 4 # several metric export intervals and push attempts
kill -TERM "$RUN"
wait "$RUN" || { cat "$LOG"; fail "run did not exit cleanly"; }

step "everything reached the collector"
for name in \
  agentfs.record.duration agentfs.commit.count agentfs.journal.backlog \
  agentfs.push.unpushed_commits agentfs.push.oldest_unpushed_age \
  agentfs.push.failures agentfs.push.diverged agentfs.remote.misconfigured \
  agentfs.precheck.rejected_paths agentfs.state.bytes; do
  wait_for 20 grep -q "\"$name\"" "$OTEL_OUT" || fail "metric $name never arrived"
  pass "metric $name"
done
for name in agentfs.record agentfs.commit agentfs.push; do
  wait_for 20 grep -q "\"name\":\"$name\"" "$OTEL_OUT" || fail "span $name never arrived"
  pass "span $name"
done
for name in agentfs.path_refused agentfs.push_failed agentfs.diverged \
  agentfs.remote_misconfigured agentfs.push_succeeded; do
  wait_for 20 grep -q "$name" "$OTEL_OUT" || fail "log $name never arrived"
  pass "log $name"
done
assert_contains "$(cat "$OTEL_OUT")" '"agentfs-ci"' "service.name is set"

step "telemetry passed"
