#!/usr/bin/env bash
# Actual bounded filesystem ENOSPC on the tree state, Linux CI only.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
base=${TREE_ENOSPC_DIR:-${TMPDIR:-/tmp}}
mkdir -p "$base"
work=$(mktemp -d "$base/agentfs-tree-enospc.XXXXXX")
work=$(realpath "$work")
printf "artifacts: %s\n" "$work"
mkdir -p "$work/volume" "$work/main" "$work/sandbox"
pid=
cleanup() {
  [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  for m in "$work/main" "$work/sandbox"; do fusermount3 -uz "$m" 2>/dev/null || true; done
  mountpoint -q "$work/volume" && sudo umount "$work/volume" || true
}
trap cleanup EXIT
expect_stopped() {
  local log=$1 status=0
  wait "$pid" || status=$?
  pid=
  if [ "$status" -ne 1 ] || ! grep -Fq 'actor stopped after a recording/save failure; reopen to recover' "$log"; then
    echo "expected actor fail-stop exit 1; got $status" >&2
    cat "$log" >&2
    return 1
  fi
}
wait_mounted() {
  local m
  for m in "$@"; do
    for _ in $(seq 1 200); do
      if mountpoint -q "$m"; then break; fi
      kill -0 "$pid"; sleep .1
    done
    mountpoint -q "$m"
  done
}
sudo mount -t tmpfs -o size=32m,mode=0777 tmpfs "$work/volume"
"$bin" --state "$work/volume/state" trees init --volume t
"$bin" --state "$work/volume/state" trees mount "$work/main" --sandbox "$work/sandbox" >"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main" "$work/sandbox"
python3 - "$work/main" "$work/volume/filler" "$work/receipt" <<'PY'
import errno, os, sys
root, filler, receipt = sys.argv[1:]
fd = os.open(root + '/file', os.O_CREAT | os.O_WRONLY, 0o600)
assert os.write(fd, b'acknowledged') == 12
os.fsync(fd)
with open(receipt, 'w') as f:
    f.write('write fsync\n'); f.flush(); os.fsync(f.fileno())
# The failure must come from the actual state filesystem, not a failpoint.
with open(filler, 'wb', buffering=0) as f:
    try:
        while True: f.write(b'x' * 65536)
    except OSError as e:
        assert e.errno == errno.ENOSPC, e
    # Exhaust any final partial block as well.
    try:
        while True: f.write(b'x')
    except OSError as e:
        assert e.errno == errno.ENOSPC, e
try:
    # A short write may fit in the WAL's already allocated final block.
    # Force allocation on the genuinely full state filesystem.
    written = os.write(fd, b'x' * (1024 * 1024))
except OSError as e:
    assert e.errno == errno.ENOSPC, e
else:
    raise AssertionError(f'write returned {written} bytes after state filesystem filled')
try: os.close(fd)
except OSError: pass
PY
# A full state area fails the call and stops nothing: the view still answers.
python3 - "$work/main" <<'PY'
import errno, os, sys
try:
    os.close(os.open(sys.argv[1] + '/while-full', os.O_CREAT | os.O_WRONLY, 0o600))
except OSError as e:
    assert e.errno == errno.ENOSPC, e
else:
    raise AssertionError('create succeeded while the state filesystem was full')
assert os.listdir(sys.argv[1]) == ['file'], os.listdir(sys.argv[1])
PY
rm "$work/volume/filler"
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
status=0
wait "$pid" || status=$?
pid=
if [ "$status" != 0 ]; then echo "mount exited $status after a full state area" >&2; cat "$work/mount.log" >&2; exit 1; fi
grep -Fq 'tree SaveFailed' "$work/mount.log" || { cat "$work/mount.log" >&2; exit 1; }
"$bin" --state "$work/volume/state" trees init --volume t
gitdir=$work/volume/state/history.git
[ "$(tree_git_file "$gitdir" root file)" = acknowledged ]
git --git-dir="$gitdir" fsck --full --strict --no-progress
tree_git_no_duplicate_opens "$gitdir"
head=$(git --git-dir="$gitdir" rev-parse root)
"$bin" --state "$work/volume/state" trees init --volume t
[ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
printf 'PASS\tenospc\twrite fsync\n' | tee "$work/results.tsv"
# Run a rename held at `point` while the real filesystem fills, then release it.
# Filling must finish before the stalled rename is released. Main only: with no
# sandbox no queue entry can take the failpoint before the rename does.
rename_while_full() {
  local point=$1 log=$work/$1.log client
  rm -rf "$work/volume/state"
  "$bin" --state "$work/volume/state" trees init --volume t
  "$bin" --state "$work/volume/state" trees mount "$work/main" >"$work/setup-mount.log" 2>&1 & pid=$!
  wait_mounted "$work/main"
  python3 - "$work/main/source" <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_CREAT | os.O_WRONLY, 0o600)
assert os.write(fd, b'git-acknowledged') == 16
os.fsync(fd)
os.close(fd)
PY
  fusermount3 -u "$work/main"
  wait "$pid"; pid=
  AGENTFS_FAILPOINTS="$point=sleep(30000)" "$bin" --state "$work/volume/state" trees mount "$work/main" >"$log" 2>&1 & pid=$!
  wait_mounted "$work/main"
  python3 - "$work/main/source" "$work/main/destination" "$2" <<'PY' >"$work/rename.log" 2>&1 & client=$!
import errno, os, sys
source, destination, want = sys.argv[1:]
got = 0
try:
    os.rename(source, destination)
except OSError as e:
    got = e.errno
assert got == getattr(errno, want, 0), f'rename answered {got}, wanted {want}'
PY
  for _ in $(seq 1 200); do
    if grep -Fq "agentfs failpoint $point triggered (sleep(30000))" "$log"; then break; fi
    kill -0 "$pid"; kill -0 "$client"; sleep .1
  done
  grep -Fq "agentfs failpoint $point triggered (sleep(30000))" "$log"
  python3 - "$work/volume/filler" <<'PY'
import errno, sys
with open(sys.argv[1], 'wb', buffering=0) as f:
    try:
        while True: f.write(b'x' * 65536)
    except OSError as e:
        assert e.errno == errno.ENOSPC, e
    try:
        while True: f.write(b'x')
    except OSError as e:
        assert e.errno == errno.ENOSPC, e
PY
  kill -0 "$client" || { echo 'rename returned before filesystem was filled' >&2; exit 1; }
  if ! wait "$client"; then cat "$work/rename.log" >&2; exit 1; fi
  rm "$work/volume/filler"
  fusermount3 -u "$work/main"
  # Neither failure stops the actor.
  wait "$pid"; pid=
  "$bin" --state "$work/volume/state" trees init --volume t
  gitdir=$work/volume/state/history.git
  [ "$(tree_git_file "$gitdir" root "$3")" = git-acknowledged ]
  git --git-dir="$gitdir" fsck --full --strict --no-progress
  tree_git_no_duplicate_opens "$gitdir"
  head=$(git --git-dir="$gitdir" rev-parse root)
  "$bin" --state "$work/volume/state" trees init --volume t
  [ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
}
# Objects cannot be written: nothing is recorded, ENOSPC, the name stays.
rename_while_full tree.before_objects ENOSPC source
printf 'PASS\tenospc-before-commit-point\trename ENOSPC; source\n' | tee -a "$work/results.tsv"
# The record stands and only the copies fail: the call succeeded.
rename_while_full tree.after_commit_point NONE destination
grep -Fq 'tree CopyStale' "$work/tree.after_commit_point.log" || { cat "$work/tree.after_commit_point.log" >&2; exit 1; }
printf 'PASS\tenospc-after-commit-point\trename 0; destination\n' | tee -a "$work/results.tsv"
# Only a failed fsync stops the actor: the file can no longer be trusted. A
# write appends with no fsync, so the client's fsync is the one that fails.
rm -rf "$work/volume/state"
"$bin" --state "$work/volume/state" trees init --volume t
AGENTFS_FAILPOINTS='tree.wal_before_fsync=error' "$bin" --state "$work/volume/state" trees mount "$work/main" >"$work/fsync-mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
python3 - "$work/main/synced" "$work/main/after-fsync" <<'PY'
import errno, os, sys
def expect_eio(what, call):
    try:
        call()
    except OSError as e:
        assert e.errno == errno.EIO, (what, e)
    else:
        raise AssertionError(f'{what} succeeded after a failed fsync')
fd = os.open(sys.argv[1], os.O_CREAT | os.O_WRONLY, 0o600)
assert os.write(fd, b'x') == 1
expect_eio('fsync', lambda: os.fsync(fd))
expect_eio('close', lambda: os.close(fd))
expect_eio('create', lambda: os.close(os.open(sys.argv[2], os.O_CREAT | os.O_WRONLY, 0o600)))
PY
fusermount3 -u "$work/main"
expect_stopped "$work/fsync-mount.log"
"$bin" --state "$work/volume/state" trees init --volume t
git --git-dir="$work/volume/state/history.git" fsck --full --strict --no-progress
printf 'PASS\tfailed-fsync-stops\tEIO; stopped\n' | tee -a "$work/results.tsv"
