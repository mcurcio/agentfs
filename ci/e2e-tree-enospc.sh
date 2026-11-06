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
sudo mount -t tmpfs -o size=32m,mode=0777 tmpfs "$work/volume"
"$bin" --state "$work/volume/state" trees init
"$bin" --state "$work/volume/state" trees mount "$work/main" --sandbox "$work/sandbox" >"$work/mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"; mountpoint -q "$work/sandbox"
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
rm "$work/volume/filler"
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
"$bin" --state "$work/volume/state" trees init
gitdir=$work/volume/state/history.git
[ "$(tree_git_file "$gitdir" main file)" = acknowledged ]
git --git-dir="$gitdir" fsck --full --strict --no-progress
tree_git_no_duplicate_opens "$gitdir"
head=$(git --git-dir="$gitdir" rev-parse main)
"$bin" --state "$work/volume/state" trees init
[ "$(git --git-dir="$gitdir" rev-parse main)" = "$head" ]
printf 'PASS\tenospc\twrite fsync\n' | tee "$work/results.tsv"
# Fill the same real filesystem after a rename's WAL is durable but before
# its Git publication. Rename is synchronous, so its errno reaches the caller.
rm -rf "$work/volume/state"
"$bin" --state "$work/volume/state" trees init
fork=$("$bin" --state "$work/volume/state" trees fork)
"$bin" --state "$work/volume/state" trees mount "$work/main" --sandbox "$work/sandbox" --fork "$fork" >"$work/setup-mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"; mountpoint -q "$work/sandbox"
python3 - "$work/main/source" "$work/git-receipt" <<'PY'
import os, sys
fd = os.open(sys.argv[1], os.O_CREAT | os.O_WRONLY, 0o600)
assert os.write(fd, b'git-acknowledged') == 16
os.fsync(fd)
os.close(fd)
with open(sys.argv[2], 'w') as f:
    f.write('write fsync close\n'); f.flush(); os.fsync(f.fileno())
PY
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
AGENTFS_FAILPOINTS='tree.after_wal_before_apply=sleep(30000)' "$bin" --state "$work/volume/state" trees mount "$work/main" --sandbox "$work/sandbox" --fork "$fork" >"$work/git-mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"; mountpoint -q "$work/sandbox"
python3 - "$work/main/source" "$work/main/destination" <<'PY' >"$work/rename.log" 2>&1 & client=$!
import errno, os, sys
try:
    os.rename(sys.argv[1], sys.argv[2])
except OSError as e:
    assert e.errno == errno.ENOSPC, e
else:
    raise AssertionError('rename succeeded while Git state filesystem was full')
PY
for _ in $(seq 1 200); do
  if grep -Fq 'agentfs failpoint tree.after_wal_before_apply triggered (sleep(30000))' "$work/git-mount.log"; then break; fi
  kill -0 "$pid"; kill -0 "$client"; sleep .1
done
grep -Fq 'agentfs failpoint tree.after_wal_before_apply triggered (sleep(30000))' "$work/git-mount.log"
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
# Filling must finish before the stalled rename is released.
kill -0 "$client" || { echo 'rename returned before filesystem was filled' >&2; exit 1; }
if ! wait "$client"; then cat "$work/rename.log" >&2; exit 1; fi
rm "$work/volume/filler"
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
"$bin" --state "$work/volume/state" trees init
gitdir=$work/volume/state/history.git
# Recovery may finish the WAL-recorded rename despite the failed reply.
if git --git-dir="$gitdir" cat-file -e main:files/destination 2>/dev/null; then
  path=destination
else
  path=source
fi
[ "$(tree_git_file "$gitdir" main "$path")" = git-acknowledged ]
git --git-dir="$gitdir" fsck --full --strict --no-progress
tree_git_no_duplicate_opens "$gitdir"
head=$(git --git-dir="$gitdir" rev-parse main)
"$bin" --state "$work/volume/state" trees init
[ "$(git --git-dir="$gitdir" rev-parse main)" = "$head" ]
printf 'PASS\tenospc-git-publication\twrite fsync close; rename ENOSPC\n' | tee -a "$work/results.tsv"
