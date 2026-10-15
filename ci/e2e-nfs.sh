#!/usr/bin/env bash
# agentfs on NFS (design §14.2, §18 test 16): an NFS server on the runner
# exports one directory, mounted twice, by two NFSv4 clients: client A in
# the host's network namespace ("node-1") and client B in a network and
# UTS namespace of its own ("node-2"), each with its own client id, lease
# and locks, both in the host's mount namespace so FUSE fronts can use
# either view. Checks:
#
# a. the state lock holds across clients: a service on A, a second
#    service on B exits 75, a --standby on B takes over at A's SIGTERM;
# b. slot leases hold across clients, a kill -9 frees one, and a slot is
#    reclaimed with its predecessor's repairs;
# c. ci/e2e-multipod.sh with the service and front a on client A and
#    front b on client B, reaching the service over TLS through the
#    endpoint file: renames and removals across fronts, a file open in
#    one front changed in the other, a concurrent burst, an outage, kill
#    -9 of the service, live restores;
# d. passthrough (the job's matrix) with an NFS backing tree works or
#    falls back as documented, and the auto refusal count is printed;
#    a front's mmap writes and an SQLite single writer through a front;
# e. silly renames: a file removed while open through a front on NFS
#    never reaches history under its .nfs name;
# f. a partition: the active service on B is cut off from the server;
#    its heartbeat fences it, a standby on A takes over after the lease
#    expires, B's service exits 75 ("lost the state lock") after the
#    partition heals; every acknowledged close is in history, none
#    journaled twice;
# g. latencies into the step summary.
#
# GitHub ubuntu-24.04 runners only: it installs nfs-kernel-server and
# changes the host. Reproducible on a disposable Linux VM with /dev/fuse,
# root and the same packages:
#
#   AGENTFS_TEST_PASSTHROUGH=auto AGENTFS_BIN=target/release/agentfs ci/e2e-nfs.sh

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/nfs-lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"
[ -x "$AGENTFS_BIN" ] || fail "AGENTFS_BIN ($AGENTFS_BIN) is not built (with --features history-tls)"

HERE=$(cd "$(dirname "$0")" && pwd)
P=${AGENTFS_TEST_PASSTHROUGH:-auto}
EXPORT=${RUNNER_TEMP:-/tmp}/nfs-export
A=/mnt/nfs-a
B=/mnt/nfs-b
NS=nfsb
SERVER_IP=10.77.0.1
B_IP=10.77.0.2
LOGS=${RUNNER_TEMP:-/tmp}/agentfs-nfs-logs
mkdir -p "$LOGS"
FENCE=5s
PIDS=()

agentfs_as() { # node args...
  local node=$1; shift
  sudo env AGENTFS_NODE="$node" AGENTFS_REPLICA="svc-$node" "$AGENTFS_BIN" "$@"
}

cleanup() {
  status=$?
  if [ $status -ne 0 ]; then
    for log in "$LOGS"/*.log "$LOGS"/agentfs-multipod-logs/*.log; do
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

step "an NFS server and two clients of its export"
nfs_server "$EXPORT"
nfs_netns "$NS" "$B_IP" "$SERVER_IP"
nfs_mount "$SERVER_IP" "$A"
nfs_mount_in "$NS" "$SERVER_IP" "$B"
sudo touch "$A/probe"
wait_for 10 test -e "$B/probe" || fail "client B does not see client A's file"
assert_eq "nfs4" "$(findmnt -no FSTYPE "$A")" "client A is NFSv4"
assert_eq "nfs4" "$(findmnt -no FSTYPE "$B")" "client B is NFSv4"
[ "$(stat -c %d "$A")" != "$(stat -c %d "$B")" ] && pass "each client numbers the export with its own st_dev"
sudo rm "$A/probe"
CLIENTS=$(sudo cat /proc/fs/nfsd/clients/*/info 2>/dev/null | grep -c '^name:' || true)
echo "nfsd sees $CLIENTS client(s)"

new_nfs_volume() { # dir
  sudo rm -rf "$A/$1"
  sudo mkdir -p "$A/$1/tree"
  sudo "$AGENTFS_BIN" --state "$A/$1/state" init --volume "$1" >/dev/null
  sudo git --git-dir "$A/$1/state/git" config agentfs.commitWindow 200ms
  sudo git --git-dir "$A/$1/state/git" config agentfs.sweepInterval 1s
  wait_for 10 test -d "$B/$1/state/git" || fail "client B does not see $1"
}

serve_bg() { # node state tree log args... -> SERVE_PID
  local node=$1 state=$2 tree=$3 log=$4; shift 4
  sudo env AGENTFS_NODE="$node" AGENTFS_REPLICA="svc-$node" "$AGENTFS_BIN" --state "$state" \
    --root "$tree" serve-history --lock-fence "$FENCE" "$@" >>"$log" 2>&1 &
  SERVE_PID=$!
  PIDS+=("$SERVE_PID")
}

# The same, from client B's network namespace (a process there reaches
# the NFS server as client B; its files are client B's view).
serve_bg_b() { # state tree log args... -> SERVE_PID
  local state=$1 tree=$2 log=$3; shift 3
  sudo ip netns exec "$NS" env AGENTFS_NODE=node-2 AGENTFS_REPLICA=svc-node-2 "$AGENTFS_BIN" \
    --state "$state" --root "$tree" serve-history --lock-fence "$FENCE" "$@" >>"$log" 2>&1 &
  SERVE_PID=$!
  PIDS+=("$SERVE_PID")
}

kill_tree() { # pattern
  sudo pkill -KILL -f -- "$1" || true
}

step "a. the state lock across clients"
new_nfs_volume lockvol
L=$LOGS/lock-a.log
serve_bg node-1 "$A/lockvol/state" "$A/lockvol/tree" "$L"
SVC_A=$SERVE_PID
wait_for 30 grep -q '^serving history' "$L" || fail "the service on A never served"
grep -q "state lock heartbeat" "$L" || fail "no heartbeat on NFS with --lock-heartbeat auto"
pass "the service on client A holds the lock, with its heartbeat (auto on NFS)"
set +e
OUT=$(sudo ip netns exec "$NS" env AGENTFS_NODE=node-2 "$AGENTFS_BIN" --state "$B/lockvol/state" \
  --root "$B/lockvol/tree" serve-history 2>&1)
CODE=$?
set -e
echo "$OUT" | tail -n 3
assert_eq 75 "$CODE" "a second service on client B exits 75"
assert_contains "$OUT" "node=node-1" "naming the holder's node"
LB=$LOGS/lock-b.log
serve_bg_b "$B/lockvol/state" "$B/lockvol/tree" "$LB" --standby
SVC_B=$SERVE_PID
wait_for 30 grep -q 'standby for volume lockvol' "$LB" || fail "no standby on B"
STATUS=$(sudo "$AGENTFS_BIN" --state "$A/lockvol/state" status)
assert_contains "$STATUS" "node=node-1 command=serve-history" "status names the active node"
sudo kill -TERM "$SVC_A"
wait "$SVC_A" || fail "the service on A did not stop cleanly"
T0=$SECONDS
wait_for 30 grep -q '^serving history' "$LB" || fail "the standby on B never took over"
pass "the standby on client B took over in $((SECONDS - T0)) s"
assert_contains "$(sudo cat "$A/lockvol/state/meta/history/endpoint.json")" '"node-2"' \
  "the endpoint names node-2"
sudo ip netns exec "$NS" kill -TERM "$SVC_B" 2>/dev/null || sudo kill -TERM "$SVC_B"
wait "$SVC_B" || true

step "b. slot leases across clients"
new_nfs_volume leasevol
L=$LOGS/lease-svc.log
serve_bg node-1 "$A/leasevol/state" "$A/leasevol/tree" "$L" --listen-tls "$SERVER_IP:0"
SVC=$SERVE_PID
wait_for 30 grep -q '^serving history' "$L" || fail "never served"
# Two ping-history fronts hold leases at once, one per client: distinct
# slots. Long runs, so both are seen; then short timed runs.
holders() { sudo "$AGENTFS_BIN" --state "$A/leasevol/state" status | grep 'command=ping-history' || true; }
sudo env AGENTFS_NODE=node-1 "$AGENTFS_BIN" --state "$A/leasevol/state" ping-history \
  --count 100000000 >/dev/null 2>&1 &
sudo ip netns exec "$NS" env AGENTFS_NODE=node-2 "$AGENTFS_BIN" --state "$B/leasevol/state" \
  ping-history --count 100000000 >"$LOGS/ping-long-b.out" 2>&1 &
wait_for 30 sh -c "[ \$(sudo $AGENTFS_BIN --state $A/leasevol/state status | grep -c 'command=ping-history') -ge 2 ]" ||
  fail "two leases were never held at once: $(holders); B: $(tail -n 3 "$LOGS/ping-long-b.out")"
HOLDERS=$(holders)
echo "$HOLDERS"
assert_contains "$HOLDERS" "node=node-1" "a lease from client A"
assert_contains "$HOLDERS" "node=node-2" "a lease from client B"
[ "$(echo "$HOLDERS" | sed -n 's/^fronts:    slot \([0-9]*\):.*/\1/p' | sort -u | wc -l)" = 2 ] ||
  fail "the two fronts share a slot"
pass "two clients lease distinct slots"
kill_tree "[p]ing-history --count 100000000"
wait_for 30 sh -c "! sudo $AGENTFS_BIN --state $A/leasevol/state status | grep -q 'command=ping-history'" ||
  fail "killed fronts' leases were never freed: $(holders)"
pass "a kill -9 frees the leases, on both clients"
PING_A=$(sudo env AGENTFS_NODE=node-1 "$AGENTFS_BIN" --state "$A/leasevol/state" ping-history --count 3000)
PING_B=$(sudo ip netns exec "$NS" env AGENTFS_NODE=node-2 "$AGENTFS_BIN" --state "$B/leasevol/state" \
  ping-history --count 3000)
echo "$PING_A"
echo "$PING_B"
assert_contains "$PING_B" "ping state from node node-2" "client B reached the service over TLS"
sudo kill -TERM "$SVC"
wait "$SVC" || fail "the lease volume's service did not stop cleanly"

step "c. two FUSE fronts on two NFS clients (ci/e2e-multipod.sh)"
sudo rm -rf "$A/multipod"
sudo mkdir -p "$A/multipod"
MULTIPOD_DIR_A=$A/multipod MULTIPOD_DIR_B=$B/multipod FRONT_B_TRANSPORT=tls \
  AGENTFS_LOCK_FENCE=$FENCE AGENTFS_TEST_PASSTHROUGH=$P BOUNDARY_N=${BOUNDARY_N:-500} \
  RUNNER_TEMP=$LOGS "$HERE/e2e-multipod.sh"
pass "the multi-pod checks hold with the tree and state on NFS"
REFUSED=$(grep -h -o 'passthrough: [^)]*' "$LOGS"/agentfs-multipod-logs/front-*.log 2>/dev/null | sort | uniq -c || true)
echo "passthrough with an NFS backing tree ($P): $REFUSED"

step "d, e. passthrough, mmap, SQLite and silly renames through a front on NFS"
new_nfs_volume fsvol
L=$LOGS/fs-svc.log
F=$LOGS/fs-front.log
MNT=$(mktemp -d)
chmod 755 "$MNT"
serve_bg node-1 "$A/fsvol/state" "$A/fsvol/tree" "$L"
SVC=$SERVE_PID
wait_for 30 grep -q '^serving history' "$L" || fail "never served"
sudo chown 10000:10000 "$A/fsvol/tree"
sudo env AGENTFS_NODE=node-1 AGENTFS_POD=pod-fs "$AGENTFS_BIN" --state "$A/fsvol/state" mount \
  --passthrough "$P" --history-service state "$A/fsvol/tree" "$MNT" >>"$F" 2>&1 &
FRONT=$!
PIDS+=("$FRONT")
wait_for 30 mountpoint -q "$MNT" || fail "the front never mounted"
grep '^mounted' "$F"
as10000() { sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- "$@"; }
# mmap writes through the front, recorded at close.
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
wait_for 30 sh -c "sudo git --git-dir $A/fsvol/state/git show refs/heads/agentfs/fsvol:mapped.bin 2>/dev/null | head -c 5 | grep -q hello" ||
  fail "the mmap write never reached history"
pass "an mmap write through a front on NFS is recorded"
# One SQLite writer through the front (single writer across nodes, §14.1).
as10000 python3 - "$MNT/app.db" <<'PY'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute("create table t (n integer)")
for i in range(50):
    db.execute("insert into t values (?)", (i,))
    db.commit()
db.close()
PY
wait_for 30 sh -c "sudo git --git-dir $A/fsvol/state/git cat-file -e refs/heads/agentfs/fsvol:app.db" ||
  fail "app.db never reached history"
sudo git --git-dir "$A/fsvol/state/git" show refs/heads/agentfs/fsvol:app.db >"$LOGS/app.db"
assert_eq ok "$(python3 -c "import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute('pragma integrity_check').fetchone()[0])" "$LOGS/app.db")" \
  "the recorded database passes integrity_check"
# Silly rename: a file open through the front is removed through the
# backing tree on client A's view (as another node's pod would): the NFS
# client renames it to .nfsXXXX until it is closed.
as10000 sh -c "printf held > '$MNT/held.txt'"
wait_for 30 sh -c "sudo git --git-dir $A/fsvol/state/git cat-file -e refs/heads/agentfs/fsvol:held.txt" ||
  fail "held.txt never reached history"
as10000 python3 - "$MNT/held.txt" "$A/fsvol/tree/held.txt" <<'PY' &
import os, sys, time
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_APPEND)
time.sleep(1)
os.write(fd, b" more")
time.sleep(4)
os.close(fd)
PY
HOLDER=$!
sleep 0.5
sudo rm "$A/fsvol/tree/held.txt"
SILLY=$(sudo ls -a "$A/fsvol/tree" | grep '^\.nfs' || true)
echo "backing tree while held open: ${SILLY:-no .nfs file}"
wait "$HOLDER" || true
sleep 3
NFSNAMES=$(sudo git --git-dir "$A/fsvol/state/git" log --name-only --format= refs/heads/agentfs/fsvol | grep '\.nfs' || true)
assert_eq "" "$NFSNAMES" "no .nfs name ever reached history"
sudo kill -TERM "$FRONT"
wait "$FRONT" || fail "the front did not stop cleanly"
sudo kill -TERM "$SVC"
wait "$SVC" || fail "the service did not stop cleanly"

step "f. a partition: the active service on client B is fenced, a standby on A takes over"
new_nfs_volume partvol
LB=$LOGS/part-b.log
LA=$LOGS/part-a.log
serve_bg_b "$B/partvol/state" "$B/partvol/tree" "$LB" --listen-tls "$B_IP:0"
SVC_B=$SERVE_PID
wait_for 30 grep -q '^serving history' "$LB" || fail "the service on B never served"
serve_bg node-1 "$A/partvol/state" "$A/partvol/tree" "$LA" --standby --listen-tls "$SERVER_IP:0"
SVC_A=$SERVE_PID
wait_for 30 grep -q 'standby for volume partvol' "$LA" || fail "no standby on A"
# A writer on node-1 records through `state` and notes every close that
# returned success.
LEDGER=$LOGS/part-ledger
: >"$LEDGER"
(
  for i in $(seq 1 400); do
    printf "%s" "$i" | sudo tee "$A/partvol/tree/f-$i" >/dev/null
    if sudo env AGENTFS_NODE=node-1 "$AGENTFS_BIN" --state "$A/partvol/state" --root "$A/partvol/tree" \
      record --history-service state --history-timeout 2s --history-wait 1s "f-$i" >/dev/null 2>&1; then
      echo "$i" >>"$LEDGER"
    fi
    sleep 0.1
  done
) &
WRITER=$!
sleep 5
nfs_partition "$NS"
PART=$SECONDS
echo "partitioned client B at $(date -u +%T)"
wait_for 120 grep -q '^serving history' "$LA" || fail "the standby on A never took over"
TAKEOVER=$((SECONDS - PART))
pass "the standby on A took over $TAKEOVER s after the partition"
sleep 10
nfs_heal "$NS"
echo "healed at $(date -u +%T)"
wait_for 120 grep -q 'lost the state lock' "$LB" || fail "B's service never said it lost the lock"
set +e
wait "$SVC_B"
CODE=$?
set -e
assert_eq 75 "$CODE" "B's service exits 75"
grep -q '^final commit:' "$LB" && fail "B's service made a final commit after losing the lock"
pass "no final commit from the fenced service"
wait "$WRITER" || true
sudo kill -TERM "$SVC_A"
wait "$SVC_A" || fail "A's service did not stop cleanly"
ACKED=$(wc -l <"$LEDGER")
MISSING=0
for i in $(cat "$LEDGER"); do
  got=$(sudo git --git-dir "$A/partvol/state/git" show "refs/heads/agentfs/partvol:f-$i" 2>/dev/null || true)
  [ "$got" = "$i" ] || { echo "missing f-$i"; MISSING=$((MISSING + 1)); }
done
assert_eq 0 "$MISSING" "all $ACKED acknowledged closes are in history"
SEQS=$(sudo git --git-dir "$A/partvol/state/git" log --format='%(trailers:key=Agentfs-Seq,valueonly)' refs/heads/agentfs/partvol | grep -v '^$' | sort | uniq -d)
assert_eq "" "$SEQS" "no sequence number committed twice"
sudo git --git-dir "$A/partvol/state/git" fsck --no-dangling >/dev/null 2>&1 || fail "git fsck failed"
pass "the history is sound"

step "g. latencies"
{
  echo "### agentfs on NFS (passthrough $P)"
  echo
  echo "Kernel $(uname -r); NFS $NFS_OPTS; nfsd lease $(sudo cat /proc/fs/nfsd/nfsv4leasetime) s; fence $FENCE."
  echo
  echo "- ping-history, client A (unix): $PING_A"
  echo "- ping-history, client B (TLS over a veth): $PING_B"
  echo "- partition: standby took over after $TAKEOVER s; $ACKED acknowledged closes, none lost"
} | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}" "$LOGS/summary.md"
notice_summary "NFS latency ($P)" "$(cat "$LOGS/summary.md")"
pass "NFS e2e done"
