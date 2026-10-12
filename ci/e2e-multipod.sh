#!/usr/bin/env bash
# End-to-end multi-pod at single-node level (design §14, §18 test 16): two
# real FUSE front mounts of one backing tree, as pods pod-a and pod-b,
# feeding one `agentfs serve-history`. Checks that every commit carries the
# committer and pod trailers of the mount that made the change, that a
# service outage fails the boundary with EIO and an alarm and that history
# catches up once it is back, that a change whose close returned survives a
# kill -9 of the service, and that the service pushes. Then measures the
# per-close cost, single-process against front (ci/boundary-latency.py).
#
# Needs Linux with /dev/fuse, fuse3, git, python3, setpriv and passwordless
# sudo; reproducible on any such host:
#
#   AGENTFS_TEST_PASSTHROUGH=auto ci/e2e-multipod.sh

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
W=$(mktemp -d)
chmod 755 "$W"
STATE=$W/state
BACKING=$W/backing
MNT_A=$W/mnt-a
MNT_B=$W/mnt-b
REMOTE=$W/remote.git
SOCK=$STATE/meta/history/history.sock
LOGS=${RUNNER_TEMP:-$W}/agentfs-multipod-logs
mkdir -p "$BACKING" "$MNT_A" "$MNT_B" "$LOGS"
chmod 755 "$BACKING" "$MNT_A" "$MNT_B"
SVC_LOG=$LOGS/service-$P.log
A_LOG=$LOGS/front-a-$P.log
B_LOG=$LOGS/front-b-$P.log
SP_LOG=$LOGS/single-process-$P.log
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
    for log in "$SVC_LOG" "$A_LOG" "$B_LOG" "$SP_LOG"; do
      echo "---- $log"
      tail -n 60 "$log" 2>/dev/null || true
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

start_service() {
  echo "---- serve-history" >>"$SVC_LOG"
  local before
  before=$(grep -c '^serving history' "$SVC_LOG" || true)
  sudo env AGENTFS_REPLICA=history-0 "$AGENTFS_BIN" --state "$STATE" --root "$BACKING" \
    serve-history >>"$SVC_LOG" 2>&1 &
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

start_front() { # pod mountpoint log -> sets FRONT_PID
  local before
  before=$(grep -c '^mounted ' "$3" || true)
  sudo env AGENTFS_POD="$1" AGENTFS_POD_UID="$1-uid" AGENTFS_NODE=node-1 \
    AGENTFS_NAMESPACE=agents AGENTFS_REPLICA="$1" \
    "$AGENTFS_BIN" --state "$STATE" mount --passthrough "$P" --unmount-grace 5s \
    --history-service "$SOCK" --history-timeout 5s --history-wait 30s \
    "$BACKING" "$2" >>"$3" 2>&1 &
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
start_front pod-b "$MNT_B" "$B_LOG"
B_PID=$FRONT_PID
sudo chown 10000:10000 "$MNT_A"
grep '^mounted ' "$A_LOG"
STATUS=$(sudo "$AGENTFS_BIN" --state "$STATE" status)
echo "$STATUS"
assert_contains "$STATUS" "history:   serving fronts at unix:$SOCK" "status shows the service"
assert_contains "$STATUS" "command=mount pod=pod-a" "and front a"
assert_contains "$STATUS" "command=mount pod=pod-b" "and front b"
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

step "a directory mv"
as_agent "$MNT_A" 'mkdir -p dir/sub && printf x > dir/sub/f && printf y > dir/g'
wait_history dir/sub/f x
wait_history dir/g y
as_agent "$MNT_B" 'mv dir dir2'
wait_history dir2/sub/f x
wait_history dir2/g y
wait_history dir/sub/f -
wait_history dir/g -

step "a shared file, written by both in turn"
for i in 1 2 3 4; do
  if [ $((i % 2)) -eq 1 ]; then pod=pod-a mnt=$MNT_A; else pod=pod-b mnt=$MNT_B; fi
  as_agent "$mnt" "printf $i > shared.txt"
  wait_history shared.txt "$i"
  assert_eq "$pod" "$(committer_of shared.txt)" "version $i is $pod's"
done
assert_eq 4 "$(sudo cat "$BACKING/shared.txt")" "last writer wins in the tree"
VERSIONS=$(sgit log --format=%cn refs/heads/agentfs/demo -- shared.txt | tr '\n' ' ')
assert_eq "pod-b pod-a pod-b pod-a " "$VERSIONS" "every boundary of shared.txt is in history"

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
grep -q "is reachable again" "$A_LOG" && pass "front a saw the service back" || true

step "kill -9 the service right after a close returns"
as_agent "$MNT_B" 'printf durable > durable.txt'
kill_service
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
sudo "$AGENTFS_BIN" --state "$STATE" mount --passthrough "$P" --unmount-grace 5s \
  "$BACKING" "$MNT_A" >>"$SP_LOG" 2>&1 &
SP_PID=$!
wait_for 30 mountpoint -q "$MNT_A" || fail "the single-process mount never mounted"
as_agent "$MNT_A" 'mkdir -p bench-single'
SINGLE=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- \
  python3 "$HERE/boundary-latency.py" "$MNT_A/bench-single" "$N" single-process)
sudo kill -TERM "$SP_PID"
wait "$SP_PID" || fail "the single-process mount did not exit cleanly"
SP_PID=
start_service
start_front pod-a "$MNT_A" "$A_LOG"
A_PID=$FRONT_PID
as_agent "$MNT_A" 'mkdir -p bench-front'
FRONT=$(sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- \
  python3 "$HERE/boundary-latency.py" "$MNT_A/bench-front" "$N" front)
stop_front "$A_PID" pod-a
A_PID=
stop_service

row() { # line -> markdown row
  echo "$1" | sed -E 's/^([^ ]+) n=([0-9]+) p50=([0-9]+) p90=([0-9]+) p99=([0-9]+) max=([0-9]+)$/| \1 | \2 | \3 | \4 | \5 | \6 |/'
}
TABLE=$(
  echo "### Per-close latency, passthrough $P (µs, open + write 4 KiB + close)"
  echo
  echo "Kernel $(uname -r); backing tree on $(findmnt -no FSTYPE -T "$BACKING")."
  echo
  echo "| mode | n | p50 | p90 | p99 | max |"
  echo "|---|---|---|---|---|---|"
  row "$SINGLE"
  row "$FRONT"
)
echo "$TABLE"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "$TABLE" >>"$GITHUB_STEP_SUMMARY"
fi
pass "multi-pod e2e done"
