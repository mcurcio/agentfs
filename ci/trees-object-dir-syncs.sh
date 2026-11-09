#!/usr/bin/env bash
# The fan-out directory and `objects/` are fsynced before a step's journal record
# names the object (phase-1.md M1.16). A prefix replay on ext4 or XFS cannot judge
# this ordering: the record's own fsync carries the rename's directory entry too. So
# this reads the system calls of one save on a real mount, under strace. As root, for
# strace to follow fusermount3.
#   sudo env AGENTFS_BIN=/path/to/agentfs ci/trees-object-dir-syncs.sh
set -euo pipefail
: "${AGENTFS_BIN:?}"
work=$(mktemp -d "${TMPDIR:-/tmp}/agentfs-object-dir-syncs.XXXXXX")
work=$(realpath "$work")
mkdir -p "$work/main" "$work/sandbox"
pid=
cleanup() {
  [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  for m in "$work/main" "$work/sandbox"; do fusermount3 -uz "$m" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT
"$AGENTFS_BIN" --state "$work/state" trees init
strace -f -y -qq -e trace=fsync,pwrite64 -o "$work/trace" \
  "$AGENTFS_BIN" --state "$work/state" trees mount "$work/main" --sandbox "$work/sandbox" >"$work/mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"
python3 -c "
import os, sys
fd = os.open(sys.argv[1], os.O_CREAT | os.O_WRONLY, 0o600)
assert os.write(fd, b'object-dir-syncs\n') == 17
os.close(fd)
" "$work/main/f"
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
python3 - "$work/trace" "$work/state" <<'PY'
import re, sys
trace, state = sys.argv[1:]
objects = re.escape(f'{state}/history.git/objects')
fanout = re.compile(rf'^\d+ +fsync\(\d+<{objects}/[0-9a-f]{{2}}>')
parent = re.compile(rf'^\d+ +fsync\(\d+<{objects}>')
journal = re.compile(rf'^\d+ +pwrite64\(\d+<{re.escape(state)}/calls\.wal>')
lines = open(trace).read().splitlines()
kinds = []
for line in lines:
    if fanout.match(line):
        kinds.append('fanout')
    elif parent.match(line):
        kinds.append('parent')
    elif journal.match(line):
        kinds.append('journal')
assert kinds, 'no matching calls: is strace tracing the mount?'
# Some commit's object directory syncs, in order, with no journal write between
# them, and the journal's record of that step after them.
sequence = ''.join({'fanout': 'f', 'parent': 'p', 'journal': 'j'}[k] for k in kinds)
assert re.search('f+p+j', sequence), f'no fan-out then objects fsync before a journal write: {sequence}'
print(f'ok: {sequence.count("f")} fan-out, {sequence.count("p")} objects fsyncs, ordered before their journal record')
PY
