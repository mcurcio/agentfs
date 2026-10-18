#!/usr/bin/env bash
# The filesystem keeps working through a live FUSE mount while its remotes
# fail (design §11, §18 test 8). One mount pushes every second to a good
# smart-HTTP remote and to six failing ones:
#
#   gone       http://127.0.0.1:9/      nothing listens: unreachable
#   hung       ci/hung-server.sh        accepts and never answers
#   refused    http, pre-receive hook   refused
#   badauth    http, wrong password     auth
#   diverged   a remote branch advanced out of band: non_fast_forward,
#              the head saved as a diverged ref
#   untrusted  https, self-signed       untrusted_host
#
# Through that mount: 4 KiB write-and-close cycles (their close latency
# against a baseline taken on the same volume with only the good remote),
# renames, mkdir and rmdir, and a `git clone`. Checks that the close
# latency stays within max(3x baseline, 50 ms) at p99 and under 1 s at
# most, that everything is in history and on the good remote, that
# `agentfs status` names each failure's kind and the alarms fired, that
# the refused and badauth remotes catch up once fixed (hook removed,
# credential rotated), and that SIGTERM ends the mount within its push
# grace, leaving the hung remote's push abandoned.
#
# Needs Linux with /dev/fuse, fuse3, git, jq, python3, socat, passwordless
# sudo, a server from ci/git-http-server.sh started with --trust-ca, and a
# binary built with --features https:
#
#   eval "$(ci/git-http-server.sh start /tmp/git-server --trust-ca)"
#   AGENTFS_TEST_PASSTHROUGH=auto ci/e2e-remote-failures.sh

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_HTTP_BASE:?}" "${GIT_HTTPS_UNTRUSTED_BASE:?}"
[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"
for tool in jq python3 socat; do
  command -v "$tool" >/dev/null || fail "this test needs $tool"
done

HERE=$(cd "$(dirname "$0")" && pwd)
P=${AGENTFS_TEST_PASSTHROUGH:-auto}
W=$(mktemp -d)
chmod 755 "$W"
STATE=$W/state
BACKING=$W/backing
MNT=$W/mnt
CREDS=$W/creds
LOGS=${RUNNER_TEMP:-$W}/agentfs-remote-failures-logs
LOG=$LOGS/mount.log
HUNG_PORT=9419
CLOSES=400
mkdir -p "$BACKING" "$MNT" "$CREDS" "$LOGS"
chmod 755 "$BACKING" "$MNT"
: >"$LOG"
MOUNT_PID=

sgit() { sudo git --git-dir "$STATE/git" "$@"; }
sagentfs() { sudo env AGENTFS_POD=pod-0 "$AGENTFS_BIN" --state "$STATE" "$@"; }
head_of() { sgit rev-parse refs/heads/agentfs/demo; }
in_mnt() { sudo sh -c "cd '$MNT' && $1"; }

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

cleanup() {
  status=$?
  if [ -n "$MOUNT_PID" ]; then
    sudo kill -TERM "$MOUNT_PID" 2>/dev/null || true
    wait "$MOUNT_PID" 2>/dev/null || true
  fi
  mountpoint -q "$MNT" && sudo umount -l "$MNT"
  "$HERE/hung-server.sh" stop "$W/hung.pid"
  if [ $status -ne 0 ]; then
    echo "---- $LOG"
    tail -n 80 "$LOG" || true
  fi
  exit $status
}
trap cleanup EXIT

mount_volume() {
  echo "---- mount" >>"$LOG"
  sudo env AGENTFS_POD=pod-0 "$AGENTFS_BIN" --state "$STATE" mount --passthrough "$P" \
    --unmount-grace 5s "$BACKING" "$MNT" >>"$LOG" 2>&1 &
  MOUNT_PID=$!
  wait_for 30 mountpoint -q "$MNT" || fail "never mounted"
}

stop_volume() {
  sudo kill -TERM "$MOUNT_PID"
  wait "$MOUNT_PID" || fail "the mount did not exit cleanly"
  MOUNT_PID=
}

# Close latency through the mount: sets P99 and MAX (microseconds).
latency() { # label
  local json
  in_mnt "mkdir -p lat-$1"
  json=$(sudo python3 "$HERE/boundary-latency.py" --json "$MNT/lat-$1" "$CLOSES" "$1")
  echo "$json"
  P99=$(jq -r '.p99_us | floor' <<<"$json")
  MAX=$(jq -r '.max_us | floor' <<<"$json")
}

step "a volume with a good remote: the baseline"
set_password good-user good-pw
set_password bad-user right-pw
printf 'username=good-user\npassword=good-pw\n' >"$CREDS/good"
printf 'username=bad-user\npassword=wrong-pw\n' >"$CREDS/bad"
GOOD=$GIT_SERVER_REPOS/good.git
REFUSED=$GIT_SERVER_REPOS/refused.git
git init -q --bare "$GOOD"
git init -q --bare "$REFUSED"
git init -q --bare "$GIT_SERVER_REPOS/badauth.git"
printf '#!/bin/sh\necho "refused by policy" >&2\nexit 1\n' >"$REFUSED/hooks/pre-receive"
chmod +x "$REFUSED/hooks/pre-receive"
sagentfs init --volume demo >/dev/null
sgit config agentfs.commitWindow 100ms
sgit config agentfs.pushInterval 1s
sgit config remote.good.url "$GIT_HTTP_BASE/good.git"
sgit config remote.good.agentfsCredentials "$CREDS/good"
mount_volume
latency baseline
BASE_P99=$P99
stop_volume
pass "baseline p99 ${BASE_P99}us"

step "six failing remotes"
"$HERE/hung-server.sh" start "$HUNG_PORT" "$W/hung.pid"
# The diverged remote's branch already holds a commit of its own.
DIVERGED=$W/diverged.git
sudo git init -q --bare "$DIVERGED"
sudo git -C "$W" -c init.defaultBranch=main init -q other
sudo sh -c "cd '$W/other' && echo other >other.txt && git add other.txt &&
  git -c user.name=else -c user.email=else@example.invalid commit -q -m other &&
  git push -q '$DIVERGED' HEAD:refs/heads/agentfs/demo"
sgit config remote.gone.url "http://127.0.0.1:9/git/gone.git"
sgit config remote.hung.url "http://127.0.0.1:$HUNG_PORT/git/hung.git"
sgit config remote.refused.url "$GIT_HTTP_BASE/refused.git"
sgit config remote.refused.agentfsCredentials "$CREDS/good"
sgit config remote.badauth.url "$GIT_HTTP_BASE/badauth.git"
sgit config remote.badauth.agentfsCredentials "$CREDS/bad"
sgit config remote.diverged.url "$DIVERGED"
sgit config remote.untrusted.url "$GIT_HTTPS_UNTRUSTED_BASE/untrusted.git"
mount_volume

step "the workload through the mount while the remotes fail"
latency failing
FAIL_P99=$P99
FAIL_MAX=$MAX
in_mnt "mkdir -p tree/a/b && echo one >tree/a/b/f.txt && mv tree/a/b/f.txt tree/a/g.txt"
in_mnt "mkdir gone-dir && rmdir gone-dir && mv tree/a tree/c"
for i in $(seq 50); do in_mnt "echo $i >small-$i.txt"; done
in_mnt "for i in \$(seq 25); do mv small-\$i.txt moved-\$i.txt; done"
sudo git -C "$W" -c init.defaultBranch=main init -q src
sudo sh -c "cd '$W/src' && for i in 1 2 3; do echo \$i >s\$i.txt; git add s\$i.txt &&
  git -c user.name=ci -c user.email=ci@example.invalid commit -q -m c\$i; done"
in_mnt "git clone -q '$W/src' clone" || fail "git clone into the mount failed"
assert_eq 3 "$(in_mnt 'cat clone/s3.txt')" "git clone into the mount works"
in_mnt "echo last >last.txt"

limit=$((3 * BASE_P99))
[ "$limit" -ge 50000 ] || limit=50000
[ "$FAIL_P99" -le "$limit" ] ||
  fail "close p99 ${FAIL_P99}us with failing remotes, over max(3x baseline ${BASE_P99}us, 50ms)"
[ "$FAIL_MAX" -lt 1000000 ] || fail "a close took ${FAIL_MAX}us with failing remotes"
pass "close latency with failing remotes: p99 ${FAIL_P99}us (baseline ${BASE_P99}us), max ${FAIL_MAX}us"

has_history() { # path content
  [ "$(sgit show "refs/heads/agentfs/demo:$1" 2>/dev/null)" = "$2" ]
}
wait_for 30 has_history last.txt last || fail "last.txt never reached history"
for check in "tree/c/g.txt one" "small-50.txt 50" "moved-1.txt 1" "moved-25.txt 25" "small-26.txt 26"; do
  set -- $check
  has_history "$1" "$2" || fail "history lacks $1 = $2"
done
for gone in small-1.txt tree/a/g.txt tree/a/b/f.txt; do
  if sgit cat-file -e "refs/heads/agentfs/demo:$gone" 2>/dev/null; then fail "history still has $gone"; fi
done
n=$(sgit ls-tree -r --name-only refs/heads/agentfs/demo lat-failing | wc -l)
[ "$n" -ge 1 ] || fail "the latency files are not in history"
pass "every change is in history"
good_is_head() { [ "$(git --git-dir "$GOOD" rev-parse refs/heads/agentfs/demo 2>/dev/null)" = "$(head_of)" ]; }
wait_for 30 good_is_head || fail "the good remote never reached the head"
pass "the good remote has the head"

step "status and alarms name each failure"
remote_status() { # name
  sagentfs status | grep "^remote:    $1 "
}
expect_failure() { # remote kind
  wait_for 60 sh -c "sudo env AGENTFS_POD=pod-0 '$AGENTFS_BIN' --state '$STATE' status |
    grep '^remote:    $1 ' | grep -q 'last error ($2)'" ||
    fail "$1 never reported $2: $(remote_status "$1")"
  pass "status: $1 failed ($2)"
  grep -q "alarm: push to $1 failed ($2)" "$LOG" || fail "no alarm for $1 ($2)"
  pass "alarm: $1 ($2)"
}
expect_failure gone unreachable
expect_failure refused refused
expect_failure badauth auth
expect_failure untrusted untrusted_host
expect_failure diverged non_fast_forward
wait_for 30 sh -c "sudo env AGENTFS_POD=pod-0 '$AGENTFS_BIN' --state '$STATE' status |
  grep '^remote:    diverged ' | grep -q 'diverged: head saved as'" ||
  fail "the diverged head was never saved: $(remote_status diverged)"
pass "status: the diverged head is saved"
grep -q "alarm: diverged diverged: remote branch is at" "$LOG" || fail "no divergence alarm"
pass "alarm: divergence"
assert_contains "$(remote_status hung)" "last pushed never" "status: the hung remote never pushed"
assert_contains "$(remote_status good)" " 0 consecutive failure(s)" "status: the good remote is healthy"

step "the refused and badauth remotes catch up once fixed"
rm "$REFUSED/hooks/pre-receive"
printf 'username=bad-user\npassword=right-pw\n' >"$CREDS/bad.new"
mv "$CREDS/bad.new" "$CREDS/bad"
out=$(sagentfs push --remote refused --remote badauth 2>&1) || fail "push after the fixes failed: $out"
assert_eq "$(head_of)" "$(git --git-dir "$REFUSED" rev-parse refs/heads/agentfs/demo)" "refused caught up"
assert_eq "$(head_of)" "$(git --git-dir "$GIT_SERVER_REPOS/badauth.git" rev-parse refs/heads/agentfs/demo)" \
  "badauth caught up"

step "SIGTERM ends the mount within its push grace"
in_mnt "echo final >final.txt"
start=$SECONDS
stop_volume
took=$((SECONDS - start))
[ "$took" -le 15 ] || fail "the mount took ${took}s to stop"
pass "stopped in ${took}s"
grep -q "final push: hung: still running at the push grace's end; abandoned" "$LOG" ||
  fail "the hung remote's push was not reported abandoned"
pass "the hung remote's push was abandoned"
has_history final.txt final || fail "final.txt was not committed"
assert_eq "$(head_of)" "$(git --git-dir "$GOOD" rev-parse refs/heads/agentfs/demo)" \
  "the final push reached the good remote"
