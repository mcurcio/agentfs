#!/usr/bin/env bash
# End-to-end recovery (design §15, §18 test 11) through a real FUSE mount,
# following docs/runbooks/recovery-and-undo.md: find a change, restore a
# file and revert a commit while mounted, undo a restore, then, with the
# workload stopped, the plain-git alternative and a direct-mode restore,
# and finally push the restore commits to a remote.
#
# Needs Linux with /dev/fuse, fuse3, git, setpriv and passwordless sudo;
# reproducible on any such host:
#
#   AGENTFS_TEST_PASSTHROUGH=auto ci/e2e-recovery.sh

. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"

step "build"
cargo build --locked -p agentfs

W=$(mktemp -d)
chmod 755 "$W"
STATE=$W/state
BACKING=$W/backing
MNT=$W/mnt
REMOTE=$W/remote.git
LOG=$W/mount.log
mkdir -p "$BACKING" "$MNT"
chmod 755 "$BACKING" "$MNT"
MOUNT_PID=

af() { sudo "$AGENTFS_BIN" --state "$STATE" "$@"; }
sgit() { sudo git --git-dir "$STATE/git" "$@"; }
as_agent() { # script: runs in the mount as uid 10000, like the workload
  sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- sh -c "cd '$MNT' && $1"
}
head_of() { sgit rev-parse refs/heads/agentfs/demo; }
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
  sudo "$AGENTFS_BIN" --state "$STATE" mount \
    --passthrough "${AGENTFS_TEST_PASSTHROUGH:-auto}" "$BACKING" "$MNT" >>"$LOG" 2>&1 &
  MOUNT_PID=$!
  wait_for 20 mountpoint -q "$MNT" || fail "never mounted"
  wait_for 20 grep -q '^mounted ' "$LOG" || fail "no startup line"
}

stop_mount() {
  sudo kill -TERM "$MOUNT_PID"
  wait "$MOUNT_PID" || fail "mount did not exit cleanly"
  MOUNT_PID=
}

# Waits until the branch holds `content` at `path`.
wait_history() { # path content
  wait_for 20 sh -c "[ \"\$(sudo git --git-dir '$STATE/git' show refs/heads/agentfs/demo:$1 2>/dev/null)\" = '$2' ]" ||
    fail "history never had $1 = $2"
}

step "a volume with a remote, mounted"
af init --volume demo >/dev/null
sgit config agentfs.commitWindow 50ms
sudo git init -q --bare "$REMOTE"
sgit config remote.origin.url "$REMOTE"
start_mount
sudo chown 10000:10000 "$MNT"

step "the workload writes, then breaks something"
as_agent 'mkdir -p notes && printf "good\n" > notes/plan.md && printf "keep\n" > notes/todo.md'
wait_history notes/plan.md good
GOOD=$(head_of)
as_agent 'printf "broken\n" > notes/plan.md && printf "junk\n" > notes/junk.md'
wait_history notes/junk.md junk
BAD=$(head_of)

step "find the change (runbook: finding a change)"
FOUND=$(sgit log --format='%H %s' --follow refs/heads/agentfs/demo -- notes/plan.md)
echo "$FOUND"
assert_contains "$FOUND" "$GOOD" "git log --follow lists the good version"
assert_contains "$FOUND" "$BAD" "and the bad one"
assert_contains "$(sgit log -1 --format=%B "$BAD")" "Agentfs-Uid: 10000" \
  "trailers name the writer"

step "restore a file while mounted"
OUT=$(af restore notes/plan.md "$GOOD")
echo "$OUT"
assert_contains "$OUT" "write notes/plan.md" "the plan is listed"
assert_eq "good" "$(cat "$MNT/notes/plan.md")" "the mount shows the restored file at once"
wait_history notes/plan.md good
RESTORE=$(head_of)
MSG=$(sgit log -1 --format=%B "$RESTORE")
assert_contains "$MSG" "Agentfs-Trigger: restore" "recorded as a restore"
assert_contains "$MSG" "Agentfs-Restore: $GOOD" "naming the source commit"
assert_contains "$OUT" "commit $RESTORE" "the reply names the commit"

step "revert a commit while mounted"
OUT=$(af revert "$BAD")
echo "$OUT"
[ ! -e "$MNT/notes/junk.md" ] || fail "notes/junk.md is still there"
pass "the reverted commit's new file is gone"
REVERT=$(head_of)
assert_contains "$(sgit log -1 --format=%B "$REVERT")" "Agentfs-Revert: $BAD" \
  "recorded as a revert"

step "a refusal changes nothing"
set +e
OUT=$(af restore notes/missing.md "$GOOD" 2>&1)
CODE=$?
set -e
assert_eq 1 "$CODE" "a refused restore exits 1"
assert_contains "$OUT" "nothing at notes/missing.md" "and says why"

step "undo the restore: revert the restore commit"
af revert "$RESTORE"
assert_eq "broken" "$(cat "$MNT/notes/plan.md")" "the restore is undone"
wait_history notes/plan.md broken

step "pause the workload: stop the mount"
stop_mount

step "plain git, with the workload stopped (runbook: plain-git alternatives)"
sudo sh -c "git --git-dir '$STATE/git' show '$GOOD:notes/plan.md' > '$BACKING/notes/plan.md'"
af --root "$BACKING" record notes/plan.md
wait_history notes/plan.md good

step "restore in direct mode, with the workload stopped"
OUT=$(af --root "$BACKING" restore notes "$BAD")
echo "$OUT"
assert_eq "broken" "$(sudo cat "$BACKING/notes/plan.md")" "the backing tree is written"
assert_eq "junk" "$(sudo cat "$BACKING/notes/junk.md")" "and the removed file is back"
assert_contains "$(sgit log -1 --format=%B refs/heads/agentfs/demo)" "Agentfs-Restore: $BAD" \
  "recorded as a restore"

step "remount: the tree and history agree"
start_mount
assert_eq "broken" "$(cat "$MNT/notes/plan.md")" "the mount shows the restored state"
stop_mount

step "push: the restore commits reach the remote"
af push
REMOTE_LOG=$(git --git-dir "$REMOTE" log --format=%B refs/heads/agentfs/demo)
assert_contains "$REMOTE_LOG" "Agentfs-Restore: $GOOD" "the mounted restore was pushed"
assert_contains "$REMOTE_LOG" "Agentfs-Revert: $BAD" "and the revert"

step "recovery passed"
