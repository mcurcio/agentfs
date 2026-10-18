#!/usr/bin/env bash
# End-to-end patch queue (design §12, §18 test 15) against a smart-HTTP
# remote with basic auth and a real FUSE mount, following
# docs/runbooks/patch-queue.md:
#
# - two producers race `agentfs patch submit` and get inbox 1 and 2;
# - three more patches, the second of them (4) conflicting with a change the
#   workload makes through the mount;
# - a remount applies 1 to 3, blocks at 4 and mounts anyway; the push moves
#   the remote's refs and the status ref says why;
# - a crash between the commit and the ref moves applies nothing twice;
# - `agentfs patch submit --replace 4` with a rebased patch unblocks it.
#
# Needs a server from ci/git-http-server.sh and its GIT_* variables, Linux
# with /dev/fuse, fuse3, git and passwordless sudo.

. "$(dirname "$0")/lib.sh"

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh first}"
: "${GIT_SERVER_HTPASSWD:?}" "${GIT_HTTP_BASE:?}"
[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"

W=$(mktemp -d)
chmod 755 "$W"
STATE=$W/state
BACKING=$W/backing
MNT=$W/mnt
LOG=$W/mount.log
mkdir -p "$BACKING" "$MNT"
chmod 755 "$BACKING" "$MNT"
MOUNT_PID=

others=$(grep -v '^producer:' "$GIT_SERVER_HTPASSWD" || true)
{ [ -n "$others" ] && echo "$others"; echo "producer:$(openssl passwd -apr1 pw1)"; } \
  >"$GIT_SERVER_HTPASSWD"
REPO=$GIT_SERVER_REPOS/patches.git
git init -q --bare "$REPO"
URL=$GIT_HTTP_BASE/patches.git
# Producers authenticate with their own git (here: userinfo in the URL).
PRODUCER_URL=${URL/:\/\//://producer:pw1@}
printf 'username=producer\npassword=pw1\n' >"$W/creds"
chmod 600 "$W/creds"

af() { sudo AGENTFS_REPLICA=ci-0 "$AGENTFS_BIN" --state "$STATE" "$@"; }
sgit() { sudo git --git-dir "$STATE/git" "$@"; }
rgit() { git --git-dir "$REPO" "$@"; }
local_head() { sgit rev-parse refs/heads/agentfs/demo; }
cleanup() {
  status=$?
  if [ $status -ne 0 ]; then
    echo "---- daemon log"
    tail -n 100 "$LOG" 2>/dev/null || true
  fi
  if [ -n "$MOUNT_PID" ] && sudo kill -0 "$MOUNT_PID" 2>/dev/null; then
    sudo kill -TERM "$MOUNT_PID" 2>/dev/null || true
    wait "$MOUNT_PID" 2>/dev/null || true
  fi
  mountpoint -q "$MNT" && sudo umount -l "$MNT"
  exit $status
}
trap cleanup EXIT

start_mount() {
  local before
  before=$(grep -c '^mounted ' "$LOG" 2>/dev/null || true)
  sudo AGENTFS_REPLICA=ci-0 "$AGENTFS_BIN" --state "$STATE" mount \
    --passthrough "${AGENTFS_TEST_PASSTHROUGH:-auto}" "$BACKING" "$MNT" >>"$LOG" 2>&1 &
  MOUNT_PID=$!
  wait_for 30 mountpoint -q "$MNT" || fail "never mounted"
  wait_for 30 sh -c "[ \$(grep -c '^mounted ' '$LOG') -gt ${before:-0} ]" ||
    fail "no startup line"
}

stop_mount() {
  sudo kill -TERM "$MOUNT_PID"
  wait "$MOUNT_PID" || fail "mount did not exit cleanly"
  MOUNT_PID=
}

remote_has_head() { [ "$(remote_ref "$REPO" refs/heads/agentfs/demo)" = "$(local_head)" ]; }
wait_pushed() { wait_for 30 remote_has_head || fail "the branch was never pushed"; }
inbox() { printf 'refs/agentfs/demo/inbox/%010d' "$1"; }
applied() { printf 'refs/agentfs/demo/applied/%010d' "$1"; }
remote_inbox() { rgit for-each-ref --format='%(refname)' refs/agentfs/demo/inbox/; }
remote_status() { rgit show refs/agentfs/demo/status:status.json; }
producer() { # dir
  git clone -q "$PRODUCER_URL" "$1"
  git -C "$1" config user.name "Patch Producer"
  git -C "$1" config user.email producer@example.com
}
submit() { # producer-dir args...
  local dir=$1; shift
  "$AGENTFS_BIN" patch submit --volume demo --repo "$dir" "$@"
}

step "a volume with an HTTP remote, mounted and pushed"
af init --volume demo >/dev/null
sgit config agentfs.commitWindow 200ms
sgit config agentfs.pushInterval 1s
sgit config remote.origin.url "$URL"
sgit config remote.origin.agentfsCredentials "$W/creds"
start_mount
sudo sh -c "printf 'a0\n' >'$MNT/a.txt' && printf 'b0\n' >'$MNT/b.txt'"
wait_for 20 sh -c "sudo git --git-dir '$STATE/git' show refs/heads/agentfs/demo:b.txt >/dev/null 2>&1" ||
  fail "never recorded"
wait_pushed
pass "the branch is on the remote"

step "two submitters race for a number"
producer "$W/p1"
producer "$W/p2"
printf 'one\n' >"$W/r1"
printf 'two\n' >"$W/r2"
submit "$W/p1" --put "r1.txt=$W/r1" -m "race one" >"$W/s1.out" 2>&1 &
S1=$!
submit "$W/p2" --put "r2.txt=$W/r2" -m "race two" >"$W/s2.out" 2>&1 &
S2=$!
wait $S1 || { cat "$W/s1.out"; fail "submitter 1 failed"; }
wait $S2 || { cat "$W/s2.out"; fail "submitter 2 failed"; }
I1=$(remote_ref "$REPO" "$(inbox 1)")
I2=$(remote_ref "$REPO" "$(inbox 2)")
[ -n "$I1" ] && [ -n "$I2" ] && [ "$I1" != "$I2" ] || fail "expected inbox 1 and 2, got: $(remote_inbox)"
pass "inbox 1 and 2, one each"

step "three more patches; the workload then changes b.txt"
printf 'a1\n' >"$W/a1"
printf 'b-from-iac\n' >"$W/b1"
printf 'c\n' >"$W/c1"
assert_contains "$(submit "$W/p1" --put "a.txt=$W/a1" -m "three")" "queued patch 3" "patch 3"
assert_contains "$(submit "$W/p1" --put "b.txt=$W/b1" -m "four")" "queued patch 4" "patch 4"
assert_contains "$(submit "$W/p1" --put "c.txt=$W/c1" -m "five")" "queued patch 5" "patch 5"
P3=$(remote_ref "$REPO" "$(inbox 3)")
sudo sh -c "printf 'b-from-workload\n' >'$MNT/b.txt'"
wait_for 20 sh -c "[ \"\$(sudo git --git-dir '$STATE/git' show refs/heads/agentfs/demo:b.txt)\" = b-from-workload ]" ||
  fail "the workload's change was never recorded"
wait_pushed
stop_mount

step "a remount applies 1 to 3, blocks at 4 and mounts anyway"
start_mount
assert_eq "a1" "$(cat "$MNT/a.txt")" "patch 3 is served from the first read"
assert_eq "one" "$(cat "$MNT/r1.txt")" "patch 1 is served"
assert_eq "b-from-workload" "$(cat "$MNT/b.txt")" "the workload's b.txt stays"
[ ! -e "$MNT/c.txt" ] || fail "patch 5 must wait behind 4"
pass "patch 5 waits"
assert_contains "$(cat "$LOG")" "patches: blocked at 4 (conflict)" "the mount says why"
assert_contains "$(cat "$LOG")" "alarm: patch queue blocked at 4" "the PatchBlocked alarm"
applied_on_remote() { [ -n "$(remote_ref "$REPO" "$(applied 3)")" ]; }
wait_for 30 applied_on_remote || fail "applied/3 never reached the remote"
wait_pushed
inbox_left() { [ "$(remote_inbox | wc -l)" = 2 ]; }
wait_for 30 inbox_left || fail "inbox 1 to 3 never deleted: $(remote_inbox)"
assert_eq "$(inbox 4) $(inbox 5)" "$(remote_inbox | tr '\n' ' ' | sed 's/ $//')" "inbox 4 and 5 remain"
assert_eq "$P3" "$(remote_ref "$REPO" "$(applied 3)")" "applied/3 is the patch"
status=$(remote_status)
assert_contains "$status" '"reason": "conflict"' "status.json: conflict"
assert_contains "$status" '"inbox": 4' "status.json: blocked at 4"
assert_contains "$(af status)" "patches:   last applied 3, 2 queued, blocked at 4 (conflict)" \
  "agentfs status shows the queue"

step "a crash between the commit and the ref moves applies nothing twice"
stop_mount
sgit update-ref -d "$(applied 3)"
sgit update-ref "$(inbox 3)" "$P3"
sudo rm -f "$STATE/meta/patch.json"
sgit config agentfs.patchFetchTimeout 0 # keep the restored inbox/3
start_mount
assert_contains "$(cat "$LOG")" "were already applied" "found by its trailer"
wait_pushed
assert_eq 1 "$(rgit log --format=%H --grep '^Agentfs-Inbox: 3$' refs/heads/agentfs/demo | wc -l)" \
  "one Inbox-3 commit on the remote branch"
stop_mount
sgit config --unset agentfs.patchFetchTimeout

step "patch 4, rebased, replaces the blocked one"
git -C "$W/p1" fetch -q origin
printf 'b-merged\n' >"$W/b2"
assert_contains "$(submit "$W/p1" --put "b.txt=$W/b2" --replace 4 -m "four, rebased")" \
  "queued patch 4" "replaced"
start_mount
assert_eq "b-merged" "$(cat "$MNT/b.txt")" "patch 4 applied"
assert_eq "c" "$(cat "$MNT/c.txt")" "patch 5 applied"
inbox_empty() { [ -z "$(remote_inbox)" ]; }
wait_for 30 inbox_empty || fail "the remote inbox never emptied: $(remote_inbox)"
wait_pushed
status_clear() { remote_status | grep -q '"blocked": null'; }
wait_for 30 status_clear || fail "status still blocked: $(remote_status)"
pass "status not blocked"
for n in 1 2 3 4 5; do
  [ -n "$(remote_ref "$REPO" "$(applied "$n")")" ] || fail "no applied/$n on the remote"
  who=$(rgit log --format='%an|%cn' --grep "^Agentfs-Inbox: $n\$" refs/heads/agentfs/demo)
  assert_eq "Patch Producer|ci-0" "$who" "patch $n: authored by the producer, committed by agentfs"
done
stop_mount
pass "patch queue end to end"
