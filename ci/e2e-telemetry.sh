#!/usr/bin/env bash
# End-to-end test of telemetry (design §13) against a real OpenTelemetry
# Collector: every metric, span and alarm log the README lists must arrive.
# The failure counters come from real failures (an unreadable file, an
# unwritable object store, a git that fails the repack), and the history
# service's fronts gauge, a front's round trip and its service failures
# (a hung service: `timeout`; a stopped one: `connect`) from a real
# `serve-history` and FUSE front mounts.
#
# Needs OTEL_OUT, the file the collector from ci/otelcol.yaml writes, the
# collector listening on localhost:4318, jq (ci/otel-points.sh reads the
# points), and for the fronts /dev/fuse, fuse3 and passwordless sudo. Runs as
# a user other than root, so that permissions stop it.

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
  agentfs.precheck.rejected_paths agentfs.state.bytes \
  agentfs.patch.queue_depth agentfs.patch.blocked; do
  wait_for 20 grep -q "\"$name\"" "$OTEL_OUT" || fail "metric $name never arrived"
  pass "metric $name"
done
for name in agentfs.record agentfs.commit agentfs.push; do
  wait_for 20 grep -q "\"name\":\"$name\"" "$OTEL_OUT" || fail "span $name never arrived"
  pass "span $name"
done
# Alarms only: a successful push is not logged (README, Telemetry).
for name in agentfs.path_refused agentfs.push_failed agentfs.diverged \
  agentfs.remote_misconfigured; do
  wait_for 20 grep -q "$name" "$OTEL_OUT" || fail "log $name never arrived"
  pass "log $name"
done
assert_contains "$(cat "$OTEL_OUT")" '"agentfs-ci"' "service.name is set"

step "a failing exporter is reported, and counted over metrics"
# Traces go to a port nothing listens on; metrics still reach the collector.
echo again >"$TREE/again.txt"
ERR=$VOL/failing.err
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:9/v1/traces \
  agentfs record again.txt 2>"$ERR" || { cat "$ERR"; fail "record failed"; }
assert_contains "$(cat "$ERR")" "warning: telemetry: traces export failed" \
  "stderr warns about the failed trace export"
wait_for 20 grep -q '"agentfs.telemetry.export_failures"' "$OTEL_OUT" ||
  fail "metric agentfs.telemetry.export_failures never arrived"
pass "metric agentfs.telemetry.export_failures"

step "real failures: a record, a commit and a repack"
# The runner is not root, so permissions stop it.
echo s >"$TREE/secret.txt"
chmod 000 "$TREE/secret.txt"
if agentfs record secret.txt 2>"$VOL/secret.err"; then
  fail "an unreadable file was recorded"
fi
chmod 644 "$TREE/secret.txt" && rm "$TREE/secret.txt"
chmod -R a-w "$STATE/git/objects"
echo c >"$TREE/commit-fails.txt"
agentfs record commit-fails.txt 2>"$VOL/commit.err" || true
chmod -R u+w "$STATE/git/objects"
agentfs flush >/dev/null
assert_contains "$(agentfs status)" "backlog:   0 record(s)" "the record stayed journaled and was committed later"
# A repack whose git fails: a git that refuses, first in PATH for `run`.
FAKE=$(mktemp -d)
printf '#!/bin/sh\necho "fake git refuses" >&2\nexit 1\n' >"$FAKE/git"
chmod +x "$FAKE/git"
volume_config agentfs.repackInterval 1m
echo r >"$TREE/repack.txt"
agentfs record repack.txt >/dev/null
PATH=$FAKE:$PATH agentfs_start "$VOL/repack.log" run
RUN=$AGENTFS_PID
counted() { # metric attr-substring
  ci/otel-points.sh "$OTEL_OUT" "$1" | grep -- "$2" | awk '$1 >= 1 { found = 1 } END { exit !found }'
}
wait_for 20 counted agentfs.record.failures trigger=close || fail "agentfs.record.failures never counted"
pass "agentfs.record.failures from an unreadable file"
wait_for 20 counted agentfs.commit.failures "" || fail "agentfs.commit.failures never counted"
pass "agentfs.commit.failures from unwritable objects"
wait_for 100 counted agentfs.repack.failures "" || { cat "$VOL/repack.log"; fail "agentfs.repack.failures never counted"; }
pass "agentfs.repack.failures from a failing git"
wait_for 20 grep -q agentfs.repack_failed "$OTEL_OUT" || fail "log agentfs.repack_failed never arrived"
pass "log agentfs.repack_failed"
kill -TERM "$RUN"
wait "$RUN" || { cat "$VOL/repack.log"; fail "run did not exit cleanly"; }

step "fronts and the history service (FUSE)"
[ -c /dev/fuse ] || fail "/dev/fuse is missing: this step needs FUSE"
# As root throughout: the service and the fronts are root, and libgit2
# refuses a repository another user owns.
VOL=$(mktemp -d)
STATE=$VOL/state
TREE=$VOL/tree
mkdir -p "$TREE"
sudo "$AGENTFS_BIN" --state "$STATE" --root "$TREE" init --volume fronts >/dev/null
sudo git --git-dir "$STATE/git" config agentfs.commitWindow 50ms
SOCK=$STATE/meta/history/history.sock
MNT_A=$VOL/mnt-a
MNT_B=$VOL/mnt-b
mkdir -p "$MNT_A" "$MNT_B"
as_root() { # pidfile command...: runs as root with the telemetry settings, its PID in pidfile
  local pidfile=$1; shift
  sudo --preserve-env=OTEL_EXPORTER_OTLP_ENDPOINT,OTEL_METRIC_EXPORT_INTERVAL,OTEL_SERVICE_NAME \
    sh -c 'echo $$ >"$0"; exec "$@"' "$pidfile" "$@"
}
front() { # pod mountpoint
  as_root "$VOL/$1.pid" env AGENTFS_POD="$1" AGENTFS_POD_UID="$1-uid" AGENTFS_REPLICA="$1" \
    "$AGENTFS_BIN" --state "$STATE" mount --history-service "$SOCK" --history-timeout 2s \
    --history-wait 30s "$TREE" "$2" >>"$VOL/$1.log" 2>&1 &
  wait_for 30 mountpoint -q "$2" || { cat "$VOL/$1.log"; fail "$1 never mounted"; }
}
cleanup_fronts() {
  for p in pod-a pod-b service; do
    [ -f "$VOL/$p.pid" ] && sudo kill -CONT "$(cat "$VOL/$p.pid")" 2>/dev/null || true
    [ -f "$VOL/$p.pid" ] && sudo kill -TERM "$(cat "$VOL/$p.pid")" 2>/dev/null || true
  done
  sudo umount -l "$MNT_A" "$MNT_B" 2>/dev/null || true
}
trap cleanup_fronts EXIT
fronts_now() { ci/otel-points.sh "$OTEL_OUT" agentfs.history_service.fronts | tail -n 1 | cut -d' ' -f1; }
fronts_are() { [ "$(fronts_now)" = "$1" ]; }
gone() { ! sudo kill -0 "$(cat "$1")" 2>/dev/null; }

as_root "$VOL/service.pid" env AGENTFS_REPLICA=history-0 "$AGENTFS_BIN" --state "$STATE" \
  --root "$TREE" serve-history >"$VOL/service.log" 2>&1 &
wait_for 30 grep -q '^serving history' "$VOL/service.log" || { cat "$VOL/service.log"; fail "never served"; }
wait_for 20 fronts_are 0 || fail "the fronts gauge never showed 0"
pass "agentfs.history_service.fronts: 0"
front pod-a "$MNT_A"
wait_for 20 fronts_are 1 || fail "the fronts gauge never showed 1"
pass "agentfs.history_service.fronts: 1"
front pod-b "$MNT_B"
wait_for 20 fronts_are 2 || fail "the fronts gauge never showed 2"
pass "agentfs.history_service.fronts: 2"
sudo kill -TERM "$(cat "$VOL/pod-b.pid")"
wait_for 30 gone "$VOL/pod-b.pid" || fail "pod-b never stopped"
wait_for 20 fronts_are 1 || fail "the fronts gauge never went back to 1"
pass "agentfs.history_service.fronts: back to 1"
SEQ=$(ci/otel-points.sh "$OTEL_OUT" agentfs.history_service.fronts | cut -d' ' -f1 | uniq | tr '\n' ' ')
echo "fronts gauge: $SEQ"
assert_contains " $SEQ" " 0 1 2 1 " "the gauge went 0, 1, 2, 1"

sudo sh -c "echo one >'$MNT_A/one.txt'"
wait_for 20 counted agentfs.front.round_trip request=record || fail "agentfs.front.round_trip never arrived"
pass "agentfs.front.round_trip for a record"

# A hung service: the front's close times out.
sudo kill -STOP "$(cat "$VOL/service.pid")"
sudo sh -c "echo two >'$MNT_A/two.txt'" 2>/dev/null || true
sudo kill -CONT "$(cat "$VOL/service.pid")"
wait_for 20 counted agentfs.front.service_failures stage=timeout ||
  fail "agentfs.front.service_failures{stage=timeout} never counted"
pass "agentfs.front.service_failures: timeout"

# The service gone: the front cannot connect.
sudo kill -TERM "$(cat "$VOL/service.pid")"
wait_for 30 gone "$VOL/service.pid" || fail "the service never stopped"
for n in 1 2 3; do
  sudo sh -c "echo three-$n >'$MNT_A/three.txt'" 2>/dev/null || true
done
wait_for 20 counted agentfs.front.service_failures stage=connect ||
  fail "agentfs.front.service_failures{stage=connect} never counted"
pass "agentfs.front.service_failures: connect"
cleanup_fronts
trap - EXIT

step "telemetry passed"
