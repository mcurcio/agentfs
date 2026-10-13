#!/usr/bin/env bash
# csi-sanity (kubernetes-csi/csi-test) against `agentfs-csi --mode all`
# (design §14.2, §18 test 16): every CSI call of the controller and node
# services, with real history services and FUSE fronts, checked for the
# spec's error codes, idempotency and cleanup. Then checks that no agentfs
# mount is left behind.
#
# Needs Linux with /dev/fuse, fuse3, passwordless sudo and csi-sanity on
# PATH (`go install github.com/kubernetes-csi/csi-test/v5/cmd/csi-sanity@v5.6.0`);
# reproducible on any such host:
#
#   cargo build --release -p agentfs -p agentfs-csi && ci/csi-sanity.sh
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
  [ -x "$bin" ] || fail "$bin is missing: cargo build --release -p agentfs -p agentfs-csi"
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

# Mounts under the work directory (the fronts' sources and the bound targets).
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
trap cleanup EXIT

step "start agentfs-csi --mode all"
sudo "$AGENTFS_CSI_BIN" --mode all \
  --endpoint "unix://$SOCK" \
  --node-id sanity \
  --base-path "$W/base" \
  --node-state "$W/node" \
  --mount-dir "$W/mounts" \
  --profiles-dir "$W/profiles" \
  --agentfs-bin "$AGENTFS_BIN" \
  --passthrough "${AGENTFS_TEST_PASSTHROUGH:-auto}" \
  --stage-wait 30s --publish-wait 30s --unpublish-grace 10s --unstage-grace 15s \
  >"$LOG" 2>&1 &
DRIVER_PID=$(sudo sh -c 'for _ in $(seq 100); do p=$(pgrep -n -x agentfs-csi) && { echo "$p"; exit 0; }; sleep 0.1; done; exit 1') \
  || fail "agentfs-csi did not start: $(cat "$LOG")"
wait_for 30 sudo test -S "$SOCK" || fail "no socket at $SOCK: $(cat "$LOG")"
pass "driver listening on $SOCK (pid $DRIVER_PID)"

step "csi-sanity"
# No test is skipped. --csi.testvolumeexpandsize is unset (no expansion);
# the capability-gated tests (snapshots, clones, expansion, stats, list)
# skip themselves because the driver does not advertise them.
sudo "$CSI_SANITY" \
  --csi.endpoint "unix://$SOCK" \
  --csi.mountdir "$W/target" \
  --csi.stagingdir "$W/staging" \
  --csi.testvolumeparameters "$HERE/csi-sanity/params.yaml" \
  --csi.testvolumesize 1073741824 \
  --ginkgo.v \
  --ginkgo.no-color
pass "csi-sanity"

step "nothing left behind"
assert_eq 0 "$(agentfs_mounts)" "no agentfs mounts left"
left=$(sudo find "$W/node/published" -name '*.json' 2>/dev/null | wc -l)
assert_eq 0 "$left" "no published records left"
left=$(sudo find "$W/node/staged" -name '*.json' 2>/dev/null | wc -l)
assert_eq 0 "$left" "no staged records left"
left=$(sudo sh -c "ls '$W/base' 2>/dev/null | grep -v '^\.'" | wc -l)
assert_eq 0 "$left" "every volume directory deleted (onDelete: delete)"

step "stop the driver"
sudo kill -TERM "$DRIVER_PID"
for _ in $(seq 60); do sudo kill -0 "$DRIVER_PID" 2>/dev/null || break; sleep 0.5; done
sudo kill -0 "$DRIVER_PID" 2>/dev/null && fail "agentfs-csi did not stop on SIGTERM"
DRIVER_PID=
[ ! -S "$SOCK" ] || fail "the socket was left behind"
pass "stopped, socket removed"
