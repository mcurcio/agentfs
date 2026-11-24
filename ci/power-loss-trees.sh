#!/usr/bin/env bash
# Tree recorder power cuts. Run only on a dedicated privileged Linux CI runner:
# sudo REPLAY_LOG=/path/to/replay-log AGENTFS_BIN=/path/to/agentfs ci/power-loss-trees.sh ext4|xfs [fua|flush]
set -euo pipefail
fs=${1:?expected ext4 or xfs}; mode=${2:-fua}
case "$fs" in ext4|xfs) ;; *) exit 2 ;; esac
case "$mode" in fua|flush) ;; *) exit 2 ;; esac
# The oracle refuses planted images before it judges a real one.
"$(dirname "$0")/power-loss-trees-check.sh" self-test
: "${REPLAY_LOG:?}" "${AGENTFS_BIN:?}"
. "$(dirname "$0")/lib.sh"
base=${POWER_LOSS_DIR:-${TMPDIR:-/tmp}}
mkdir -p "$base"
work=$(mktemp -d "$base/agentfs-tree-power-loss.XXXXXX")
work=$(realpath "$work")
artifact_base=${POWER_LOSS_ARTIFACT_DIR:-${base}-artifacts}
mkdir -p "$artifact_base"
chmod 755 "$artifact_base"
artifacts=$(mktemp -d "$artifact_base/agentfs-tree-power-loss.XXXXXX")
chmod 755 "$artifacts"
printf "artifacts: %s\n" "$artifacts"
mkdir -p "$work/mnt" "$work/check" "$work/main" "$work/sandbox"
results=$artifacts/results.tsv
export RECOVERY_LOG_DIR=$artifacts
printf 'status\tfilesystem\tcheckpoint\tdetail\n' > "$results"
chmod 644 "$results"
notrun() { printf 'NOTRUN\t%s\tsetup\t%s\n' "$fs" "$*" | tee -a "$results" >&2; exit 1; }
[ "$(id -u)" = 0 ] || notrun 'requires root on a dedicated CI runner'
[ -x "$REPLAY_LOG" ] && [ -x "$AGENTFS_BIN" ] || notrun 'missing replay-log or agentfs binary'
[ -c /dev/fuse ] || notrun 'missing /dev/fuse'
# The compaction marks park the mount's compactor through AGENTFS_COMPACT_GATE,
# which only a failpoints build reads.
"$AGENTFS_BIN" --version | grep failpoints >/dev/null || notrun 'agentfs is not a failpoints build'
for cmd in dmsetup losetup blockdev mount umount fusermount3 git python3 modprobe "mkfs.$fs"; do command -v "$cmd" >/dev/null || notrun "missing $cmd"; done
for mod in dm_log_writes dm_snapshot; do modprobe "$mod" || notrun "missing kernel module $mod"; done
suffix=$$
lw=agentfs-tree-lw-$suffix; ck=agentfs-tree-ck-$suffix
devices=(); pid=
cleanup() {
  [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  for m in "$work/main" "$work/sandbox"; do fusermount3 -uz "$m" 2>/dev/null || true; done
  mountpoint -q "$work/check" && umount "$work/check" || true
  mountpoint -q "$work/mnt" && umount "$work/mnt" || true
  dmsetup remove "$ck" 2>/dev/null || true
  dmsetup remove "$lw" 2>/dev/null || true
  for dev in "${devices[@]}"; do losetup -d "$dev" 2>/dev/null || true; done
  # Only publish diagnostic files; disk images and mounted state stay private.
  for file in "$work"/*.log "$work"/*.tsv "$work"/external-receipts "$work"/ops "$work"/marks "$work"/count; do
    [ ! -f "$file" ] || install -m 0644 "$file" "$artifacts/$(basename "$file")" || true
  done
  chmod 644 "$results" 2>/dev/null || true
}
on_exit cleanup
loop() { truncate -s "$2" "$1" || notrun "cannot create backing image"; dev=$(losetup -f --show "$1") || notrun "loop device unavailable"; devices+=("$dev"); }
loop "$work/data.img" 512M; data=$dev
loop "$work/log.img" 4G; log=$dev
loop "$work/replay.img" 512M; replay=$dev
loop "$work/cow.img" 512M; cow=$dev
dmsetup create "$lw" --table "0 $(blockdev --getsz "$data") log-writes $data $log" || notrun "dm-log-writes device unavailable"
case "$fs" in ext4) mkfs.ext4 -q -F "/dev/mapper/$lw" ;; xfs) mkfs.xfs -q -f "/dev/mapper/$lw" ;; esac
dmsetup message "$lw" 0 mark mkfs
mount "/dev/mapper/$lw" "$work/mnt" || notrun "cannot mount $fs test filesystem"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
"$AGENTFS_BIN" --state "$work/mnt/state" init --volume t
# The operator's rules: an editor's temporary file is untracked (docs/trees.md,
# "Rules and the untracked area").
printf '%s\n' '*.tmp' > "$work/mnt/state/history.git/info/exclude"
"$AGENTFS_BIN" --state "$work/mnt/state" mount "$work/main" --fork-mount "$work/sandbox" --fork-key power-loss \
  --metrics-output "$work/metrics.json" >"$work/mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"; mountpoint -q "$work/sandbox"
# The driver's log, off the device, read by the oracle in
# ci/power-loss-trees-check.sh: every call that changes the volume is written
# there before it is made, every fsync once it has returned, and every receipt
# in the order of the external receipts.
cat > "$work/driver_lib.py" <<'PYLIB'
import json, os, subprocess
class Driver:
    def __init__(self, disk, external, ops, device, marks):
        self.disk, self.external, self.ops, self.device, self.marks = disk, external, ops, device, marks
    @staticmethod
    def append(name, value):
        with open(name, 'a') as f:
            f.write(value + '\n'); f.flush(); os.fsync(f.fileno())
    def log(self, **line):
        self.append(self.ops, json.dumps(line))
    def receipt(self, value):
        """A call returned: said off the device, in one order in both files."""
        self.log(t='receipt', name=value)
        self.append(self.external, value)
    def mark(self, kind, n):
        """A dm-log-writes mark once a call returned, before any receipt of it
        is written to the device."""
        self.receipt(f'{kind} {n}')
        name = f'ack-{kind.lower()}-{n}'
        subprocess.run(['dmsetup', 'message', self.device, '0', 'mark', name], check=True)
        self.append(self.marks, f'{name} {kind} {n}')
    def on_disk(self, *values):
        for value in values: self.append(self.disk, value)
    # The calls. `unit * times` is what a write holds.
    def create(self, tree, root, path):
        self.log(t='op', op='create', tree=tree, path=path)
        return os.open(f'{root}/{path}', os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    def write(self, tree, path, fd, unit, times=1):
        self.log(t='op', op='write', tree=tree, path=path, data={'hex': unit.hex(), 'times': times})
        data = unit * times
        assert os.write(fd, data) == len(data)
    def rename(self, tree, root, old, new):
        self.log(t='op', op='rename', tree=tree, **{'from': old, 'to': new})
        os.rename(f'{root}/{old}', f'{root}/{new}')
    def fsync(self, fd):
        """fsync, FSYNCDIR: every call so far survives a power cut from here."""
        os.fsync(fd)
        self.log(t='sync')
PYLIB
# A dm-log-writes mark follows each returned fsync/close before any receipt
# is written to this device. Off-device receipts associate marks with calls.
# Only after both marks do we fsync on-device receipts for the sweep oracle.
python3 - "$work" "$lw" "$(dirname "$0")/trees-figures.py" <<'PYWORK'
import importlib.util, json, os, sys, time
work, device, figures = sys.argv[1:]
sys.path.insert(0, work)
from driver_lib import Driver
root, sandbox, metrics = f'{work}/main', f'{work}/sandbox', f'{work}/metrics.json'
d = Driver(f'{work}/mnt/receipts', f'{work}/external-receipts', f'{work}/ops', device, f'{work}/marks')
d.append(d.external, 'S'); d.on_disk('S')
dirfd = os.open(os.path.dirname(d.disk), os.O_RDONLY | os.O_DIRECTORY)
os.fsync(dirfd); os.close(dirfd)
for n in range(1, 9):
    fd = d.create('root', root, f'f{n}')
    d.write('root', f'f{n}', fd, f'acknowledged-{n}\n'.encode())
    d.receipt(f'W {n}')
    d.fsync(fd); d.mark('F', n)
    os.close(fd); d.mark('C', n)
    d.on_disk(f'W {n}', f'F {n}', f'C {n}')
# Two sandbox files: a step on the fork branch moves its pointers too.
for n in range(1, 3):
    fd = d.create('fork', sandbox, f's{n}')
    d.write('fork', f's{n}', fd, f'sandbox-{n}\n'.encode())
    d.receipt(f'SW {n}')
    d.fsync(fd); d.mark('SF', n)
    os.close(fd); d.mark('SC', n)
    d.on_disk(f'SW {n}', f'SF {n}', f'SC {n}')
# A write with no fsync, marked once close(2) returned. A close does not wait
# for the disk (docs/trees.md, "Recording and durability"): a cut here may lose
# the file, and never keeps it without everything before it.
for n in range(1, 3):
    fd = d.create('root', root, f'x{n}')
    d.write('root', f'x{n}', fd, f'unsynced-{n}\n'.encode())
    os.close(fd)
    d.mark('XW', n); d.on_disk(f'XW {n}')
# A rename, marked once an fsync of the view's directory returned: FSYNCDIR
# makes every record durable, the two unsynced files' included.
fd = d.create('root', root, 'y')
d.write('root', 'y', fd, b'renamed\n')
d.fsync(fd); os.close(fd)
d.rename('root', root, 'y', 'r')
viewfd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
d.fsync(viewfd); os.close(viewfd)
d.mark('RN', 1); d.on_disk('RN 1')
# Closes and a rename with no fsync anywhere: close nb and na, rename na over
# nb, close nc, create nd. A cut keeps a prefix of these, in this order.
for name in ('nb', 'na'):
    fd = d.create('root', root, name)
    d.write('root', name, fd, f'no-fsync-{name}\n'.encode())
    os.close(fd)
d.rename('root', root, 'na', 'nb')
fd = d.create('root', root, 'nc')
d.write('root', 'nc', fd, b'no-fsync-nc\n')
os.close(fd)
os.close(d.create('root', root, 'nd'))
d.mark('NS', 1); d.on_disk('NS 1')
# The editor pattern: a temporary file in the untracked area (`*.tmp`), renamed
# into the tree. With an fsync of the temporary file first (leg 1), its bytes
# are in the area or in the tree after any cut: the area's file is unlinked
# only once the record that carries its bytes is durable. Without one (leg 2),
# only the order holds.
for leg, synced in ((1, True), (2, False)):
    data = f'edited-{leg}\n'.encode()
    unit = {'hex': data.hex(), 'times': 1}
    d.log(t='leg', leg=leg, area=f'e{leg}.tmp', path=f'e{leg}', data=unit)
    fd = os.open(f'{root}/e{leg}.tmp', os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    assert os.write(fd, data) == len(data)
    if synced:
        os.fsync(fd)
        d.log(t='area_sync', leg=leg)
    os.close(fd)
    if synced:
        d.mark('EF', leg); d.on_disk(f'EF {leg}')
    d.log(t='op', op='put', tree='root', path=f'e{leg}', data=unit)
    os.rename(f'{root}/e{leg}.tmp', f'{root}/e{leg}')
    d.mark('ER', leg); d.on_disk(f'ER {leg}')
# A file held open across a segment rollover, a compaction and a carry-forward
# (docs/trees.md, "Recording and durability"): h's first 4 MiB sit in the first
# segment; 70 MiB of another file roll the journal past it; the built event of
# that file's save compacts on history's compactor, which drops the first
# segment and carries h's frames forward. h's fsync, marked once its close
# returned, holds every byte.
spec = importlib.util.spec_from_file_location('figures', figures)
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
renames = module.Renames(metrics)
def metric():
    try:
        with open(metrics) as f: return json.load(f)
    except (OSError, ValueError): return None
# What the metrics showed while big was open, one line for each file seen:
# printed with the mount's log when a wait below is not reached.
seen = []
def sample():
    m = metric()
    if m and (not seen or seen[-1]['seq'] != m.get('seq')):
        stages = m.get('stages') or {}
        seen.append({'seq': m.get('seq'), 'built_through': m['history']['built_through'],
                     'appended': m['history']['appended'], 'queued': m.get('queued'),
                     'compactions': (stages.get('compaction') or {}).get('count'),
                     'checkpoints': stages.get('checkpoints')})
def until(test, what):
    # The view rewrites the file at each built event: wait on its renames.
    limit = time.monotonic() + 600
    while not test(metric() or {}):
        sample()
        if not renames.wait(limit):
            for line in seen: print(json.dumps(line), file=sys.stderr)
            with open(f'{work}/mount.log') as f: sys.stderr.write(f.read())
            sys.exit(f'{what}: not reached; metrics {metric()}')
def at_rest(what):
    """No built event is left to come: a metrics file written after this call
    began shows history built through every record, no pointer at a step and
    no fork sync queued (a sync is a step, and its built event another)."""
    floor = (metric() or {}).get('seq', 0) + 2
    until(lambda m: m.get('seq', 0) >= floor
          and m['history']['built_through'] >= m['history']['appended']
          and m['history']['pointers_at_steps'] == 0
          and m['queued'] == 0, what)
MIB = 1 << 20
# The compactor compacts at a built event once the journal is long enough.
# Before big's bytes go in, the fork has taken the root's last change, big's
# create (its sync is a step), every step so far is built and no sync is
# queued, so no step's built event is due before big's save. A batch with no
# step is a built event too, and one may still compact while big is open: it
# frees nothing, and big's bytes come off the compactor's baseline once its
# save is built, so that save's built event compacts all the same.
checkpoints = ((metric() or {}).get('stages') or {}).get('checkpoints', 0)
held = d.create('root', root, 'h')
for i in range(4):
    d.write('root', 'h', held, bytes([0x41 + i]), MIB)
big = d.create('root', root, 'big')
until(lambda m: os.path.exists(f'{sandbox}/big'), "the fork's sync of big's create")
at_rest("the mount at rest before big's writes")
for i in range(70):
    d.write('root', 'big', big, i.to_bytes(4, 'little'), MIB // 4)
    sample()
os.close(big)
# The view rewrites the file at each published checkpoint and at each
# compaction's end: no call of this driver starts the compaction.
until(lambda m: (m.get('stages') or {}).get('checkpoints', 0) > checkpoints
      and m.get('journal_carried_bytes', 0) > 0 and m.get('journal_dropped', 0) > 0,
      "a compaction carrying h's frames")
d.write('root', 'h', held, b'held\n')
d.fsync(held); os.close(held)
d.mark('HC', 1); d.on_disk('HC 1')
renames.close()
PYWORK
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
# Power cuts inside a compaction (docs/trees.md, "Recording and durability"), in
# a mount of its own, after the one above has unmounted so its marks keep the
# default segment size. With the compaction gate the journal has one frame per
# segment and compacts at every built event; this driver marks the log device
# while the compactor is parked at a gap, then answers `go`:
#   CP 1  the checkpoint written to checkpoint.tmp and fsynced, not renamed
#   CP 2  renamed into place, the compactor's own fsync of the directory not
#         yet run. c2's fsync leads a group commit that fsyncs that directory
#         (every record is a new segment under the gate), so the rename is
#         durable at this mark: no mark here holds an unsynced rename, and
#         nothing in this script fails when that fsync is removed. What the
#         fsync protects (the builder trims the built log only by a durable
#         checkpoint) rests on the order in compact_once and on open's own
#         directory fsync, with no power-cut test.
#   CP 3  its directory fsynced, History not yet told
#   CP 4  History told, the segments it covers still linked
#   CP 5  those segments unlinked (the next compaction has started)
#   CN 1  after that, a write appended to a new segment and not fsynced: the
#         next segment's first frame is not durable
# Before each CP mark a file c<n> is written and fsynced: the fsync returned, so
# the file survives a cut at that mark. Records after the checkpoint's point are
# not durable at these marks unless a caller's fsync made them so.
mkdir "$work/gate"
mkfifo "$work/gate/at" "$work/gate/go"
AGENTFS_COMPACT_GATE="$work/gate" "$AGENTFS_BIN" --state "$work/mnt/state" mount "$work/main" \
  >"$work/compact-mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"
python3 - "$work" "$lw" <<'PYGATE'
import os, select, sys
work, device = sys.argv[1:]
sys.path.insert(0, work)
from driver_lib import Driver
root, gate = f'{work}/main', f'{work}/gate'
d = Driver(f'{work}/mnt/receipts', f'{work}/external-receipts', f'{work}/ops', device, f'{work}/marks')
# Read and write: neither end blocks opening, nor sees end of file.
at = os.open(f'{gate}/at', os.O_RDWR)
go = os.open(f'{gate}/go', os.O_RDWR)
pending = b''
def reached():
    """The next gap the compactor parks at. The bound only ends a hang."""
    global pending
    while b'\n' not in pending:
        if not select.select([at], [], [], 600)[0]:
            sys.exit('the compactor reached no gap')
        pending += os.read(at, 4096)
    line, pending = pending.split(b'\n', 1)
    return line.decode()
def park(gap):
    while reached() != gap:
        os.write(go, b'go\n')
def kept(n):
    fd = d.create('root', root, f'c{n}')
    d.write('root', f'c{n}', fd, f'compacted-{n}\n'.encode())
    d.fsync(fd); os.close(fd)
for n, gap in enumerate(('written', 'renamed', 'dir_synced', 'before_unlink'), 1):
    kept(n)
    park(gap)
    d.mark('CP', n); d.on_disk(f'CP {n}')
    os.write(go, b'go\n')
# The compaction parked at before_unlink unlinks, and the next one starts
# only after it: its first gap is the event that the unlink is done.
kept(5)
park('covered')
d.mark('CP', 5); d.on_disk('CP 5')
fd = d.create('root', root, 'unsynced')
d.write('root', 'unsynced', fd, b'not durable\n')
d.mark('CN', 1); d.on_disk('CN 1')
os.close(fd)
# An empty line ends the gate: every gap passes from now on, so the unmount's
# join of the compactor never waits on it.
os.write(go, b'\n')
PYGATE
fusermount3 -u "$work/main"
wait "$pid"; pid=
umount "$work/mnt"
dmsetup remove "$lw"
export REPLAY_DEV=$replay COW_DEV=$cow CHECK_MNT=$work/check CHECK_DM=$ck CHECK_COUNT=$work/count
export FS=$fs AGENTFS_BIN RESULTS=$results EXTERNAL_RECEIPTS=$work/external-receipts OPS=$work/ops
export FORK
FORK=$(sed -n 's/^fork tree: //p' "$work/mount.log")
[ -n "$FORK" ] || notrun 'mount log names no sandbox tree'
: > "$CHECK_COUNT"
# Replay each returned-call mark from an empty device. Reusing the final image
# would leave later sectors behind and could make a broken checkpoint pass.
python3 - "$work/marks" <<'PYMARKS' || { printf 'FAIL\t%s\tmarks\tmissing or invalid acknowledgement marks\n' "$fs" >> "$results"; exit 1; }
import sys
actual = open(sys.argv[1]).read().splitlines()
expected = [f'ack-{kind.lower()}-{n} {kind} {n}' for n in range(1, 9) for kind in ('F', 'C')]
expected += [f'ack-{kind.lower()}-{n} {kind} {n}' for n in range(1, 3) for kind in ('SF', 'SC')]
expected += [f'ack-xw-{n} XW {n}' for n in range(1, 3)] + ['ack-rn-1 RN 1', 'ack-ns-1 NS 1']
expected += ['ack-ef-1 EF 1', 'ack-er-1 ER 1', 'ack-er-2 ER 2', 'ack-hc-1 HC 1']
expected += [f'ack-cp-{n} CP {n}' for n in range(1, 6)] + ['ack-cn-1 CN 1']
assert actual == expected, (actual, expected)
PYMARKS
# Locate each mark and scan the pinned replay tool's flagged entries once.
while read -r mark kind n; do
  entry=$("$REPLAY_LOG" --log "$log" --find --end-mark "$mark")
  [[ "$entry" =~ ^[0-9]+@[0-9]+$ ]] || { printf 'FAIL\t%s\t%s\tinvalid replay entry\n' "$fs" "$mark" >> "$results"; exit 1; }
  printf '%s\t%s\t%s\t%s\n' "$mark" "$kind" "$n" "$entry" >> "$work/mark-entries.tsv"
done < "$work/marks"
"$REPLAY_LOG" --log "$log" --find -vv --end-mark ack-cn-1 > "$work/entry-flags.log"
python3 - "$work/mark-entries.tsv" "$work/entry-flags.log" "$work/durable-entries.tsv" <<'PYDURABLE' || { printf 'FAIL\t%s\tmarks\tdurable entry mapping failed\n' "$fs" >> "$results"; exit 1; }
import bisect, re, sys
marks, flags_file, result = sys.argv[1:]
pattern = re.compile(r'^seek entry (\d+)@(\d+): .* flags 0x[0-9a-f]+\(([^)]*)\)$')
durable = []
with open(flags_file) as f:
    for line in f:
        m = pattern.match(line.strip())
        if m and set(m[3].split('|')) & {'FLUSH', 'FUA'}:
            durable.append((int(m[1]), m[2], m[3]))
assert durable and all(a[0] < b[0] for a, b in zip(durable, durable[1:]))
indices = [x[0] for x in durable]
with open(marks) as source, open(result, 'w') as out:
    for line in source:
        mark, kind, n, position = line.split()
        mark_entry = int(position.split('@')[0])
        i = bisect.bisect_left(indices, mark_entry) - 1
        assert i >= 0, f'no flush or FUA before {mark}'
        entry, sector, flags = durable[i]
        print(mark, kind, n, entry, sector, flags, sep='\t', file=out)
PYDURABLE
while read -r mark kind n entry sector flags; do
  # Replay up to the last observed flush/FUA before the returned-call mark.
  # Prefix replay models this logged order, not every volatile-cache subset.
  dd if=/dev/zero of="$replay" bs=4M count=128 status=none
  "$REPLAY_LOG" --log "$log" --replay "$replay" --limit "$((entry + 1))" >"$work/replay-$mark-durable.log" 2>&1 || { printf 'FAIL\t%s\t%s-durable\treplay failed\n' "$fs" "$mark" >> "$results"; exit 1; }
  CHECK_MARK="$mark-durable-$entry" CHECK_MARK_KIND="$kind" CHECK_MARK_N="$n" ci/power-loss-trees-check.sh
  # Keep the full submitted-write prefix through the mark as a diagnostic.
  dd if=/dev/zero of="$replay" bs=4M count=128 status=none
  "$REPLAY_LOG" --log "$log" --replay "$replay" --end-mark "$mark" >"$work/replay-$mark.log" 2>&1 || { printf 'FAIL\t%s\t%s\treplay failed\n' "$fs" "$mark" >> "$results"; exit 1; }
  CHECK_MARK="$mark" CHECK_MARK_KIND="$kind" CHECK_MARK_N="$n" ci/power-loss-trees-check.sh
done < "$work/durable-entries.tsv"
dd if=/dev/zero of="$replay" bs=4M count=128 status=none
"$REPLAY_LOG" --log "$log" --replay "$replay" --end-mark mkfs
if ! "$REPLAY_LOG" --log "$log" --replay "$replay" --start-mark mkfs --check "$mode" --fsck "$PWD/ci/power-loss-trees-check.sh" >"$work/replay.log" 2>&1; then
  printf 'FAIL\t%s\treplay\tsee replay.log\n' "$fs" >> "$results"
  cat "$work/replay.log" >&2; exit 1
fi
[ -s "$CHECK_COUNT" ] || { printf 'FAIL\t%s\treplay\tno checkpoints\n' "$fs" >> "$results"; exit 1; }
CHECK_FINAL=1 ci/power-loss-trees-check.sh
printf 'PASS\t%s\tcomplete\t%s checkpoints; mode=%s\n' "$fs" "$(wc -l < "$CHECK_COUNT")" "$mode" | tee -a "$results"
