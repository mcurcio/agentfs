#!/usr/bin/env bash
# End-to-end SQLite in the fsync class (design §8) through a real FUSE
# mount, following docs/operator-guide.md ("Databases: the fsync class")
# and docs/runbooks/recovery-and-undo.md ("Restoring a database"):
#
# 1. a WAL database (synchronous=NORMAL, so commits do not fsync) and a
#    DELETE-mode one are written by long-lived writers;
# 2. after the WAL writer's last commit, while it still holds the
#    database open, the history catches up within one agentfs.coalesce
#    interval (plus the commit window and slack): the trailing capture;
# 3. every recorded version of each database, restored with its -wal or
#    -journal from the same commit, passes PRAGMA integrity_check and holds
#    whole transactions, and no -shm is ever recorded;
# 4. a database restored from an older version, with the workload stopped,
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
CHECK=$(cd "$(dirname "$0")" && pwd)/sqlite-check.py
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

# Rows in the database at refs/heads/agentfs/demo:<path>, with its -wal
# or -journal from the same commit; empty if it is not there yet.
head_rows() { # path
  sudo python3 "$CHECK" "$STATE/git" refs/heads/agentfs/demo "$1" --min 0 2>/dev/null |
    head -n 1 | cut -d' ' -f2
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
as_agent "python3 '$W/sqlite-writer.py' wal/app.db --mode wal --txs 20 --rows $ROWS --pause 0.25 --idle 20" \
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
wait_for "$LIMIT" sh -c "[ \"\$(sudo python3 '$CHECK' '$STATE/git' refs/heads/agentfs/demo wal/app.db --min 0 2>/dev/null | head -n 1 | cut -d' ' -f2)\" = '$WANT' ]" ||
  fail "the history did not reach $WANT rows within ${LIMIT}s (has '$(head_rows wal/app.db)')"
pass "the history caught up in $((SECONDS - DONE_AT))s (limit ${LIMIT}s), writer still open"
wait "$WAL_PID" || fail "the WAL writer failed: $(cat "$W/wal.out")"

step "a DELETE-mode database"
as_agent "python3 '$W/sqlite-writer.py' delete/app.db --mode delete --txs 20 --rows $ROWS --pause 0.2" \
  >"$W/delete.out" 2>&1 || fail "the DELETE writer failed: $(cat "$W/delete.out")"
WANT_DELETE=$(sed -n 's/^done //p' "$W/delete.out")
wait_for 20 sh -c "[ \"\$(sudo python3 '$CHECK' '$STATE/git' refs/heads/agentfs/demo delete/app.db --min 0 2>/dev/null | head -n 1 | cut -d' ' -f2)\" = '$WANT_DELETE' ]" ||
  fail "the history never reached $WANT_DELETE rows (has '$(head_rows delete/app.db)')"
pass "the closed database is recorded whole"

step "stop the mount"
stop_mount
grep -q 'alarm:' "$LOG" && fail "the daemon raised an alarm: $(grep 'alarm:' "$LOG")"
pass "no alarms"

step "every recorded version is whole"
for db in wal/app.db delete/app.db; do
  echo "-- $db"
  sudo python3 "$CHECK" "$STATE/git" refs/heads/agentfs/demo "$db" --rows "$ROWS" --min 2 |
    tee "$W/versions-${db%%/*}" || fail "a damaged version of $db"
  pass "every version of $db passes integrity_check"
done
HEAD_TREE=$(sgit ls-tree -r --name-only refs/heads/agentfs/demo)
case "$HEAD_TREE" in *-shm*) fail "a -shm was recorded: $HEAD_TREE" ;; esac
pass "no -shm in the head"

step "restore an older version (runbook: restoring a database)"
# An intermediate version: the newest with some rows but not all.
read -r OLD OLD_ROWS _ < <(awk -v want="$WANT" '$2 > 0 && $2 < want' "$W/versions-wal" | head -n 1) || true
[ -n "$OLD" ] && [ "$OLD_ROWS" -lt "$WANT" ] || fail "no older version to restore"
OUT=$(af --root "$BACKING" restore wal "$OLD")
echo "$OUT"
sudo rm -f "$BACKING/wal/app.db-shm"
GOT=$(sudo python3 -c 'import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
assert con.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
print(con.execute("SELECT count(*) FROM t").fetchone()[0])' "$BACKING/wal/app.db")
assert_eq "$OLD_ROWS" "$GOT" "the restored database opens with the old version's rows"
assert_contains "$(sgit log -1 --format=%B refs/heads/agentfs/demo)" "Agentfs-Restore: $OLD" \
  "recorded as a restore"

step "remount: the workload sees the restored database"
start_mount
GOT=$(as_agent "python3 -c 'import sqlite3; print(sqlite3.connect(\"wal/app.db\").execute(\"SELECT count(*) FROM t\").fetchone()[0])'")
assert_eq "$OLD_ROWS" "$GOT" "through the mount"
stop_mount

step "sqlite passed"
