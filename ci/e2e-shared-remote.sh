#!/usr/bin/env bash
# Two volumes pushing to one shared remote at the same time (design §11.3,
# §18 test 10): vol-a and vol-b, each a real FUSE mount with a writer, push
# every second to one smart-HTTP repository (as different users) and to
# one file:// bare repository, each on its own branch and ref namespace,
# plus ten rounds of `agentfs push` started together on both. The shared
# HTTP repository's pre-receive hook sleeps, so the two volumes' pushes
# overlap on the server, which its access log proves. Checks that no push
# failed, that each branch ends at its volume's head, that nothing was
# saved as diverged, and that both remotes pass `git fsck --strict`.
#
# Needs Linux with /dev/fuse, fuse3, git and passwordless sudo, and a server
# from ci/git-http-server.sh (its GIT_* variables):
#
#   eval "$(ci/git-http-server.sh start /tmp/git-server)"
#   AGENTFS_TEST_PASSTHROUGH=auto ci/e2e-shared-remote.sh

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_HTTP_BASE:?}" "${GIT_SERVER_ACCESS_LOG:?}"
[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"

P=${AGENTFS_TEST_PASSTHROUGH:-auto}
W=$(mktemp -d)
chmod 755 "$W"
LOGS=${RUNNER_TEMP:-$W}/agentfs-shared-remote-logs
mkdir -p "$LOGS"
HTTP_REPO=$GIT_SERVER_REPOS/shared.git
FILE_REPO=$W/shared-file.git
CREDS=$W/creds
ROUNDS=10
declare -A MOUNT_PID=() WRITER_PID=()

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

vstate() { echo "$W/$1/state"; }
vmnt() { echo "$W/$1/mnt"; }
vgit() { local v=$1; shift; sudo git --git-dir "$(vstate "$v")/git" "$@"; }
# git on a remote, as its owner (the http one is the runner's, the file one root's).
rgit() { local r=$1; shift; if [ -O "$r" ]; then git --git-dir "$r" "$@"; else sudo git --git-dir "$r" "$@"; fi; }
vagentfs() { local v=$1; shift; sudo env AGENTFS_POD="pod-$v" "$AGENTFS_BIN" --state "$(vstate "$v")" "$@"; }

cleanup() {
  status=$?
  for pid in "${WRITER_PID[@]}"; do kill "$pid" 2>/dev/null || true; done
  for v in "${!MOUNT_PID[@]}"; do
    sudo kill -TERM "${MOUNT_PID[$v]}" 2>/dev/null || true
    wait "${MOUNT_PID[$v]}" 2>/dev/null || true
    mountpoint -q "$(vmnt "$v")" && sudo umount -l "$(vmnt "$v")"
  done
  if [ $status -ne 0 ]; then
    for log in "$LOGS"/*.log; do
      echo "---- $log"
      tail -n 60 "$log" || true
    done
  fi
  exit $status
}
trap cleanup EXIT

step "two volumes, one shared http remote and one shared file remote"
set_password vol-a pw-a
set_password vol-b pw-b
mkdir -p "$CREDS"
git init -q --bare "$HTTP_REPO"
# Every receive-pack takes a second: pushes from both volumes overlap.
printf '#!/bin/sh\nsleep 1\n' >"$HTTP_REPO/hooks/pre-receive"
chmod +x "$HTTP_REPO/hooks/pre-receive"
sudo git init -q --bare "$FILE_REPO"
for v in a b; do
  mkdir -p "$W/$v/backing" "$W/$v/mnt"
  chmod 755 "$W/$v" "$W/$v/backing" "$W/$v/mnt"
  printf 'username=vol-%s\npassword=pw-%s\n' "$v" "$v" >"$CREDS/$v"
  vagentfs "$v" init --volume "vol-$v" --local-only >/dev/null
  vgit "$v" config agentfs.commitWindow 100ms
  vgit "$v" config agentfs.pushInterval 1s
  vgit "$v" config --unset agentfs.remotes  # a remote declares the remotes (F8 S3)
  vgit "$v" config remote.http.url "$GIT_HTTP_BASE/shared.git"
  vgit "$v" config remote.http.agentfsCredentials "$CREDS/$v"
  vgit "$v" config remote.file.url "$FILE_REPO"
  : >"$LOGS/vol-$v.log"
  sudo env AGENTFS_POD="pod-$v" "$AGENTFS_BIN" --state "$(vstate "$v")" mount \
    --passthrough "$P" --unmount-grace 5s "$W/$v/backing" "$(vmnt "$v")" \
    >>"$LOGS/vol-$v.log" 2>&1 &
  MOUNT_PID[$v]=$!
  wait_for 30 mountpoint -q "$(vmnt "$v")" || fail "vol-$v never mounted"
  pass "vol-$v mounted"
done

step "writers on both mounts while both push every second"
for v in a b; do
  (
    i=0
    while :; do
      i=$((i + 1))
      sudo sh -c "printf '%s\n' $i >'$(vmnt "$v")/w-$((i % 16)).txt'"
      sleep 0.05
    done
  ) &
  WRITER_PID[$v]=$!
done

step "$ROUNDS rounds of agentfs push on both volumes at once"
for round in $(seq "$ROUNDS"); do
  go=$W/go-$round
  for v in a b; do
    (
      while [ ! -e "$go" ]; do sleep 0.01; done
      vagentfs "$v" push >"$LOGS/push-$v-$round.out" 2>&1
      echo $? >"$LOGS/push-$v-$round.code"
    ) &
  done
  sleep 0.5
  touch "$go"
  wait_for 120 sh -c "[ -e '$LOGS/push-a-$round.code' ] && [ -e '$LOGS/push-b-$round.code' ]" ||
    fail "round $round never finished"
  for v in a b; do
    [ "$(cat "$LOGS/push-$v-$round.code")" = 0 ] ||
      fail "round $round: vol-$v's push failed: $(cat "$LOGS/push-$v-$round.out")"
  done
done
pass "every push of every round succeeded"
sleep 5
for v in a b; do kill "${WRITER_PID[$v]}"; wait "${WRITER_PID[$v]}" 2>/dev/null || true; done
# A write the writer had started finishes.
sleep 1
WRITER_PID=()

step "the final push; each branch is its volume's head"
for v in a b; do
  vagentfs "$v" push >"$LOGS/push-$v-final.out" 2>&1 ||
    fail "vol-$v's final push failed: $(cat "$LOGS/push-$v-final.out")"
done
for v in a b; do
  head=$(vgit "$v" rev-parse "refs/heads/agentfs/vol-$v")
  # Every write reached history: the last content of each file.
  for f in "$(vmnt "$v")"/w-*.txt; do
    name=$(basename "$f")
    assert_eq "$(sudo cat "$f")" "$(vgit "$v" show "$head:$name")" "vol-$v: $name is recorded" >/dev/null
  done
  assert_eq "$head" "$(rgit "$HTTP_REPO" rev-parse "refs/heads/agentfs/vol-$v")" \
    "vol-$v's branch on the http remote is its head"
  assert_eq "$head" "$(rgit "$FILE_REPO" rev-parse "refs/heads/agentfs/vol-$v")" \
    "vol-$v's branch on the file remote is its head"
  status=$(vagentfs "$v" status)
  n=$(grep -c '^remote:' <<<"$status")
  assert_eq 2 "$n" "vol-$v has two remotes"
  if grep '^remote:' <<<"$status" | grep -v ' 0 consecutive failure(s)' | grep -q .; then
    fail "vol-$v has a failing remote: $status"
  fi
  case $status in *"last error"* | *diverged*) fail "vol-$v reports an error: $status" ;; esac
  pass "vol-$v: no push failed"
  if grep -q "alarm: push to" "$LOGS/vol-$v.log"; then
    fail "vol-$v raised a push alarm: $(grep "alarm: push to" "$LOGS/vol-$v.log" | head -n 3)"
  fi
  pass "vol-$v: no push alarm"
done
for repo in "$HTTP_REPO" "$FILE_REPO"; do
  refs=$(rgit "$repo" for-each-ref --format='%(refname)')
  if grep -q diverged <<<"$refs"; then fail "$repo has diverged refs: $refs"; fi
  rgit "$repo" fsck --full --strict --no-progress >/dev/null 2>&1 ||
    fail "git fsck of $repo: $(rgit "$repo" fsck --full --strict 2>&1 | tail -n 5)"
done
pass "no diverged refs; both remotes pass git fsck --strict"
assert_eq "refs/heads/agentfs/vol-a
refs/heads/agentfs/vol-b" "$(rgit "$HTTP_REPO" for-each-ref --format='%(refname)' refs/heads/)" \
  "the http remote has exactly the two branches"

step "the two volumes' pushes overlapped on the server"
# receive-pack requests as "user start end" (start = end - request time).
pushes=$(awk '$0 ~ /git-receive-pack/ && ($3 == "vol-a" || $3 == "vol-b") { printf "%s %.3f %.3f\n", $3, $1 - $NF, $1 }' \
  "$GIT_SERVER_ACCESS_LOG")
na=$(grep -c '^vol-a ' <<<"$pushes" || true)
nb=$(grep -c '^vol-b ' <<<"$pushes" || true)
[ "$na" -ge "$ROUNDS" ] && [ "$nb" -ge "$ROUNDS" ] ||
  fail "too few receive-packs in the access log (vol-a $na, vol-b $nb)"
overlaps=$(awk '
  $1 == "vol-a" { as[++na] = $2; ae[na] = $3 }
  $1 == "vol-b" { bs[++nb] = $2; be[nb] = $3 }
  END {
    n = 0
    for (i = 1; i <= na; i++) for (j = 1; j <= nb; j++)
      if (as[i] < be[j] && bs[j] < ae[i]) n++
    print n
  }' <<<"$pushes")
[ "$overlaps" -ge 1 ] || fail "no two receive-packs of vol-a and vol-b overlapped:
$pushes"
pass "$overlaps overlapping receive-pack pairs (vol-a $na, vol-b $nb pushes)"

step "both mounts stop cleanly"
for v in a b; do
  sudo kill -TERM "${MOUNT_PID[$v]}"
  wait "${MOUNT_PID[$v]}" || fail "vol-$v did not exit cleanly"
  unset "MOUNT_PID[$v]"
  pass "vol-$v stopped"
done
