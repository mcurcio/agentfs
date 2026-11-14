#!/usr/bin/env bash
# Tree recorder power cuts. Run only on a dedicated privileged Linux CI runner:
# sudo REPLAY_LOG=/path/to/replay-log AGENTFS_BIN=/path/to/agentfs ci/power-loss-trees.sh ext4|xfs [fua|flush]
set -euo pipefail
fs=${1:?expected ext4 or xfs}; mode=${2:-fua}
case "$fs" in ext4|xfs) ;; *) exit 2 ;; esac
case "$mode" in fua|flush) ;; *) exit 2 ;; esac
: "${REPLAY_LOG:?}" "${AGENTFS_BIN:?}"
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
  for file in "$work"/*.log "$work"/*.tsv "$work"/external-receipts "$work"/marks "$work"/count; do
    [ ! -f "$file" ] || install -m 0644 "$file" "$artifacts/$(basename "$file")" || true
  done
  chmod 644 "$results" 2>/dev/null || true
}
trap cleanup EXIT
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
"$AGENTFS_BIN" --state "$work/mnt/state" trees init --volume t
"$AGENTFS_BIN" --state "$work/mnt/state" trees mount "$work/main" --fork-mount "$work/sandbox" --fork-key power-loss \
  --metrics-output "$work/metrics.json" >"$work/mount.log" 2>&1 & pid=$!
for _ in $(seq 1 200); do
  if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
  kill -0 "$pid"; sleep .1
done
mountpoint -q "$work/main"; mountpoint -q "$work/sandbox"
# A dm-log-writes mark follows each returned fsync/close before any receipt
# is written to this device. Off-device receipts associate marks with calls.
# Only after both marks do we fsync on-device receipts for the sweep oracle.
python3 - "$work/main" "$work/sandbox" "$work/mnt/receipts" "$work/external-receipts" "$lw" "$work/marks" "$work/metrics.json" "$(dirname "$0")/trees-figures.py" <<'PYWORK'
import importlib.util, json, os, subprocess, sys, time
root, sandbox, disk, external, device, marks, metrics, figures = sys.argv[1:]
def append(name, value):
    with open(name, 'a') as f:
        f.write(value + '\n'); f.flush(); os.fsync(f.fileno())
def mark(kind, n):
    value = f'{kind} {n}'
    append(external, value)
    name = f'ack-{kind.lower()}-{n}'
    subprocess.run(['dmsetup', 'message', device, '0', 'mark', name], check=True)
    append(marks, f'{name} {kind} {n}')
append(external, 'S'); append(disk, 'S')
dirfd = os.open(os.path.dirname(disk), os.O_RDONLY | os.O_DIRECTORY)
os.fsync(dirfd); os.close(dirfd)
for n in range(1, 9):
    fd = os.open(f'{root}/f{n}', os.O_CREAT | os.O_WRONLY, 0o600)
    data = f'acknowledged-{n}\n'.encode()
    assert os.write(fd, data) == len(data)
    append(external, f'W {n}')
    os.fsync(fd); mark('F', n)
    os.close(fd); mark('C', n)
    for kind in ('W', 'F', 'C'):
        append(disk, f'{kind} {n}')
# Two sandbox files: a step on the fork branch moves its pointers too.
for n in range(1, 3):
    fd = os.open(f'{sandbox}/s{n}', os.O_CREAT | os.O_WRONLY, 0o600)
    data = f'sandbox-{n}\n'.encode()
    assert os.write(fd, data) == len(data)
    append(external, f'SW {n}')
    os.fsync(fd); mark('SF', n)
    os.close(fd); mark('SC', n)
    for kind in ('SW', 'SF', 'SC'):
        append(disk, f'{kind} {n}')
# A write with no fsync, marked once close(2) returned: the close's FLUSH makes
# the open's calls durable (git-model §3). A rename, marked once an fsync of
# the view's directory returned: FSYNCDIR makes every record durable. The last
# acknowledged rename of the run.
for n in range(1, 3):
    fd = os.open(f'{root}/x{n}', os.O_CREAT | os.O_WRONLY, 0o600)
    data = f'unsynced-{n}\n'.encode()
    assert os.write(fd, data) == len(data)
    os.close(fd)
    mark('XW', n); append(disk, f'XW {n}')
fd = os.open(f'{root}/y', os.O_CREAT | os.O_WRONLY, 0o600)
assert os.write(fd, b'renamed\n') == 8
os.fsync(fd); os.close(fd)
os.rename(f'{root}/y', f'{root}/r')
viewfd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
os.fsync(viewfd); os.close(viewfd)
mark('RN', 1); append(disk, 'RN 1')
# A file held open across a segment rollover, a compaction and a carry-forward
# (docs/trees.md, "Recording and durability"): h's first 4 MiB sit in the first
# segment; 70 MiB of another file roll the journal past it; once history is
# built, the next call compacts, drops the old segments and carries h's frames
# forward. h's close, marked once it returned, holds every byte.
spec = importlib.util.spec_from_file_location('figures', figures)
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
renames = module.Renames(metrics)
def metric():
    try:
        with open(metrics) as f: return json.load(f)
    except (OSError, ValueError): return None
def until(test, what):
    # The view rewrites the file at each built event: wait on its renames.
    limit = time.monotonic() + 600
    while not test(metric() or {}):
        if not renames.wait(limit): sys.exit(f'{what}: not reached; metrics {metric()}')
MIB = 1 << 20
held = os.open(f'{root}/h', os.O_CREAT | os.O_WRONLY, 0o600)
for i in range(4):
    assert os.write(held, bytes([0x41 + i]) * MIB) == MIB
big = os.open(f'{root}/big', os.O_CREAT | os.O_WRONLY, 0o600)
for i in range(70):
    assert os.write(big, i.to_bytes(4, 'little') * (MIB // 4)) == MIB
os.close(big)
floor = (metric() or {}).get('seq', 0) + 2
until(lambda m: m.get('seq', 0) >= floor, 'a metrics file after the big save')
target = metric()['history']['appended']
until(lambda m: m['history']['built_through'] >= target, 'history built through the big save')
poke = os.open(f'{root}/poke', os.O_CREAT | os.O_WRONLY, 0o600); os.close(poke)
until(lambda m: m.get('journal_carried_bytes', 0) > 0 and m.get('journal_dropped', 0) > 0,
      "a compaction carrying h's frames")
assert os.write(held, b'held\n') == 5
os.fsync(held); os.close(held)
mark('HC', 1); append(disk, 'HC 1')
renames.close()
PYWORK
fusermount3 -u "$work/main"; fusermount3 -u "$work/sandbox"
wait "$pid"; pid=
umount "$work/mnt"
dmsetup remove "$lw"
export REPLAY_DEV=$replay COW_DEV=$cow CHECK_MNT=$work/check CHECK_DM=$ck CHECK_COUNT=$work/count
export FS=$fs AGENTFS_BIN RESULTS=$results EXTERNAL_RECEIPTS=$work/external-receipts
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
expected += [f'ack-xw-{n} XW {n}' for n in range(1, 3)] + ['ack-rn-1 RN 1', 'ack-hc-1 HC 1']
assert actual == expected, (actual, expected)
PYMARKS
# Locate each mark and scan the pinned replay tool's flagged entries once.
while read -r mark kind n; do
  entry=$("$REPLAY_LOG" --log "$log" --find --end-mark "$mark")
  [[ "$entry" =~ ^[0-9]+@[0-9]+$ ]] || { printf 'FAIL\t%s\t%s\tinvalid replay entry\n' "$fs" "$mark" >> "$results"; exit 1; }
  printf '%s\t%s\t%s\t%s\n' "$mark" "$kind" "$n" "$entry" >> "$work/mark-entries.tsv"
done < "$work/marks"
"$REPLAY_LOG" --log "$log" --find -vv --end-mark ack-hc-1 > "$work/entry-flags.log"
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
