#!/usr/bin/env bash
# Kill the tree actor at each recording/publication failpoint. Linux FUSE CI.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
base=${TREE_FAULT_DIR:-${TMPDIR:-/tmp}}
mkdir -p "$base"
work=$(mktemp -d "$base/agentfs-tree-faults.XXXXXX")
work=$(realpath "$work")
printf "artifacts: %s\n" "$work"
printf 'status\tpoint\tlast_receipt\n' > "$work/results.tsv"
pid=
cleanup() {
  [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  for m in "$work/main" "$work/sandbox"; do fusermount3 -uz "$m" 2>/dev/null || true; done
}
trap cleanup EXIT
start() {
  local spec=$1
  AGENTFS_FAILPOINTS="$spec" "$bin" --state "$work/state" trees mount "$work/main" --sandbox "$work/sandbox" --fork "$fork" >"$work/mount.log" 2>&1 & pid=$!
  for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then return; fi
    kill -0 "$pid" || { cat "$work/mount.log" >&2; return 1; }
    sleep .1
  done
  echo 'mount startup timed out' >&2; return 1
}
# Takes run on the view's worker. Let them finish before the kill, so the failpoint
# meets the client's call rather than a leftover take.
drain_queue() {
  for _ in $(seq 1 300); do
    [ -n "$(git --git-dir="$work/state/history.git" for-each-ref refs/queue)" ] || return 0
    sleep .1
  done
  echo 'queue did not drain' >&2; return 1
}
points=(tree.wal_before_header tree.wal_after_header tree.wal_after_payload tree.wal_before_fsync tree.wal_after_fsync tree.before_objects tree.after_objects_durable tree.after_commit_point tree.after_ref_copy tree.before_checkout tree.after_checkout)
for point in "${points[@]}"; do
  rm -rf "$work/state" "$work/main" "$work/sandbox"
  mkdir "$work/main" "$work/sandbox"
  "$bin" --state "$work/state" trees init
  fork=$("$bin" --state "$work/state" trees fork)
  start ''
  # Establish an independently observed acknowledged write before the kill.
  : > "$work/baseline.receipt"
  python3 - "$work/main/baseline" "$work/baseline.receipt" <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_CREAT | os.O_WRONLY, 0o600)
def receipt(s):
    with open(sys.argv[2], 'a') as f:
        f.write(s + '\n'); f.flush(); os.fsync(f.fileno())
assert os.write(fd, b'baseline') == 8
receipt('write')
os.fsync(fd); receipt('fsync')
os.close(fd); receipt('close')
PY
  : > "$work/main/candidate"
  drain_queue
  fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
  wait "$pid"; pid=
  case "$point" in
    tree.wal_*) spec="$point=kill@2" ;; # The open is the first record.
    *) spec="$point=kill" ;;
  esac
  start "$spec"
  : > "$work/receipt"
  python3 - "$work/main/candidate" "$work/receipt" <<'PY' >"$work/client.log" 2>&1 & client=$!
import os, sys
fd = os.open(sys.argv[1], os.O_WRONLY, 0o600)
def receipt(s):
    with open(sys.argv[2], 'a') as f:
        f.write(s + '\n'); f.flush(); os.fsync(f.fileno())
n = os.write(fd, b'candidate')
if n: receipt(f'write {n}')
assert n == 9, f'partial write returned {n}'
os.fsync(fd); receipt('fsync')
os.close(fd); receipt('close')
PY
  # Wait for the requested failpoint, and require evidence that it fired.
  for _ in $(seq 1 300); do kill -0 "$pid" 2>/dev/null || break; sleep .1; done
  if kill -0 "$pid" 2>/dev/null; then echo "$point did not fire" >&2; exit 1; fi
  wait "$pid" 2>/dev/null || true; pid=
  wait "$client" 2>/dev/null || true
  grep -F "agentfs failpoint $point triggered (kill)" "$work/mount.log" >/dev/null || { cat "$work/mount.log" >&2; exit 1; }
  fusermount3 -uz "$work/main"; fusermount3 -uz "$work/sandbox"
  "$bin" --state "$work/state" trees init
  gitdir=$work/state/history.git
  [ "$(tree_git_file "$gitdir" root baseline)" = baseline ]
  if grep -q "^write " "$work/receipt"; then
    count=$(awk '/^write / {print $2}' "$work/receipt")
    written=candidate
    [ "$(tree_git_file "$gitdir" root candidate)" = "${written:0:count}" ]
  fi
  git --git-dir="$gitdir" fsck --full --strict --no-progress
  tree_git_no_duplicate_opens "$gitdir"
  head=$(git --git-dir="$gitdir" rev-parse root)
  "$bin" --state "$work/state" trees init
  [ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
  printf 'PASS\t%s\t%s\n' "$point" "$(tail -n 1 "$work/receipt")" | tee -a "$work/results.tsv"
done
# The failpoints a mounted process cannot reach: each opening of the state area
# passes them. Killed there, the next open still finds every returned write.
for point in tree.after_ref_rebuild tree.compact_before_rename tree.compact_after_rename; do
  rm -rf "$work/state" "$work/main" "$work/sandbox"
  mkdir "$work/main" "$work/sandbox"
  "$bin" --state "$work/state" trees init
  fork=$("$bin" --state "$work/state" trees fork)
  start ''
  printf saved > "$work/main/kept"
  fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
  wait "$pid"; pid=
  status=0
  AGENTFS_FAILPOINTS="$point=kill" "$bin" --state "$work/state" trees init >"$work/open.log" 2>&1 || status=$?
  [ "$status" = 137 ] || { echo "$point did not kill: $status" >&2; cat "$work/open.log" >&2; exit 1; }
  grep -F "agentfs failpoint $point triggered (kill)" "$work/open.log" >/dev/null
  "$bin" --state "$work/state" trees init
  gitdir=$work/state/history.git
  [ "$(tree_git_file "$gitdir" root kept)" = saved ]
  git --git-dir="$gitdir" fsck --full --strict --no-progress
  tree_git_no_duplicate_opens "$gitdir"
  printf 'PASS\t%s\t-\n' "$point" | tee -a "$work/results.tsv"
done
# A first init killed before its `format` starts over.
rm -rf "$work/state"
status=0
AGENTFS_FAILPOINTS=tree.init_before_format=kill "$bin" --state "$work/state" trees init >"$work/open.log" 2>&1 || status=$?
[ "$status" = 137 ] || { echo "tree.init_before_format did not kill: $status" >&2; cat "$work/open.log" >&2; exit 1; }
[ -e "$work/state/initializing" ]
"$bin" --state "$work/state" trees init
[ ! -e "$work/state/initializing" ]
git --git-dir="$work/state/history.git" fsck --full --strict --no-progress
printf 'PASS\ttree.init_before_format\t-\n' | tee -a "$work/results.tsv"
# Crash with a replaced object still held open: its acknowledged writes must
# never be replayed onto the new object at that pathname.
rm -rf "$work/state" "$work/main" "$work/sandbox"
mkdir "$work/main" "$work/sandbox"
"$bin" --state "$work/state" trees init
fork=$("$bin" --state "$work/state" trees fork)
start ''
python3 - "$work/main" "$work/orphan-receipt" <<'PY' >"$work/orphan-client.log" 2>&1 & client=$!
import os, sys, time
root, receipt = sys.argv[1:]
with open(root + '/old', 'wb', buffering=0) as f: f.write(b'old')
old = os.open(root + '/old', os.O_WRONLY)
with open(root + '/replacement', 'wb', buffering=0) as f: f.write(b'replacement')
os.replace(root + '/replacement', root + '/old')
assert os.write(old, b'ORPHAN-BAD') == 10
os.fsync(old)
with open(receipt, 'w') as f:
    f.write('write fsync\n'); f.flush(); os.fsync(f.fileno())
while True: time.sleep(1)
PY
for _ in $(seq 1 200); do
  [ ! -f "$work/orphan-receipt" ] || break
  kill -0 "$client"; sleep .1
done
[ -f "$work/orphan-receipt" ]
kill -KILL "$pid"; wait "$pid" 2>/dev/null || true; pid=
kill "$client"; wait "$client" 2>/dev/null || true
fusermount3 -uz "$work/main"; fusermount3 -uz "$work/sandbox"
"$bin" --state "$work/state" trees init
gitdir=$work/state/history.git
[ "$(tree_git_file "$gitdir" root old)" = replacement ]
git --git-dir="$gitdir" rev-list --all > "$work/orphan-commits"
while read -r commit; do
  prefix=
  if git --git-dir="$gitdir" show -s --format=%B "$commit" | grep -Fx 'Agentfs-Format: envelope-1' >/dev/null; then prefix=:files; fi
  if git --git-dir="$gitdir" grep -F 'ORPHAN-BAD' "$commit$prefix" >/dev/null; then
    echo "orphan bytes entered history at $commit" >&2; exit 1
  else
    status=$?
    [ "$status" = 1 ] || { echo "git grep failed at $commit: $status" >&2; exit 1; }
  fi
done < "$work/orphan-commits"
tree_git_no_duplicate_opens "$gitdir"
head=$(git --git-dir="$gitdir" rev-parse root)
"$bin" --state "$work/state" trees init
[ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
git --git-dir="$gitdir" fsck --full --strict --no-progress
printf 'PASS\treplaced-open-object\twrite fsync\n' | tee -a "$work/results.tsv"
