#!/usr/bin/env bash
# agentfs on NFS, one node at a time (design §14.2, §18 test 16): an NFS
# server on the runner exports one directory, mounted by two NFSv4
# clients: client A in the host's network namespace ("node-1") and client
# B in a network and UTS namespace of its own ("node-2"), each with its
# own client id and locks, both in the host's mount namespace. A volume
# is used from one node at a time; it may move between nodes on the
# shared export. Each claim is one assertion:
#
# a. B sees A's writes, and nfsd counts two clients;
# b. with serve-history serving on A, a second serve-history on B's view
#    exits 75 naming command=serve-history (the holder line reached the
#    other client);
# c. on the export mounted again with vers=4.0, and with
#    local_lock=flock, serve-history exits 1 naming the reason, and so
#    does agentfs-csi with its base path there, before serving (a kernel
#    that will not show local_lock on NFSv4 makes that half SKIP: NOT RUN);
# d. a front on A over the NFS tree: an mmap write is recorded; 50 SQLite
#    commits are recorded and pass integrity_check;
# e. a file held open through the front and removed through it leaves a
#    .nfs name in the backing directory; the directory is renamed through
#    the front; no .nfs name ever reaches history, and the directory's
#    other file is at its new path;
# f. DeleteVolume on a base path on the export (agentfs-csi's ignored
#    grpc test delete_volume_on_a_shared_base_path) leaves nothing behind.
#
# GitHub ubuntu-24.04 runners only: it installs nfs-kernel-server and
# changes the host. Reproducible on a disposable Linux VM with /dev/fuse,
# root and the same packages:
#
#   AGENTFS_TEST_PASSTHROUGH=auto AGENTFS_BIN=target/release/agentfs \
#     AGENTFS_CSI_BIN=target/release/agentfs-csi ci/e2e-nfs.sh

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/nfs-lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"
[ -x "$AGENTFS_BIN" ] || fail "AGENTFS_BIN ($AGENTFS_BIN) is not built"
AGENTFS_CSI_BIN=${AGENTFS_CSI_BIN:-$(dirname "$AGENTFS_BIN")/agentfs-csi}
[ -x "$AGENTFS_CSI_BIN" ] || fail "AGENTFS_CSI_BIN ($AGENTFS_CSI_BIN) is not built"

HERE=$(cd "$(dirname "$0")" && pwd)
P=${AGENTFS_TEST_PASSTHROUGH:-auto}
EXPORT=${RUNNER_TEMP:-/tmp}/nfs-export
A=/mnt/nfs-a
B=/mnt/nfs-b
V40=/mnt/nfs-v40
FLOCK=/mnt/nfs-flock
NS=nfsb
SERVER_IP=10.77.0.1
B_IP=10.77.0.2
LOGS=${RUNNER_TEMP:-/tmp}/agentfs-nfs-logs
mkdir -p "$LOGS"
PIDS=()
NOT_RUN=()

cleanup() {
  status=$?
  if [ $status -ne 0 ]; then
    for log in "$LOGS"/*.log; do
      [ -f "$log" ] || continue
      echo "---- $log"
      tail -n 60 "$log" 2>/dev/null || true
      annotate_log "$log"
    done
  fi
  for pid in "${PIDS[@]}"; do sudo kill -KILL "$pid" 2>/dev/null || true; done
  sudo sh -c 'grep agentfs /proc/self/mountinfo | cut -d" " -f5 | xargs -r -n1 umount -l' 2>/dev/null || true
  nfs_cleanup
  exit $status
}
trap cleanup EXIT
# Every step's output, for annotations if a command fails without `fail`.
exec > >(tee "$LOGS/e2e-nfs.out.log") 2>&1
trap 'annotate_log "$LOGS/e2e-nfs.out.log" 25' ERR

new_nfs_volume() { # dir
  sudo rm -rf "$A/$1"
  sudo mkdir -p "$A/$1/tree"
  sudo "$AGENTFS_BIN" --state "$A/$1/state" init --volume "$1" >/dev/null
  sudo git --git-dir "$A/$1/state/git" config agentfs.commitWindow 200ms
  sudo git --git-dir "$A/$1/state/git" config agentfs.sweepInterval 1s
  wait_for 10 test -d "$B/$1/state/git" || fail "client B does not see $1"
}

serve_bg() { # state tree log -> SERVE_PID
  sudo env AGENTFS_REPLICA=history-0 "$AGENTFS_BIN" --state "$1" --root "$2" \
    serve-history >>"$3" 2>&1 &
  SERVE_PID=$!
  PIDS+=("$SERVE_PID")
  wait_for 30 grep -q '^serving history' "$3" || fail "the service on $1 never served"
}

# Runs a command that must exit 1 before serving, naming `reason`.
refused() { # what reason command...
  local what=$1 reason=$2 out code
  shift 2
  set +e
  out=$(timeout 15 "$@" 2>&1)
  code=$?
  set -e
  echo "$out" | tail -n 4
  assert_eq 1 "$code" "$what exits 1"
  assert_contains "$out" "$reason" "$what names $reason"
}

step "a. an NFS server and two clients of its export"
nfs_server "$EXPORT"
nfs_netns "$NS" "$B_IP" "$SERVER_IP"
nfs_mount "$SERVER_IP" "$A"
nfs_mount_in "$NS" "$SERVER_IP" "$B"
sudo sh -c "printf from-a > '$A/probe'"
wait_for 10 sh -c "[ \"\$(cat '$B/probe' 2>/dev/null)\" = from-a ]" ||
  fail "client B does not see client A's write"
pass "client B sees client A's write"
sudo rm "$A/probe"
CLIENTS=$(sudo cat /proc/fs/nfsd/clients/*/info 2>/dev/null | grep -c '^name:' || true)
[ "$CLIENTS" -ge 2 ] || fail "nfsd counts $CLIENTS client(s), not two"
pass "nfsd counts $CLIENTS clients"

step "b. the state lock and its holder line across clients"
new_nfs_volume lockvol
L=$LOGS/lock-a.log
serve_bg "$A/lockvol/state" "$A/lockvol/tree" "$L"
SVC=$SERVE_PID
set +e
OUT=$(sudo ip netns exec "$NS" timeout 15 "$AGENTFS_BIN" --state "$B/lockvol/state" \
  --root "$B/lockvol/tree" serve-history 2>&1)
CODE=$?
set -e
echo "$OUT" | tail -n 3
assert_eq 75 "$CODE" "a second service on client B exits 75"
assert_contains "$OUT" "command=serve-history" "naming the holder client A wrote"
sudo kill -TERM "$SVC"
wait "$SVC" || fail "the service on A did not stop cleanly"

step "c. unsafe NFS mounts are refused at start"
CSI_TMP=$(mktemp -d)
check_refused() { # mnt reason
  refused "serve-history on $1" "$2" \
    sudo "$AGENTFS_BIN" --state "$1/lockvol/state" --root "$1/lockvol/tree" serve-history
  refused "agentfs-csi with its base path on $1" "$2" \
    sudo "$AGENTFS_CSI_BIN" --mode controller --base-path "$1/csi" \
    --profiles-dir "$CSI_TMP" --endpoint "unix://$CSI_TMP/csi.sock"
  [ ! -e "$CSI_TMP/csi.sock" ] || fail "agentfs-csi served with its base path on $1"
}
nfs_mount "$SERVER_IP" "$V40" vers=4.0,hard,nosharecache,timeo=50,retrans=2
assert_contains "$(findmnt -no OPTIONS "$V40")" "vers=4.0" "the second mount is NFS 4.0"
check_refused "$V40" "NFS version 4.0"
if ! nfs_mount "$SERVER_IP" "$FLOCK" vers=4.2,local_lock=flock,hard,nosharecache,timeo=50,retrans=2; then
  echo "SKIP local_lock: the kernel refused local_lock=flock on NFSv4"
  NOT_RUN+=("local_lock=flock: the kernel refused the mount")
elif ! findmnt -no OPTIONS "$FLOCK" | grep -q 'local_lock=flock'; then
  echo "SKIP local_lock: the kernel dropped it: $(findmnt -no OPTIONS "$FLOCK")"
  NOT_RUN+=("local_lock=flock: the kernel dropped the option")
else
  check_refused "$FLOCK" "local_lock=flock"
fi

step "d. a front on client A over the NFS tree: mmap and SQLite"
new_nfs_volume fsvol
L=$LOGS/fs-svc.log
F=$LOGS/fs-front.log
MNT=$(mktemp -d)
chmod 755 "$MNT"
serve_bg "$A/fsvol/state" "$A/fsvol/tree" "$L"
SVC=$SERVE_PID
sudo chown 10000:10000 "$A/fsvol/tree"
sudo env AGENTFS_POD=pod-fs AGENTFS_REPLICA=pod-fs "$AGENTFS_BIN" --state "$A/fsvol/state" mount \
  --passthrough "$P" --history-service "$A/fsvol/state/meta/history/history.sock" \
  "$A/fsvol/tree" "$MNT" >>"$F" 2>&1 &
FRONT=$!
PIDS+=("$FRONT")
wait_for 30 mountpoint -q "$MNT" || fail "the front never mounted"
grep '^mounted' "$F" || true
fsgit() { sudo git --git-dir "$A/fsvol/state/git" "$@"; }
in_history() { fsgit cat-file -e "refs/heads/agentfs/fsvol:$1" 2>/dev/null; }
as10000() { sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- "$@"; }
backlog_drained() { sudo "$AGENTFS_BIN" --state "$A/fsvol/state" status | grep -Eq '^backlog: +0 record'; }
as10000 python3 - "$MNT/mapped.bin" <<'PY'
import mmap, os, sys
fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, 4096)
m = mmap.mmap(fd, 4096)
m[:5] = b"hello"
m.flush()
m.close()
os.close(fd)
PY
mapped() { [ "$(fsgit show refs/heads/agentfs/fsvol:mapped.bin 2>/dev/null | head -c 5)" = hello ]; }
wait_for 30 mapped || fail "the mmap write never reached history"
pass "an mmap write through a front on NFS is recorded"
as10000 python3 - "$MNT/app.db" <<'PY'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute("create table t (n integer)")
for i in range(50):
    db.execute("insert into t values (?)", (i,))
    db.commit()
db.close()
PY
DB=$LOGS/app.db
db_query() { python3 -c 'import sqlite3, sys; print(sqlite3.connect(sys.argv[1]).execute(sys.argv[2]).fetchone()[0])' "$DB" "$1"; }
fifty_rows() { fsgit show refs/heads/agentfs/fsvol:app.db >"$DB" 2>/dev/null && [ "$(db_query 'select count(*) from t')" = 50 ]; }
wait_for 30 fifty_rows || fail "the 50 SQLite commits never reached history"
assert_eq ok "$(db_query 'pragma integrity_check')" \
  "the recorded database holds 50 rows and passes integrity_check"

step "e. silly renames through a front never reach history"
as10000 sh -c "mkdir '$MNT/dir' && printf held > '$MNT/dir/held.txt' && printf other > '$MNT/dir/other.txt'"
wait_for 30 in_history dir/other.txt || fail "dir/other.txt never reached history"
SYNC=$(mktemp -d)
chmod 777 "$SYNC"
as10000 python3 - "$MNT/dir/held.txt" "$SYNC/ready" "$SYNC/release" <<'PY' &
import os, sys, time
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_APPEND)
open(sys.argv[2], "w").close()
while not os.path.exists(sys.argv[3]):
    time.sleep(0.1)
os.write(fd, b" more")
os.close(fd)
PY
HOLDER=$!
wait_for 30 test -e "$SYNC/ready" || fail "the holder never opened held.txt"
as10000 rm "$MNT/dir/held.txt"
SILLY=$(sudo ls -a "$A/fsvol/tree/dir" | grep '^\.nfs' || true)
[ -n "$SILLY" ] || fail "no .nfs name in the backing directory while held.txt is open: $(sudo ls -a "$A/fsvol/tree/dir")"
pass "the backing directory holds $SILLY while held.txt is open"
as10000 mv "$MNT/dir" "$MNT/moved"
touch "$SYNC/release"
wait "$HOLDER" || fail "the holder failed"
wait_for 30 in_history moved/other.txt || fail "moved/other.txt never reached history"
wait_for 30 backlog_drained ||
  fail "the backlog never drained: $(sudo "$AGENTFS_BIN" --state "$A/fsvol/state" status 2>&1)"
NFSNAMES=$(fsgit log --name-only --format= refs/heads/agentfs/fsvol | grep '\.nfs' || true)
assert_eq "" "$NFSNAMES" "no .nfs name ever reached history"
assert_eq other "$(fsgit show refs/heads/agentfs/fsvol:moved/other.txt)" \
  "the directory's other file is at its new path"
! in_history dir/other.txt || fail "dir/other.txt is still in history after the move"
sudo kill -TERM "$FRONT"
wait "$FRONT" || fail "the front did not stop cleanly"
sudo kill -TERM "$SVC"
wait "$SVC" || fail "the service did not stop cleanly"

step "f. DeleteVolume on a base path on the export"
sudo mkdir -p "$A/csi"
sudo chown "$(id -u):$(id -g)" "$A/csi"
(cd "$HERE/.." && AGENTFS_CSI_TEST_BASE=$A/csi cargo test --locked --release -p agentfs-csi \
  --test grpc -- --ignored --exact delete_volume_on_a_shared_base_path)
pass "DeleteVolume on NFS leaves neither the volume nor a .nfs name"

for what in "${NOT_RUN[@]}"; do
  echo "NOT RUN: $what"
  [ -z "${GITHUB_ACTIONS:-}" ] || printf '::warning title=e2e-nfs NOT RUN::%s\n' "$what"
done
pass "NFS e2e done (passthrough $P)"
