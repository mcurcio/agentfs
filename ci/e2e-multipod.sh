#!/usr/bin/env bash
# End-to-end multi-pod at single-node level (design §14, §18 test 16): two
# real FUSE front mounts of one backing tree, as pods pod-a and pod-b,
# feeding one `agentfs serve-history`. Checks that every commit carries the
# committer and pod trailers of the mount that made the change, that a
# service outage fails the boundary with EIO and an alarm and that history
# catches up once it is back (front a reconnecting by itself), that a
# close a front failed before it died commits as that front's pod when
# another pod's front takes over its slot, that a file
# open in one front and renamed or removed through the other is recorded
# under its new name or not at all, that a directory rename through a
# front is one move the service reconciles (a file held open across it in
# the other front lands at its new path), that absolute gitdir targets map
# exactly through the fronts' workload mounts, that a concurrent burst on one path
# from both fronts converges on the tree's last state, that both fronts
# creating the same new names at once never fail (each creates as the
# caller), that a restore and a revert served by the service reach both
# mounts at once (and refuse a path open for writing in either), that a
# change whose close returned (journaled,
# not yet committed: commitWindow is raised for it) survives a
# kill -9 of the service, and that the service pushes. Then measures the
# per-close cost, single-process against front (ci/boundary-latency.py).
#
# With MULTIPOD_DIR_A and MULTIPOD_DIR_B (one directory through two
# clients of a shared filesystem), front b mounts the second client's view
# (ci/e2e-nfs.sh runs it so over NFS, design §14.2).
#
# With FRONT_B_TRANSPORT=tls (a binary built with the history-tls
# feature), the service also listens on loopback TLS and front b runs as
# if on another node (AGENTFS_NODE=node-2, --history-service state): it
# finds the service through the endpoint file and reaches it over TLS
# (design §14.1), and every check above holds across the transports; the
# per-close cost then also measures a TLS front.
#
# Needs Linux with /dev/fuse, fuse3, git, python3, setpriv and passwordless
# sudo; reproducible on any such host:
#
#   AGENTFS_TEST_PASSTHROUGH=auto ci/e2e-multipod.sh
#   AGENTFS_TEST_PASSTHROUGH=auto FRONT_B_TRANSPORT=tls ci/e2e-multipod.sh

. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"
command -v python3 >/dev/null || fail "this test needs python3"

if [ ! -x "$AGENTFS_BIN" ]; then
  step "build"
  cargo build --locked -p agentfs
fi

HERE=$(cd "$(dirname "$0")" && pwd)
P=${AGENTFS_TEST_PASSTHROUGH:-auto}
T=${FRONT_B_TRANSPORT:-unix}
case $T in
  unix) TLS_ARGS=() ;;
  tls) TLS_ARGS=(--listen-tls 127.0.0.1:0) ;;
  *) fail "FRONT_B_TRANSPORT is unix or tls, not $T" ;;
esac
W=$(mktemp -d)
chmod 755 "$W"
STATE=$W/state
BACKING=$W/backing
# MULTIPOD_DIR_A and MULTIPOD_DIR_B: one directory seen through two
# clients of one shared filesystem (ci/e2e-nfs.sh: two NFS clients of one
# export). The service and front a use A's view, front b B's.
if [ -n "${MULTIPOD_DIR_A:-}" ]; then
  [ -n "${MULTIPOD_DIR_B:-}" ] || fail "MULTIPOD_DIR_A needs MULTIPOD_DIR_B"
  sudo mkdir -p "$MULTIPOD_DIR_A/backing"
  STATE=$MULTIPOD_DIR_A/state
  BACKING=$MULTIPOD_DIR_A/backing
  STATE_B=$MULTIPOD_DIR_B/state
  BACKING_B=$MULTIPOD_DIR_B/backing
  wait_for 30 test -d "$BACKING_B" || fail "$BACKING_B is not visible through the second client"
else
  STATE_B=$STATE
  BACKING_B=$BACKING
fi
MNT_A=$W/mnt-a
MNT_B=$W/mnt-b
REMOTE=$W/remote.git
SOCK=$STATE/meta/history/history.sock
LOGS=${RUNNER_TEMP:-$W}/agentfs-multipod-logs
sudo mkdir -p "$BACKING"
mkdir -p "$MNT_A" "$MNT_B" "$LOGS"
sudo chmod 755 "$BACKING"
chmod 755 "$MNT_A" "$MNT_B"
SVC_LOG=$LOGS/service-$P-$T.log
A_LOG=$LOGS/front-a-$P-$T.log
B_LOG=$LOGS/front-b-$P-$T.log
SP_LOG=$LOGS/single-process-$P-$T.log
: >"$SVC_LOG"
: >"$A_LOG"
: >"$B_LOG"
: >"$SP_LOG"
SVC_PID=
A_PID=
B_PID=
SP_PID=

sgit() { sudo git --git-dir "$STATE/git" "$@"; }
as_agent() { # dir script: runs as uid 10000, like the workload
  sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- sh -c "cd '$1' && $2"
}
head_of() { sgit rev-parse refs/heads/agentfs/demo; }
cleanup() {
  status=$?
  if [ $status -ne 0 ]; then
    for log in "$SVC_LOG" "$A_LOG" "$B_LOG" "$SP_LOG" "$OUT_LOG"; do
      echo "---- $log"
      tail -n 60 "$log" 2>/dev/null || true
      annotate_log "$log" 20
    done
  fi
  for pid in $A_PID $B_PID $SP_PID $SVC_PID; do
    sudo kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in $A_PID $B_PID $SP_PID $SVC_PID; do
    wait "$pid" 2>/dev/null || true
  done
  for m in "$MNT_A" "$MNT_B"; do
    mountpoint -q "$m" && sudo umount -l "$m"
  done
  exit $status
}
trap cleanup EXIT
# This script's own output, for an annotation when a command fails
# without `fail`.
OUT_LOG=$LOGS/e2e-multipod-$P-$T.out.log
exec > >(tee "$OUT_LOG") 2>&1

start_service() {
  echo "---- serve-history" >>"$SVC_LOG"
  local before
  before=$(grep -c '^serving history' "$SVC_LOG" || true)
  sudo env AGENTFS_REPLICA=history-0 AGENTFS_NODE=node-1 AGENTFS_LOCK_FENCE="${AGENTFS_LOCK_FENCE:-10s}" "$AGENTFS_BIN" --state "$STATE" \
    --root "$BACKING" serve-history "${TLS_ARGS[@]}" >>"$SVC_LOG" 2>&1 &
  SVC_PID=$!
  wait_for 30 sh -c "[ \$(grep -c '^serving history' '$SVC_LOG') -gt $before ]" ||
    fail "the history service never served"
}

stop_service() {
  sudo kill -TERM "$SVC_PID"
  wait "$SVC_PID" || fail "serve-history did not exit cleanly"
  SVC_PID=
  grep -q "final commit" "$SVC_LOG" || fail "no final commit line"
  pass "serve-history stopped cleanly"
}

# Kills the service with SIGKILL (sudo does not relay it, so the binary
# itself is signalled).
kill_service() {
  # [s]: the pattern must not match this pkill's own sudo.
  sudo pkill -KILL -f -- "--root $BACKING [s]erve-history" || true
  wait "$SVC_PID" 2>/dev/null || true
  SVC_PID=
}

# A front on node-1 reaches the service at its socket; with
# FRONT_ADDR=state FRONT_NODE=node-2 it is "on another node" and finds the
# service through the endpoint file, over TLS.
start_front() { # pod mountpoint log -> sets FRONT_PID
  local before state=$STATE backing=$BACKING
  if [ "$2" = "$MNT_B" ]; then state=$STATE_B backing=$BACKING_B; fi
  before=$(grep -c '^mounted ' "$3" || true)
  sudo env ${FRONT_SLOT:+AGENTFS_HISTORY_SLOT=$FRONT_SLOT} AGENTFS_WORKLOAD_MOUNTS=/workspace AGENTFS_POD="$1" AGENTFS_POD_UID="$1-uid" AGENTFS_NODE="${FRONT_NODE:-node-1}" \
    AGENTFS_NAMESPACE=agents AGENTFS_REPLICA="$1" \
    "$AGENTFS_BIN" --state "$state" mount --passthrough "$P" --unmount-grace 5s \
    --history-service "${FRONT_ADDR:-$SOCK}" --history-timeout 5s --history-wait 30s \
    "$backing" "$2" >>"$3" 2>&1 &
  FRONT_PID=$!
  wait_for 30 mountpoint -q "$2" || fail "$1 never mounted"
  wait_for 30 sh -c "[ \$(grep -c '^mounted ' '$3') -gt $before ]" || fail "$1: no startup line"
}

stop_front() { # pid name
  sudo kill -TERM "$1"
  wait "$1" || fail "front $2 did not exit cleanly"
  pass "front $2 stopped cleanly"
}

# Waits until the branch holds `content` at `path` (or, with content '-',
# until it holds nothing there).
wait_history() { # path content
  if [ "$2" = "-" ]; then
    wait_for 30 sh -c "! sudo git --git-dir '$STATE/git' cat-file -e refs/heads/agentfs/demo:$1" ||
      fail "history still has $1"
  else
    wait_for 30 sh -c "[ \"\$(sudo git --git-dir '$STATE/git' show refs/heads/agentfs/demo:$1 2>/dev/null)\" = '$2' ]" ||
      fail "history never had $1 = $2"
  fi
  pass "history: $1 = $2"
}

# The committer of the commit that last changed `path`.
committer_of() { # path
  sgit log -1 --format=%cn refs/heads/agentfs/demo -- "$1"
}

step "a volume with a remote"
sudo "$AGENTFS_BIN" --state "$STATE" init --volume demo >/dev/null
sgit config agentfs.commitWindow 200ms
sgit config agentfs.sweepInterval 1s
sgit config agentfs.pushInterval 1s
sudo git init -q --bare "$REMOTE"
sgit config remote.origin.url "$REMOTE"

step "a front without a service exits without mounting"
set +e
OUT=$(sudo "$AGENTFS_BIN" --state "$STATE" mount --history-service "$SOCK" --history-wait 1s \
  "$BACKING" "$MNT_A" 2>&1)
CODE=$?
set -e
echo "$OUT"
assert_eq 1 "$CODE" "exit status 1"
assert_contains "$OUT" "agentfs serve-history" "the error names the fix"
mountpoint -q "$MNT_A" && fail "mounted without a service"

step "one service, two fronts over one backing tree"
start_service
start_front pod-a "$MNT_A" "$A_LOG"
A_PID=$FRONT_PID
if [ "$T" = tls ]; then
  FRONT_ADDR=state FRONT_NODE=node-2 start_front pod-b "$MNT_B" "$B_LOG"
  grep -q "front of history service state" "$B_LOG" || fail "front b is not a front of state"
else
  start_front pod-b "$MNT_B" "$B_LOG"
fi
B_PID=$FRONT_PID
sudo chown 10000:10000 "$MNT_A"
# Through another client of a shared filesystem, the owner shows once its
# attribute cache expires (NFS: up to acdirmax, 60 s).
wait_for 90 sh -c "[ \$(stat -c %u '$MNT_B') = 10000 ]" ||
  fail "front b never saw the tree's new owner"
grep '^mounted ' "$A_LOG"
STATUS=$(sudo "$AGENTFS_BIN" --state "$STATE" status)
echo "$STATUS"
assert_contains "$STATUS" "history:   serving fronts at unix:$SOCK" "status shows the service"
assert_contains "$STATUS" "command=mount pod=pod-a" "and front a"
assert_contains "$STATUS" "command=mount pod=pod-b" "and front b"
assert_contains "$STATUS" "endpoint:  node node-1" "and the endpoint fronts given state find"
if [ "$T" = tls ]; then
  assert_contains "$STATUS" "tls:127.0.0.1:" "with its TLS address"
  assert_contains "$STATUS" "node=node-2 command=mount pod=pod-b" "front b is on node-2"
  PING=$(sudo env AGENTFS_NODE=node-2 "$AGENTFS_BIN" --state "$STATE" ping-history --count 200)
  echo "$PING"
  assert_contains "$PING" "ping state from node node-2" "ping-history over TLS"
fi
set +e
OUT=$(sudo "$AGENTFS_BIN" --state "$STATE" --root "$BACKING" record x 2>&1)
CODE=$?
set -e
assert_eq 75 "$CODE" "single-process commands wait for the lock"

step "each pod's files carry its committer"
as_agent "$MNT_A" 'printf a > from-a.txt'
as_agent "$MNT_B" 'printf b > from-b.txt'
wait_history from-a.txt a
wait_history from-b.txt b
assert_eq pod-a "$(committer_of from-a.txt)" "from-a.txt committed as pod-a"
assert_eq pod-b "$(committer_of from-b.txt)" "from-b.txt committed as pod-b"
assert_eq b "$(cat "$MNT_A/from-b.txt")" "a sees b's file"

step "created in a, renamed and removed through b"
as_agent "$MNT_A" 'printf tmp > made-in-a.txt'
wait_history made-in-a.txt tmp
as_agent "$MNT_B" 'mv made-in-a.txt moved-by-b.txt'
wait_history moved-by-b.txt tmp
wait_history made-in-a.txt -
assert_eq pod-b "$(committer_of moved-by-b.txt)" "the rename is pod-b's"
as_agent "$MNT_B" 'rm moved-by-b.txt'
wait_history moved-by-b.txt -

step "a file open in a, renamed or removed through b, then written and closed in a"
as_agent "$MNT_A" 'printf 1 > app.log && printf 1 > gone.log'
wait_history app.log 1
wait_history gone.log 1
sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- python3 -c '
import os, sys
a, b = sys.argv[1], sys.argv[2]
moved = os.open(a + "/app.log", os.O_WRONLY | os.O_APPEND)
removed = os.open(a + "/gone.log", os.O_WRONLY | os.O_APPEND)
os.rename(b + "/app.log", b + "/app.log.1")  # pod b rotates the log
os.remove(b + "/gone.log")                    # and removes the other
os.write(moved, b"2")
os.write(removed, b"2")
os.close(moved)
os.close(removed)
' "$MNT_A" "$MNT_B"
wait_history app.log.1 12
wait_history app.log -
wait_history gone.log -
# Quiet for a sweep or two: nothing brings the old names back.
sleep 3
wait_history app.log -
wait_history gone.log -
as_agent "$MNT_B" 'rm app.log.1'
wait_history app.log.1 -

step "a directory mv"
as_agent "$MNT_A" 'mkdir -p dir/sub && printf x > dir/sub/f && printf y > dir/g'
wait_history dir/sub/f x
wait_history dir/g y
as_agent "$MNT_B" 'mv dir dir2'
wait_history dir2/sub/f x
wait_history dir2/g y
wait_history dir/sub/f -
wait_history dir/g -
# One move, journaled by pod-b's front, reconciled by the service (design
# §3.1).
assert_eq "rename: dir -> dir2" "$(sgit log -1 --format=%s refs/heads/agentfs/demo -- dir2/g)" \
  "the directory rename is one move"
assert_eq pod-b "$(committer_of dir2/g)" "the move is pod-b's"

step "a file open in a while b renames its directory, then written and closed in a"
as_agent "$MNT_A" 'mkdir -p held && printf 1 > held/f'
wait_history held/f 1
sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- python3 -c '
import os, sys
a, b = sys.argv[1], sys.argv[2]
fd = os.open(a + "/held/f", os.O_WRONLY | os.O_APPEND)
os.rename(b + "/held", b + "/held2")  # pod b renames the directory
os.write(fd, b"2")
os.close(fd)
' "$MNT_A" "$MNT_B"
wait_history held2/f 12
wait_history held/f -

step "absolute gitdir targets map exactly through the fronts' views"
# Fronts run with AGENTFS_WORKLOAD_MOUNTS=/workspace (design §5).
as_agent "$MNT_A" 'set -e
  git init -q wsmain
  cd wsmain
  git -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m one
  git worktree add -q ../wswt
  cd ..
  printf w > wswt/f
  mkdir -p ws-seen ws-else
  printf "gitdir: /workspace/wsmain/.git/worktrees/wswt\n" > ws-seen/.git
  printf s > ws-seen/f
  printf "gitdir: /workspace/x/wsmain/.git/worktrees/wswt\n" > ws-else/.git
  printf e > ws-else/f'
wait_history ws-else/f e
wait_history ws-seen/f -
wait_history wswt/f -

step "a shared file, written by both in turn"
for i in 1 2 3 4; do
  if [ $((i % 2)) -eq 1 ]; then pod=pod-a mnt=$MNT_A; else pod=pod-b mnt=$MNT_B; fi
  as_agent "$mnt" "printf $i > shared.txt"
  wait_history shared.txt "$i"
  assert_eq "$pod" "$(committer_of shared.txt)" "version $i is $pod's"
done
assert_eq 4 "$(sudo cat "$BACKING/shared.txt")" "last writer wins in the tree"
# Each version, with the pod that committed it. A front's late record
# (RELEASE) can catch the other front's O_TRUNC before its write and
# commit that empty state as its own (design §14.1): skipped here.
VERSIONS=$(for c in $(sgit log --format=%H refs/heads/agentfs/demo -- shared.txt); do
  content=$(sgit show "$c:shared.txt" 2>/dev/null) || continue
  if [ -n "$content" ]; then printf '%s:%s ' "$(sgit log -1 --format=%cn "$c")" "$content"; fi
done)
assert_eq "pod-b:4 pod-a:3 pod-b:2 pod-a:1 " "$VERSIONS" "every boundary of shared.txt is in history"

step "a shared file, written by both at once"
# Every write must succeed: each front creates as the caller (design §6),
# so a create racing the other front's create of the same name never finds
# a file that is root's for an instant (EACCES).
burst() { # mountpoint pod
  as_agent "$1" "set -e; for i in \$(seq 1 40); do printf '$2-%s' \$i > burst.txt; done"
}
burst "$MNT_A" pod-a &
BURST_A=$!
burst "$MNT_B" pod-b &
BURST_B=$!
wait "$BURST_A" || fail "pod-a's burst failed"
wait "$BURST_B" || fail "pod-b's burst failed"
TREE=$(sudo cat "$BACKING/burst.txt")
# Freeze stamps order the racing records (design §14.1): HEAD ends on the
# tree's last state, with no later boundary to fix it. Late RELEASE records
# and commitWindow (200ms) take a moment; on a shared filesystem, longer.
if ! wait_for 30 sh -c "[ \"\$(sudo git --git-dir '$STATE/git' show refs/heads/agentfs/demo:burst.txt)\" = '$TREE' ]"; then
  echo "the last commits of burst.txt (committer, content):"
  for c in $(sgit log -12 --format=%H refs/heads/agentfs/demo -- burst.txt); do
    echo "  $(sgit log -1 --format='%cn %s' "$c"): '$(sgit show "$c:burst.txt" 2>/dev/null)'"
  done
  echo "tree now: '$(sudo cat "$BACKING/burst.txt")', via b: '$(sudo cat "$BACKING_B/burst.txt")'"
fi
assert_eq "$TREE" "$(sgit show refs/heads/agentfs/demo:burst.txt)" \
  "after the burst, HEAD has the tree's burst.txt"
N_BURST=$(sgit log --format=%cn refs/heads/agentfs/demo -- burst.txt | wc -l)
[ "$N_BURST" -ge 2 ] || fail "only $N_BURST commits of burst.txt"
pass "$N_BURST commits of burst.txt"
as_agent "$MNT_A" 'printf final > burst.txt'
wait_history burst.txt final

step "both fronts create the same new names at once"
# Each name is new to both fronts: one creates it, the other finds it made
# meanwhile and opens it, which needs it to be the caller's already.
create_race() { # mountpoint pod
  as_agent "$1" "set -e; mkdir -p fresh; for i in \$(seq 1 60); do printf '$2' > fresh/n-\$i.txt; done"
}
create_race "$MNT_A" pod-a &
RACE_A=$!
create_race "$MNT_B" pod-b &
RACE_B=$!
wait "$RACE_A" || fail "a create in pod-a failed"
wait "$RACE_B" || fail "a create in pod-b failed"
OWNERS=$(sudo stat -c %u "$BACKING"/fresh/n-*.txt | sort -u | tr '\n' ' ')
assert_eq "10000 " "$OWNERS" "every new file is the caller's"
wait_history fresh/n-60.txt "$(sudo cat "$BACKING/fresh/n-60.txt")"
as_agent "$MNT_A" 'rm -r fresh'
wait_history fresh/n-60.txt -

step "every commit's committer is its pod"
LOG=$(sgit log --format='%cn|%(trailers:key=Agentfs-Pod,valueonly,separator=%x2C)' \
  refs/heads/agentfs/demo)
echo "$LOG"
while IFS='|' read -r cn pod; do
  [ -z "$pod" ] && continue
  [ "$cn" = "$pod" ] || fail "committer $cn carries pod $pod"
done <<<"$LOG"
pass "committers match their pod trailers"
assert_contains "$LOG" "pod-a|pod-a" "pod-a appears"
assert_contains "$LOG" "pod-b|pod-b" "pod-b appears"

step "service down: the boundary fails with EIO and an alarm"
kill_service
OUT=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- python3 -c '
import errno, os, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"during the outage")
try:
    os.close(fd)
    print("closed")
except OSError as e:
    print(errno.errorcode.get(e.errno, e.errno))
' "$MNT_A/outage.txt")
assert_eq EIO "$OUT" "close(2) through a front fails with EIO"
wait_for 10 grep -q "alarm: history service .* is unavailable" "$A_LOG" ||
  fail "no history service alarm in front a's log"
pass "front a raised the alarm"
assert_eq "during the outage" "$(sudo cat "$BACKING/outage.txt")" "the tree has the write"

step "service back: the fronts reconnect and history catches up"
start_service
wait_history outage.txt "during the outage"
as_agent "$MNT_B" 'printf back > after-restart.txt'
wait_history after-restart.txt back
# Front a's sweep retries its failed close, so it reaches the service too.
wait_for 30 grep -q "is reachable again" "$A_LOG" || fail "front a never saw the service back"
pass "front a saw the service back"


step "a repair inherited from a dead front commits as its pod"
# Front a fails a close while the service is down, then dies (kill -9)
# before it can retry; a front of another pod takes its slot and repairs
# the path, under pod a's committer (design §14.1).
A_SLOT=$(sed -n 's/^mounted .* slot \([0-9]*\),.*/\1/p' "$A_LOG" | tail -n 1)
[ -n "$A_SLOT" ] || fail "no slot in front a's startup line"
kill_service
OUT=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- python3 -c '
import errno, os, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"inherited")
try:
    os.close(fd)
    print("closed")
except OSError as e:
    print(errno.errorcode.get(e.errno, e.errno))
' "$MNT_A/inherited.txt")
assert_eq EIO "$OUT" "the close fails with EIO"
REPAIRS=$(sudo cat "$STATE/meta/fronts/slot-$A_SLOT.repairs")
echo "$REPAIRS"
assert_contains "$REPAIRS" "@origin " "the repair file names its pod"
# [h]: the pattern must not match this pkill's own sudo.
sudo pkill -KILL -f -- "[h]istory-wait 30s $BACKING $MNT_A" || true
wait "$A_PID" 2>/dev/null || true
A_PID=
sudo umount -l "$MNT_A" 2>/dev/null || true
start_service
FRONT_SLOT=$A_SLOT start_front pod-c "$MNT_A" "$A_LOG"
A_PID=$FRONT_PID
wait_history inherited.txt inherited
assert_eq pod-a "$(committer_of inherited.txt)" "the repair commits as pod-a"
POD=$(sgit log -1 --format='%(trailers:key=Agentfs-Pod,valueonly)' refs/heads/agentfs/demo -- inherited.txt)
assert_contains "$POD" "pod-a" "with pod-a's trailer"
as_agent "$MNT_A" 'printf mine > from-c.txt'
wait_history from-c.txt mine
assert_eq pod-c "$(committer_of from-c.txt)" "pod-c's own writes are its own"

step "restore and revert while the service serves both fronts"
# The service settles both fronts, refuses a path one holds open for
# writing, writes the backing tree and has both drop their kernel caches
# (design §15): each mount shows the restored content at once.
as_agent "$MNT_A" 'printf r1 > restored.txt'
wait_history restored.txt r1
R1=$(sgit log -1 --format=%H refs/heads/agentfs/demo -- restored.txt)
as_agent "$MNT_B" 'printf r2-longer > restored.txt'
wait_history restored.txt r2-longer
R2=$(sgit log -1 --format=%H refs/heads/agentfs/demo -- restored.txt)
# Both mounts have read the file, so both kernels cache it.
assert_eq r2-longer "$(as_agent "$MNT_A" 'cat restored.txt')" "a sees r2-longer"
assert_eq r2-longer "$(as_agent "$MNT_B" 'cat restored.txt')" "b sees r2-longer"
as_agent "$MNT_B" 'exec 3>>restored.txt; sleep 4' &
HOLDER=$!
sleep 1
set +e
sudo "$AGENTFS_BIN" --state "$STATE" restore restored.txt "$R1" >"$W/refused.out" 2>&1
code=$?
set -e
cat "$W/refused.out"
assert_eq 1 "$code" "a restore of a path open for writing in b is refused (exit 1)"
assert_contains "$(cat "$W/refused.out")" "restore refused:" "with a refusal"
assert_contains "$(cat "$W/refused.out")" "open for writing" "naming the open file"
wait "$HOLDER" || true
assert_eq r2-longer "$(sudo cat "$BACKING/restored.txt")" "the refused restore wrote nothing"
# --force writes it while b holds it open for writing.
as_agent "$MNT_B" 'exec 3>>restored.txt; sleep 4' &
HOLDER=$!
sleep 1
sudo "$AGENTFS_BIN" --state "$STATE" restore --force restored.txt "$R1"
wait "$HOLDER" || true
assert_eq r1 "$(sudo cat "$BACKING/restored.txt")" "--force restores a path open for writing"
assert_eq r1 "$(as_agent "$MNT_A" 'cat restored.txt')" "a sees the forced restore"
wait_history restored.txt r1
sudo "$AGENTFS_BIN" --state "$STATE" restore restored.txt "$R2"
wait_history restored.txt r2-longer
sudo "$AGENTFS_BIN" --state "$STATE" restore restored.txt "$R1"
assert_eq r1 "$(as_agent "$MNT_A" 'cat restored.txt')" "a sees the restored r1"
assert_eq r1 "$(as_agent "$MNT_B" 'cat restored.txt')" "b sees the restored r1"
wait_history restored.txt r1
assert_contains "$(sgit log -1 --format=%B refs/heads/agentfs/demo)" "Agentfs-Trigger: restore" \
  "HEAD is the restore"
sudo "$AGENTFS_BIN" --state "$STATE" revert HEAD
assert_eq r2-longer "$(as_agent "$MNT_A" 'cat restored.txt')" "a sees the revert"
assert_eq r2-longer "$(as_agent "$MNT_B" 'cat restored.txt')" "b sees the revert"
wait_history restored.txt r2-longer
as_agent "$MNT_A" 'printf after > after-restore.txt'
wait_history after-restore.txt after
assert_eq pod-c "$(committer_of after-restore.txt)" "writes after the restore are recorded"

step "kill -9 the service right after a close returns"
# A long commit window, so the record is journaled but not committed when
# the service dies, and only the journal's replay can commit it.
stop_service
sgit config agentfs.commitWindow 5s
start_service
as_agent "$MNT_B" 'printf durable > durable.txt'
kill_service
sgit cat-file -e refs/heads/agentfs/demo:durable.txt 2>/dev/null &&
  fail "durable.txt was committed before the kill: the replay is not exercised"
pass "durable.txt is journaled, not committed, when the service dies"
sgit config agentfs.commitWindow 200ms
start_service
wait_history durable.txt durable
assert_eq pod-b "$(committer_of durable.txt)" "replayed with pod-b's committer"

step "the service pushes"
wait_for 30 sh -c "[ \"\$(sudo git --git-dir '$REMOTE' rev-parse refs/heads/agentfs/demo 2>/dev/null)\" = \"\$(sudo git --git-dir '$STATE/git' rev-parse refs/heads/agentfs/demo)\" ]" ||
  fail "the remote never got the local head"
pass "the remote has the local head"

step "stop the fronts, then the service"
stop_front "$A_PID" pod-a
A_PID=
stop_front "$B_PID" pod-b
B_PID=
stop_service
# The final commit may add commits after the last push.
sudo "$AGENTFS_BIN" --state "$STATE" push >/dev/null
assert_eq "$(head_of)" "$(sudo git --git-dir "$REMOTE" rev-parse refs/heads/agentfs/demo)" \
  "after a push, the remote has the final head"

step "per-close cost: single-process mount, then a front"
N=${BOUNDARY_N:-1000}
# uid 10000 cannot read the checkout (the runner's home is private).
install -m 755 "$HERE/boundary-latency.py" "$W/boundary-latency.py"
sudo "$AGENTFS_BIN" --state "$STATE" mount --passthrough "$P" --unmount-grace 5s \
  "$BACKING" "$MNT_A" >>"$SP_LOG" 2>&1 &
SP_PID=$!
wait_for 30 mountpoint -q "$MNT_A" || fail "the single-process mount never mounted"
as_agent "$MNT_A" 'mkdir -p bench-single'
SINGLE=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- \
  python3 "$W/boundary-latency.py" "$MNT_A/bench-single" "$N" single-process)
sudo kill -TERM "$SP_PID"
wait "$SP_PID" || fail "the single-process mount did not exit cleanly"
SP_PID=
start_service
start_front pod-a "$MNT_A" "$A_LOG"
A_PID=$FRONT_PID
as_agent "$MNT_A" 'mkdir -p bench-front'
FRONT=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- \
  python3 "$W/boundary-latency.py" "$MNT_A/bench-front" "$N" front)
stop_front "$A_PID" pod-a
A_PID=
TLS_FRONT=
TLS_PING=
UNIX_PING=$(sudo env AGENTFS_NODE=node-1 "$AGENTFS_BIN" --state "$STATE" ping-history --count 2000)
if [ "$T" = tls ]; then
  TLS_PING=$(sudo env AGENTFS_NODE=node-2 "$AGENTFS_BIN" --state "$STATE" ping-history --count 2000)
  FRONT_ADDR=state FRONT_NODE=node-2 start_front pod-a "$MNT_A" "$A_LOG"
  A_PID=$FRONT_PID
  as_agent "$MNT_A" 'mkdir -p bench-tls'
  TLS_FRONT=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- \
    python3 "$W/boundary-latency.py" "$MNT_A/bench-tls" "$N" front-tls-loopback)
  stop_front "$A_PID" pod-a
  A_PID=
fi
stop_service

row() { # line -> markdown row
  echo "$1" | sed -E 's/^([^ ]+) n=([0-9]+) p50=([0-9]+) p90=([0-9]+) p99=([0-9]+) max=([0-9]+)$/| \1 | \2 | \3 | \4 | \5 | \6 |/'
}
TABLE=$(
  echo "### Per-close latency, passthrough $P, front b over $T (µs, open + write 4 KiB + close)"
  echo
  echo "Kernel $(uname -r); backing tree on $(findmnt -no FSTYPE -T "$BACKING")."
  echo
  echo "| mode | n | p50 | p90 | p99 | max |"
  echo "|---|---|---|---|---|---|"
  row "$SINGLE"
  row "$FRONT"
  if [ -n "$TLS_FRONT" ]; then row "$TLS_FRONT"; fi
  echo
  echo "Round trip (\`agentfs ping-history\`, no FUSE): $UNIX_PING"
  if [ -n "$TLS_PING" ]; then echo; echo "$TLS_PING"; fi
)
echo "$TABLE"
notice_summary "multipod latency ($P, $T)" "$TABLE"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "$TABLE" >>"$GITHUB_STEP_SUMMARY"
fi
pass "multi-pod e2e done"
