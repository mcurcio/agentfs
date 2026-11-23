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
  [ -z "$pid" ] || tree_stop "$pid"
  for m in "$work/main" "$work/fork"; do fusermount3 -uz "$m" 2>/dev/null || true; done
}
trap cleanup EXIT
# `start SPEC` mounts the root and the keyed fork's view (made at the first
# mount), whose writer it attaches; `start SPEC root` the root alone. The fork's
# one writer is spent at a clean unmount, so a remount after one leaves the
# fork out. Keyed, the fork stays open: no end or drop moves a count. With
# `compact_gate` set to a directory, the mount's compactor parks at each gap of
# every compaction until this script answers on the FIFOs there
# (AGENTFS_COMPACT_GATE, docs/configuration.md).
compact_gate=
start() {
  local spec=$1 views=("$work/main")
  local args=("$work/main" --metrics-output "$work/metrics.json")
  if [ "${2:-}" != root ]; then
    views+=("$work/fork")
    args+=(--fork-mount "$work/fork" --fork-key faults)
  fi
  rm -f "$work/metrics.json"
  env AGENTFS_FAILPOINTS="$spec" ${compact_gate:+AGENTFS_COMPACT_GATE="$compact_gate"} \
    "$bin" --state "$work/state" mount "${args[@]}" >"$work/mount.log" 2>&1 & pid=$!
  for _ in $(seq 1 200); do
    local up=1 view
    for view in "${views[@]}"; do mountpoint -q "$view" || up=; done
    [ -z "$up" ] || return 0
    kill -0 "$pid" || { cat "$work/mount.log" >&2; return 1; }
    sleep .1
  done
  echo 'mount startup timed out' >&2; return 1
}
# Takes run on the view's worker. Let them finish before the kill, so the failpoint
# meets the client's call rather than a leftover take. Git's refs trail the journal
# (docs/trees.md, "History trails the files"): an empty queue in git means drained
# only once the fork's head holds the last save's sync, which took its entry.
drain_queue() {
  local gitdir=$work/state/history.git
  for _ in $(seq 1 300); do
    if tree_git_file "$gitdir" "refs/heads/$fork" "$1" >/dev/null 2>&1 &&
       [ -z "$(git --git-dir="$gitdir" for-each-ref refs/queue)" ]; then
      return 0
    fi
    sleep .1
  done
  echo 'queue did not drain' >&2; return 1
}
# A mount of the root alone, killed once its client's attach is built and the
# mount is at rest (`tree_at_rest`): the volume stays staged (a stage then writes nothing), the
# client attached (a mount then rebinds it) and every pointer switched (a mount
# then appends no switch record before its own first step is built). Returns
# once the killed process's lease is free.
leave_root_attached() {
  local before
  before=$(tree_git_root_clients "$work/state/history.git")
  start '' root
  tree_git_wait_root_attached "$work/state/history.git" "$before"
  tree_at_rest "$work/metrics.json"
  tree_kill_mount "$pid" "$work/mount.log" "$work/main"; pid=
  flock "$work/state/actor.lock" true
}
points=(tree.wal_before_header tree.wal_after_payload tree.wal_before_fsync tree.wal_after_fsync tree.before_step tree.build_before_batch tree.build_after_pack tree.build_after_built_log tree.build_after_ref_copy)
for point in "${points[@]}"; do
  rm -rf "$work/state" "$work/main" "$work/fork"
  mkdir "$work/main" "$work/fork"
  "$bin" --state "$work/state" init --volume t
  start ''
  fork=$(sed -n 's/^fork tree: //p' "$work/mount.log")
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
  drain_queue candidate
  fusermount3 -u "$work/main"; fusermount3 -u "$work/fork"
  tree_wait_exit "$pid"; pid=
  # A clean exit released the volume: a mount now stages it, one step, before
  # its client's attach, the two in one builder batch or two. A killed mount
  # leaves it staged with its root client attached instead.
  leave_root_attached
  # So the mount records one step of its own first, the root client's rebind,
  # and once that step is built, the switch record for the pointers it set.
  # Before its client's first call the mount is at rest (`tree_at_rest`) with:
  #   appends   2  the rebind's record, the switch record
  #   fsyncs    3  the rebind's own, and the builder's before each of its
  #                two batches
  #   steps     1  the rebind (a switch record is no step)
  #   batches   2  the rebind's, the switch record's
  # The client then opens (one append), writes (one append, no fsync), fsyncs
  # (one fsync) and closes (one step, one batch).
  case "$point" in
    # The client's fsync is the fourth.
    tree.wal_*fsync) spec="$point=kill@4"; meets=fsync ;;
    # The open is the third record appended, the write the fourth.
    tree.wal_*) spec="$point=kill@4"; meets=write ;;
    # The close is the second step.
    tree.before_step) spec="$point=kill@2"; meets=close ;;
    # The close's batch is the third.
    *) spec="$point=kill@3"; meets=close ;;
  esac
  before=$(tree_git_root_clients "$work/state/history.git")
  start "$spec" root
  # The rebind's batch and the switch record's are built before the client's
  # first record: else a counted hit would come at another record, or never.
  tree_git_wait_root_attached "$work/state/history.git" "$before"
  tree_at_rest "$work/metrics.json"
  : > "$work/receipt"
  python3 - "$work/main/candidate" "$work/receipt" <<'PY' >"$work/client.log" 2>&1 & client=$!
import os, sys
def receipt(s):
    with open(sys.argv[2], 'a') as f:
        f.write(s + '\n'); f.flush(); os.fsync(f.fileno())
fd = os.open(sys.argv[1], os.O_WRONLY, 0o600)
receipt('open')
n = os.write(fd, b'candidate')
if n: receipt(f'write {n}')
assert n == 9, f'partial write returned {n}'
os.fsync(fd); receipt('fsync')
os.close(fd); receipt('close')
PY
  # Wait for the requested failpoint, and require evidence that it fired.
  for _ in $(seq 1 300); do kill -0 "$pid" 2>/dev/null || break; sleep .1; done
  if kill -0 "$pid" 2>/dev/null; then echo "$point did not fire" >&2; exit 1; fi
  tree_wait_exit "$pid" 2>/dev/null || true; pid=
  wait "$client" 2>/dev/null || true
  grep -F "agentfs failpoint $point triggered (kill)" "$work/mount.log" >/dev/null || { cat "$work/mount.log" >&2; exit 1; }
  # The kill met the call its count names: the receipt holds every call that
  # returned before it, and not that call. A close's step is recorded at the
  # view's RELEASE, which the kernel sends after close(2) has returned, so the
  # receipt may hold the close.
  case "$meets" in
    write) want=open ;;
    fsync) want='open write 9' ;;
    close) want='open write 9 fsync' ;;
  esac
  got=$(tr '\n' ' ' < "$work/receipt" | sed 's/ $//')
  [ "$meets" != close ] || got=${got% close}
  [ "$got" = "$want" ] || { echo "$spec met the wrong call: receipt '$got', expected '$want'" >&2; cat "$work/mount.log" >&2; exit 1; }
  fusermount3 -uz "$work/main"
  "$bin" --state "$work/state" init --volume t
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
  "$bin" --state "$work/state" init --volume t
  [ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
  printf 'PASS\t%s\t%s\n' "$point" "$(tail -n 1 "$work/receipt")" | tee -a "$work/results.tsv"
done
# The failpoint a mounted process cannot reach: each opening of the state area
# passes it. Killed there, the next open still finds every returned write.
for point in tree.after_ref_rebuild; do
  rm -rf "$work/state" "$work/main" "$work/fork"
  mkdir "$work/main" "$work/fork"
  "$bin" --state "$work/state" init --volume t
  start ''
  fork=$(sed -n 's/^fork tree: //p' "$work/mount.log")
  printf saved > "$work/main/kept"
  fusermount3 -u "$work/main"; fusermount3 -u "$work/fork"
  tree_wait_exit "$pid"; pid=
  status=0
  AGENTFS_FAILPOINTS="$point=kill" "$bin" --state "$work/state" init --volume t >"$work/open.log" 2>&1 || status=$?
  [ "$status" = 137 ] || { echo "$point did not kill: $status" >&2; cat "$work/open.log" >&2; exit 1; }
  grep -F "agentfs failpoint $point triggered (kill)" "$work/open.log" >/dev/null
  "$bin" --state "$work/state" init --volume t
  gitdir=$work/state/history.git
  [ "$(tree_git_file "$gitdir" root kept)" = saved ]
  git --git-dir="$gitdir" fsck --full --strict --no-progress
  tree_git_no_duplicate_opens "$gitdir"
  printf 'PASS\t%s\t-\n' "$point" | tee -a "$work/results.tsv"
done
# A kill while history's compactor is parked at each gap of a compaction: the
# checkpoint not yet written, written and not renamed, renamed and its
# directory not fsynced, in place and not yet counted, and counted with the
# segments it covers still linked (docs/trees.md, "Recording and durability").
# The mount's gate gives one frame per segment and a compaction at every built
# event. `kept` was returned before the kill, so it survives whichever
# compaction the kill met.
for gap in covered written renamed dir_synced before_unlink; do
  rm -rf "$work/state" "$work/main" "$work/fork" "$work/gate"
  mkdir "$work/main" "$work/fork" "$work/gate"
  "$bin" --state "$work/state" init --volume t
  mkfifo "$work/gate/at" "$work/gate/go"
  # Read and write: neither end blocks opening, nor sees end of file.
  exec 7<>"$work/gate/at" 8<>"$work/gate/go"
  compact_gate=$work/gate
  start '' root
  compact_gate=
  printf saved > "$work/main/kept"
  while :; do
    IFS= read -r -t 120 reached <&7 || { echo "the compactor did not reach $gap" >&2; cat "$work/mount.log" >&2; exit 1; }
    [ "$reached" != "$gap" ] || break
    echo go >&8
  done
  tree_kill_mount "$pid" "$work/mount.log" "$work/main"; pid=
  exec 7<&- 8>&-
  flock "$work/state/actor.lock" true
  "$bin" --state "$work/state" init --volume t
  gitdir=$work/state/history.git
  [ "$(tree_git_file "$gitdir" root kept)" = saved ]
  git --git-dir="$gitdir" fsck --full --strict --no-progress
  tree_git_no_duplicate_opens "$gitdir"
  head=$(git --git-dir="$gitdir" rev-parse root)
  "$bin" --state "$work/state" init --volume t
  [ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
  printf 'PASS\tcompact:%s\t-\n' "$gap" | tee -a "$work/results.tsv"
done
# A first init killed before its `format` starts over.
rm -rf "$work/state"
status=0
AGENTFS_FAILPOINTS=tree.init_before_format=kill "$bin" --state "$work/state" init --volume t >"$work/open.log" 2>&1 || status=$?
[ "$status" = 137 ] || { echo "tree.init_before_format did not kill: $status" >&2; cat "$work/open.log" >&2; exit 1; }
[ -e "$work/state/initializing" ]
"$bin" --state "$work/state" init --volume t
[ ! -e "$work/state/initializing" ]
git --git-dir="$work/state/history.git" fsck --full --strict --no-progress
printf 'PASS\ttree.init_before_format\t-\n' | tee -a "$work/results.tsv"
# Crash with a replaced object still held open: its acknowledged writes must
# never be replayed onto the new object at that pathname.
rm -rf "$work/state" "$work/main" "$work/fork"
mkdir "$work/main" "$work/fork"
"$bin" --state "$work/state" init --volume t
start ''
fork=$(sed -n 's/^fork tree: //p' "$work/mount.log")
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
tree_kill_mount "$pid" "$work/mount.log" "$work/main" "$work/fork"; pid=
kill "$client"; wait "$client" 2>/dev/null || true
"$bin" --state "$work/state" init --volume t
gitdir=$work/state/history.git
[ "$(tree_git_file "$gitdir" root old)" = replacement ]
git --git-dir="$gitdir" rev-list --all > "$work/orphan-commits"
while read -r commit; do
  prefix=
  if git --git-dir="$gitdir" show -s --format=%B "$commit" | grep -Fx 'Agentfs-Format: envelope-2' >/dev/null; then prefix=:files; fi
  if git --git-dir="$gitdir" grep -F 'ORPHAN-BAD' "$commit$prefix" >/dev/null; then
    echo "orphan bytes entered history at $commit" >&2; exit 1
  else
    status=$?
    [ "$status" = 1 ] || { echo "git grep failed at $commit: $status" >&2; exit 1; }
  fi
done < "$work/orphan-commits"
tree_git_no_duplicate_opens "$gitdir"
head=$(git --git-dir="$gitdir" rev-parse root)
"$bin" --state "$work/state" init --volume t
[ "$(git --git-dir="$gitdir" rev-parse root)" = "$head" ]
git --git-dir="$gitdir" fsck --full --strict --no-progress
printf 'PASS\treplaced-open-object\twrite fsync\n' | tee -a "$work/results.tsv"
