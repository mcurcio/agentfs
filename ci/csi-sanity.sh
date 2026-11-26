#!/usr/bin/env bash
# csi-sanity (kubernetes-csi/csi-test) against `agentfs-csi`, which serves
# the identity, controller and node services of one node: every CSI call,
# with real `agentfs init` and `agentfs host` children and FUSE views,
# checked for the spec's error codes, idempotency, topology and cleanup.
# Then checks that no agentfs mount, published record or host is left, and
# that every deleted volume was kept (deleting a volume is not built).
#
# Needs Linux with /dev/fuse, fuse3, passwordless sudo and csi-sanity on
# PATH (`go install github.com/kubernetes-csi/csi-test/v5/cmd/csi-sanity@v5.6.0`);
# reproducible on any such host:
#
#   cargo build --release -p agentfs-cli -p agentfs-csi && ci/csi-sanity.sh
#
# AGENTFS_BIN and AGENTFS_CSI_BIN name the binaries (default: release).

: "${AGENTFS_BIN:=$PWD/target/release/agentfs}"
. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"
CSI_SANITY=${CSI_SANITY:-$(command -v csi-sanity || true)}
[ -x "$CSI_SANITY" ] || fail "csi-sanity is not on PATH"

HERE=$(cd "$(dirname "$0")" && pwd)
AGENTFS_CSI_BIN=${AGENTFS_CSI_BIN:-$PWD/target/release/agentfs-csi}
for bin in "$AGENTFS_BIN" "$AGENTFS_CSI_BIN"; do
  [ -x "$bin" ] || fail "$bin is missing: cargo build --release -p agentfs-cli -p agentfs-csi"
done

W=$(mktemp -d)
chmod 755 "$W"
LOGS=${RUNNER_TEMP:-$W}/agentfs-csi-logs
mkdir -p "$W/profiles" "$LOGS"
cp "$HERE/csi-sanity/profiles/sanity.config" "$W/profiles/"
LOG=$LOGS/agentfs-csi.log
: >"$LOG"
SOCK=$W/csi.sock
DRIVER_PID=

# Mounts under the work directory (the views at the targets).
agentfs_mounts() { grep -c -F " $W/" /proc/self/mountinfo || true; }

cleanup() {
  if [ -n "$DRIVER_PID" ]; then
    sudo kill -TERM "$DRIVER_PID" 2>/dev/null || true
    for _ in $(seq 60); do sudo kill -0 "$DRIVER_PID" 2>/dev/null || break; sleep 0.5; done
    sudo kill -KILL "$DRIVER_PID" 2>/dev/null || true
  fi
  sudo sh -c "grep '$W' /proc/self/mountinfo | cut -d' ' -f5 | sort -r | xargs -r -n1 umount -l" || true
  sudo rm -rf "$W"
}
on_exit cleanup

step "start agentfs-csi"
sudo "$AGENTFS_CSI_BIN" \
  --endpoint "unix://$SOCK" \
  --node-id sanity \
  --base-path "$W/base" \
  --node-state "$W/node" \
  --profiles-dir "$W/profiles" \
  --agentfs-bin "$AGENTFS_BIN" \
  >"$LOG" 2>&1 &
DRIVER_PID=$(sudo sh -c 'for _ in $(seq 100); do p=$(pgrep -n -x agentfs-csi) && { echo "$p"; exit 0; }; sleep 0.1; done; exit 1') \
  || fail "agentfs-csi did not start: $(cat "$LOG")"
wait_for 30 sudo test -S "$SOCK" || fail "no socket at $SOCK: $(cat "$LOG")"
pass "driver listening on $SOCK (pid $DRIVER_PID)"

step "csi-sanity"
# --csi.testvolumeexpandsize is unset (no expansion); the capability-gated
# tests (snapshots, expansion, stats, list, stage) skip themselves because
# the driver does not advertise them. The clone tests ("source volume") are
# skipped: the driver clones only into a fork class (fork: "true"), which
# the test parameters are not; crates/agentfs-csi/tests/grpc.rs and
# ci/e2e-csi-forks.sh cover cloning. No case publishes one volume at two
# targets and expects it to work (several pods on one volume are not
# built): the one that tries expects FAILED_PRECONDITION, which the driver
# answers. csi-sanity makes its staging directory whatever the driver
# advertises, so --csi.stagingdir keeps it under the work directory.
sudo "$CSI_SANITY" \
  --csi.endpoint "unix://$SOCK" \
  --ginkgo.skip "source volume" \
  --csi.mountdir "$W/target" \
  --csi.stagingdir "$W/staging" \
  --csi.testvolumeparameters "$HERE/csi-sanity/params.yaml" \
  --csi.testvolumesize 1073741824 \
  --ginkgo.v \
  --ginkgo.no-color
pass "csi-sanity"
assert_contains "$(cat "$LOG")" "] not built: agentfs.pushinterval" "the driver served volumes with real agentfs hosts, which said what is not built"
assert_contains "$(cat "$LOG")" "[host " "the hosts' output is forwarded"

step "nothing left behind"
assert_eq 0 "$(agentfs_mounts)" "no agentfs mounts left"
left=$(sudo find "$W/node/published" -name '*.json' 2>/dev/null | wc -l)
assert_eq 0 "$left" "no published records left"
left=$(sudo find "$W/base" -mindepth 2 -maxdepth 2 -name csi.json | wc -l)
assert_eq 0 "$left" "no claim left: every created volume was deleted"
kept=$(sudo find "$W/base" -mindepth 2 -maxdepth 2 -name csi.retained.json | wc -l)
[ "$kept" -gt 0 ] || fail "no deleted volume was kept"
pass "$kept deleted volume(s) kept (deleting a volume is not built)"
hosts=$(pgrep -f -c -- "--state $W/base/.* host --launcher csi" || true)
assert_eq 0 "$hosts" "no agentfs host left running"

step "stop the driver"
sudo kill -TERM "$DRIVER_PID"
for _ in $(seq 60); do sudo kill -0 "$DRIVER_PID" 2>/dev/null || break; sleep 0.5; done
sudo kill -0 "$DRIVER_PID" 2>/dev/null && fail "agentfs-csi did not stop on SIGTERM"
DRIVER_PID=
[ ! -S "$SOCK" ] || fail "the socket was left behind"
pass "stopped, socket removed"
