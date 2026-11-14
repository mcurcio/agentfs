#!/usr/bin/env bash
# The builder fsyncs a batch's pack files and `objects/pack` before the batch's
# built-log entry is written and fsynced, and only that entry makes the batch's
# objects reachable (`docs/trees.md`, "History trails the files"). A prefix replay on
# ext4 or XFS cannot judge this ordering: a later fsync carries the pack's directory
# entry too. So this reads the system calls of one save on a real mount, under
# strace. As root, for strace to follow fusermount3.
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
"$AGENTFS_BIN" --state "$work/state" trees init --volume t
strace -f -y -qq -e trace=fsync,fdatasync,pwrite64 -o "$work/trace" \
  "$AGENTFS_BIN" --state "$work/state" trees mount "$work/main" --fork-mount "$work/sandbox" >"$work/mount.log" 2>&1 & pid=$!
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
packs = re.escape(f'{state}/history.git/objects/pack')
pack_file = re.compile(rf'^\d+ +fsync\(\d+<{packs}/pack-[0-9a-f]+\.(pack|idx|rev)>')
pack_dir = re.compile(rf'^\d+ +fsync\(\d+<{packs}>')
log_write = re.compile(rf'^\d+ +pwrite64\(\d+<{re.escape(state)}/built\.log>')
log_sync = re.compile(rf'^\d+ +fdatasync\(\d+<{re.escape(state)}/built\.log>')
sequence = ''
for line in open(trace).read().splitlines():
    for letter, pattern in (('k', pack_file), ('d', pack_dir), ('w', log_write), ('s', log_sync)):
        if pattern.match(line):
            sequence += letter
assert sequence, 'no matching calls: is strace tracing the mount?'
# Some batch's pack files then their directory fsynced, with nothing of the built
# log between them, then its entry written and fsynced.
assert re.search('k+d+ws', sequence), f'no pack fsyncs then a built-log entry: {sequence}'
print(f'ok: {sequence.count("k")} pack files and {sequence.count("d")} pack directory fsyncs, '
      f'each batch before its built-log entry ({sequence.count("s")})')
PY
