#!/usr/bin/env bash
# End-to-end SQLite in the fsync class (design §8) through a real FUSE
# mount, following docs/operator-guide.md ("Databases: the fsync class")
# and docs/runbooks/recovery-and-undo.md ("Restoring a database"):
#
# 1. a WAL database (synchronous=NORMAL, so commits do not fsync) and a
#    DELETE-mode one are written by long-lived writers;
# 2. after the WAL writer's last commit, while it still holds the
#    database open, the history catches up within one agentfs.coalesce
#    interval (plus the commit window and slack): the trailing capture
#    (trigger interval). This also proves the class's writes reach the
#    daemon under every --passthrough setting: through kernel passthrough
#    the daemon would never see them;
# 3. captures are rate-limited: consecutive fsync and interval captures
#    of a database are at least one interval apart, the DELETE-mode
#    database (which opens, fsyncs and closes its -journal on every
#    transaction) is captured a bounded number of times (trigger fsync),
#    and each database has several versions;
# 4. a live restore of the DELETE database's main file through the mount
#    (the control socket) brings back its whole group from one commit;
# 5. every recorded version of each database, including those the
#    restore made, restored with its -wal or -journal from the same
#    commit, passes PRAGMA integrity_check and holds whole transactions,
#    and no -shm is ever recorded;
# 6. a database restored from an older version, with the workload stopped,
#    opens with that version's rows.
#
# Needs Linux with /dev/fuse, fuse3, git, python3 (with sqlite3), setpriv
# and passwordless sudo; reproducible on any such host:
#
#   AGENTFS_TEST_PASSTHROUGH=auto ci/e2e-sqlite.sh

. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: this test needs FUSE"
sudo -n true 2>/dev/null || fail "this test needs passwordless sudo"
command -v python3 >/dev/null || fail "python3 is needed"

COALESCE=2
ROWS=50
TXS=40

step "build"
cargo build --locked -p agentfs

W=$(mktemp -d)
chmod 755 "$W"
STATE=$W/state
BACKING=$W/backing
MNT=$W/mnt
LOG=$W/mount.log
mkdir -p "$BACKING" "$MNT"
chmod 755 "$BACKING" "$MNT"
# The workload runs as uid 10000: give it its own copy of the writer.
install -m 755 "$(dirname "$0")/sqlite-writer.py" "$W/sqlite-writer.py"
CI_DIR=$(cd "$(dirname "$0")" && pwd)
CHECK=$CI_DIR/sqlite-check.py
CAPTURES=$CI_DIR/sqlite-captures.py
BRANCH=refs/heads/agentfs/demo
MOUNT_PID=

af() { sudo "$AGENTFS_BIN" --state "$STATE" "$@"; }
sgit() { sudo git --git-dir "$STATE/git" "$@"; }
as_agent() { # script: runs in the mount as uid 10000, like the workload
  sudo setpriv --reuid=10000 --regid=10000 --clear-groups -- sh -c "cd '$MNT' && $1"
}
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

# Rows in the database at $BRANCH:<path>, with its -wal or -journal from
# the same commit; empty if it is not there yet.
head_rows() { # path
  sudo python3 "$CHECK" "$STATE/git" "$BRANCH" "$1" --min 0 2>/dev/null |
    head -n 1 | cut -d' ' -f2
}

# Rows a workload sees in the database at <path> through the mount.
mount_rows() { # path
  as_agent "python3 -c 'import sqlite3; print(sqlite3.connect(\"$1\").execute(\"SELECT count(*) FROM t\").fetchone()[0])'"
}

step "a volume with SQLite databases in the fsync class, mounted"
af init --volume demo >/dev/null
sgit config agentfs.commitWindow 200ms
# The operator's file (design §4.3): every member of a database's group.
sudo tee "$STATE/git/info/attributes" >/dev/null <<ATTR
*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=${COALESCE}s
ATTR
start_mount
sudo chown 10000:10000 "$MNT"
as_agent 'mkdir wal delete'

step "a WAL database: commits do not fsync, the interval captures"
as_agent "python3 '$W/sqlite-writer.py' wal/app.db --mode wal --txs $TXS --rows $ROWS --pause 0.25 --idle 20" \
  >"$W/wal.out" 2>&1 &
WAL_PID=$!
wait_for 60 grep -q '^done ' "$W/wal.out" || fail "the WAL writer never finished: $(cat "$W/wal.out")"
DONE_AT=$SECONDS
WANT=$(sed -n 's/^done //p' "$W/wal.out")
# Still open: the last commits are only in the WAL, and nothing fsyncs.
[ -e "$MNT/wal/app.db-wal" ] || fail "no WAL while the writer is open"
pass "the writer holds the database open with a WAL"
# At most one interval late: coalesce after the last write, then the
# commit window; allow slack for a loaded runner.
LIMIT=$((COALESCE + 6))
wait_for "$LIMIT" sh -c "[ \"\$(sudo python3 '$CHECK' '$STATE/git' $BRANCH wal/app.db --min 0 2>/dev/null | head -n 1 | cut -d' ' -f2)\" = '$WANT' ]" ||
  fail "the history did not reach $WANT rows within ${LIMIT}s (has '$(head_rows wal/app.db)')"
pass "the history caught up in $((SECONDS - DONE_AT))s (limit ${LIMIT}s), writer still open"
wait "$WAL_PID" || fail "the WAL writer failed: $(cat "$W/wal.out")"

step "a DELETE-mode database"
STARTED=$SECONDS
as_agent "python3 '$W/sqlite-writer.py' delete/app.db --mode delete --txs $TXS --rows $ROWS --pause 0.2" \
  >"$W/delete.out" 2>&1 || fail "the DELETE writer failed: $(cat "$W/delete.out")"
ELAPSED=$((SECONDS - STARTED + 1))
WANT_DELETE=$(sed -n 's/^done //p' "$W/delete.out")
wait_for 20 sh -c "[ \"\$(sudo python3 '$CHECK' '$STATE/git' $BRANCH delete/app.db --min 0 2>/dev/null | head -n 1 | cut -d' ' -f2)\" = '$WANT_DELETE' ]" ||
  fail "the history never reached $WANT_DELETE rows (has '$(head_rows delete/app.db)')"
pass "the closed database is recorded whole"

step "captures are at least one interval apart"
sudo python3 "$CAPTURES" "$STATE/git" "$BRANCH" wal/app.db --coalesce "$COALESCE" \
  --min 4 --want interval || fail "the WAL database's captures"
pass "the WAL database: interval captures, spaced"
# $TXS transactions in ${ELAPSED}s: closing the -journal every transaction
# must not capture, so at most one capture per interval, plus the first
# fsync, the last close and slack.
MAX=$((ELAPSED / COALESCE + 3))
sudo python3 "$CAPTURES" "$STATE/git" "$BRANCH" delete/app.db --coalesce "$COALESCE" \
  --min 3 --max "$MAX" --want fsync || fail "the DELETE database's captures"
pass "the DELETE database: fsync captures, spaced, at most $MAX for $TXS transactions"

step "restore the DELETE database live, through the mount"
sudo python3 "$CHECK" "$STATE/git" "$BRANCH" delete/app.db --rows "$ROWS" --min 0 >"$W/live-versions"
# An older version, with its -journal if one was captured.
read -r OLD_D OLD_D_ROWS OLD_D_MEMBERS < <(
  { grep -- '-journal)' "$W/live-versions" || true; cat "$W/live-versions"; } |
    awk -v want="$WANT_DELETE" '$2 < want' | head -n 1
) || true
[ -n "$OLD_D" ] || fail "no older version of delete/app.db: $(cat "$W/live-versions")"
# No --root: the running mount serves it over its control socket.
OUT=$(af restore delete/app.db "$OLD_D")
echo "$OUT"
assert_contains "$OUT" "commit" "the live restore made a commit"
MSG=$(sgit log -1 --format=%B "$BRANCH")
assert_contains "$MSG" "Agentfs-Restore: $OLD_D" "recorded as one restore"
HEAD_GROUP=$(sgit ls-tree --name-only "$BRANCH" delete/ | tr '\n' ' ')
case "$OLD_D_MEMBERS" in
  *-journal*) assert_contains "$HEAD_GROUP" "delete/app.db-journal" "the -journal came back with the database" ;;
  *) case "$HEAD_GROUP" in *-journal*) fail "a -journal the version did not have: $HEAD_GROUP" ;; esac ;;
esac
assert_eq "$OLD_D_ROWS" "$(head_rows delete/app.db)" "history holds the old version, whole"
assert_eq "$OLD_D_ROWS" "$(mount_rows delete/app.db)" "the workload sees the old version"

step "stop the mount"
stop_mount
grep -q 'alarm:' "$LOG" && fail "the daemon raised an alarm: $(grep 'alarm:' "$LOG")"
pass "no alarms"
if [ "${AGENTFS_TEST_PASSTHROUGH:-auto}" = on ]; then
  grep -q 'passthrough refused\|--passthrough on:' "$LOG" &&
    fail "a passthrough fallback under --passthrough on: $(grep 'passthrough' "$LOG")"
  pass "--passthrough on: no fallback, and the class still went through the daemon (step 2)"
fi

step "every recorded version is whole, the restores' included"
for db in wal/app.db delete/app.db; do
  echo "-- $db"
  sudo python3 "$CHECK" "$STATE/git" "$BRANCH" "$db" --rows "$ROWS" --min 4 |
    tee "$W/versions-${db%%/*}" || fail "a damaged version of $db"
  pass "every version of $db passes integrity_check"
done
HEAD_TREE=$(sgit ls-tree -r --name-only "$BRANCH")
case "$HEAD_TREE" in *-shm*) fail "a -shm was recorded: $HEAD_TREE" ;; esac
pass "no -shm in the head"

step "restore an older version directly (runbook: restoring a database)"
# An intermediate version: the newest with some rows but not all.
read -r OLD OLD_ROWS _ < <(awk -v want="$WANT" '$2 > 0 && $2 < want' "$W/versions-wal" | head -n 1) || true
[ -n "$OLD" ] && [ "$OLD_ROWS" -lt "$WANT" ] || fail "no older version to restore"
# The database alone is named: its group comes with it.
OUT=$(af --root "$BACKING" restore wal/app.db "$OLD")
echo "$OUT"
sudo rm -f "$BACKING/wal/app.db-shm"
GOT=$(sudo python3 -c 'import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
assert con.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
print(con.execute("SELECT count(*) FROM t").fetchone()[0])' "$BACKING/wal/app.db")
assert_eq "$OLD_ROWS" "$GOT" "the restored database opens with the old version's rows"
assert_contains "$(sgit log -1 --format=%B "$BRANCH")" "Agentfs-Restore: $OLD" \
  "recorded as a restore"

step "remount: the workload sees the restored database"
start_mount
assert_eq "$OLD_ROWS" "$(mount_rows wal/app.db)" "through the mount"
stop_mount

step "sqlite passed"
