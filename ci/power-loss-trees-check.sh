#!/usr/bin/env bash
# Called once per dm-log-writes replay checkpoint; recovery happens on a snapshot.
#
# What a power cut may leave (docs/trees.md, "Recording and durability"), checked
# on each replayed image:
#   1. the volume opens;
#   2. recovery saves what survived, and a second recovery changes nothing;
#   3. what survived is a prefix of the calls the driver made: the recovered
#      files are those of an uninterrupted run cut after some call (a write may
#      be cut partway), never a later call without an earlier one;
#   4. that prefix holds every call made before the last fsync, FSYNCDIR or
#      `O_SYNC` write that had returned. A close proves nothing: it does not
#      wait for the disk.
# And for a file moved from the untracked area into the tree: bytes a program
# fsynced in the area are in the area or in the tree.
#
# `ci/power-loss-trees-check.sh self-test` runs the oracle on planted images,
# with no device: it must refuse a later call without an earlier one, a lost
# call an fsync covered, and fsynced bytes in neither place.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
# The oracle. `check OPS RETURNED GITDIR FORK AREAS`: OPS is the driver's log,
# off the device, one JSON line per call it was about to make (`op`), per fsync
# that returned (`sync`: every op before it is durable; `area_sync`: an area
# file's bytes are), and per receipt (`receipt`, in the order of the external
# receipts). RETURNED holds the receipts known to have been written before the
# image's cut. The recovered files are read from Git and the untracked areas.
oracle() {
  PYTHONPATH="$(dirname "$0")${PYTHONPATH:+:$PYTHONPATH}" python3 - "$@" <<'PYORACLE'
import json, os, sys

def data_of(spec):
    return bytes.fromhex(spec['hex']) * spec['times']

class Run:
    """The driver's calls, replayed: the files after each prefix of them."""
    def __init__(self, lines):
        self.ops, self.lines = [], lines
        self.content = []            # per file identity: every byte it was given
        self.states = [{}]           # after k ops: (tree, path) -> (identity, length)
        self.legs = {}               # editor leg -> (area path, tree path, bytes)
        state = {}
        for line in lines:
            if line['t'] == 'leg':
                self.legs[line['leg']] = (line['area'], line['path'], data_of(line['data']))
            if line['t'] != 'op':
                continue
            state = dict(state)
            key = (line['tree'], line.get('path'))
            if line['op'] == 'create':
                assert key not in state, line
                self.content.append(bytearray())
                state[key] = (len(self.content) - 1, 0)
            elif line['op'] == 'write':
                identity, length = state[key]
                data = data_of(line['data'])
                assert len(self.content[identity]) == length, line
                self.content[identity] += data
                state[key] = (identity, length + len(data))
            elif line['op'] == 'put':
                self.content.append(bytearray(data_of(line['data'])))
                state[key] = (len(self.content) - 1, len(self.content[-1]))
            elif line['op'] == 'rename':
                state[(line['tree'], line['to'])] = state.pop((line['tree'], line['from']))
            else:
                raise AssertionError(line)
            self.ops.append(line)
            self.states.append(state)

    def paths(self):
        return sorted({key for state in self.states for key in state})

    def bounds(self, returned):
        """(ops an fsync that returned covers, editor legs whose area bytes one
        covers), from the receipts known returned."""
        receipts = [(i, line['name']) for i, line in enumerate(self.lines) if line['t'] == 'receipt']
        assert [name for _, name in receipts[:len(returned)]] == returned, (returned, receipts)
        end = receipts[len(returned) - 1][0] if returned else 0
        required, seen, legs = 0, 0, []
        for line in self.lines[:end]:
            if line['t'] == 'op': seen += 1
            elif line['t'] == 'sync': required = seen
            elif line['t'] == 'area_sync': legs.append(line['leg'])
        return required, legs

    def cut_after(self, k, found):
        """Whether `found` is the run cut after op k, op k+1 perhaps partway."""
        state = self.states[k]
        following = self.ops[k] if k < len(self.ops) else None
        for key in self.paths():
            got, want = found.get(key), state.get(key)
            if want is None:
                if got is not None: return False
                continue
            if got is None: return False
            identity, length = want
            most = length
            if following and following['op'] == 'write' and (following['tree'], following['path']) == key:
                most += len(data_of(following['data']))
            if not length <= len(got) <= most: return False
            if got != self.content[identity][:len(got)]: return False
        return True

    def judge(self, returned, found, areas):
        """None, or what is wrong with the recovered files `found` ((tree,
        path) -> bytes) and area files `areas` (area path -> bytes)."""
        required, legs = self.bounds(returned)
        cuts = [k for k in range(len(self.ops) + 1) if self.cut_after(k, found)]
        if not cuts:
            return 'the recovered files are no prefix of the calls made'
        if max(cuts) < required:
            return (f'the recovered files end after call {max(cuts)} of {len(self.ops)}; '
                    f'a returned fsync covered the first {required}')
        for leg in legs:
            area, path, data = self.legs[leg]
            if areas.get(area) != data and found.get(('root', path)) != data:
                return f'editor leg {leg}: its fsynced bytes are in neither the area nor the tree'
        return None

def self_test():
    def spec(text, times=1): return {'hex': text.encode().hex(), 'times': times}
    lines = [
        {'t': 'op', 'op': 'create', 'tree': 'root', 'path': 'a'},
        {'t': 'op', 'op': 'write', 'tree': 'root', 'path': 'a', 'data': spec('one\n')},
        {'t': 'sync'},
        {'t': 'receipt', 'name': 'F 1'},
        {'t': 'receipt', 'name': 'C 1'},
        {'t': 'op', 'op': 'create', 'tree': 'root', 'path': 'b'},
        {'t': 'op', 'op': 'write', 'tree': 'root', 'path': 'b', 'data': spec('two\n')},
        {'t': 'receipt', 'name': 'XW 1'},
        {'t': 'op', 'op': 'rename', 'tree': 'root', 'from': 'b', 'to': 'a'},
        {'t': 'op', 'op': 'create', 'tree': 'fork', 'path': 'c'},
        {'t': 'op', 'op': 'write', 'tree': 'fork', 'path': 'c', 'data': spec('x', 8)},
        {'t': 'leg', 'leg': 1, 'area': 'e.tmp', 'path': 'e', 'data': spec('edited\n')},
        {'t': 'area_sync', 'leg': 1},
        {'t': 'receipt', 'name': 'EF 1'},
        {'t': 'op', 'op': 'put', 'tree': 'root', 'path': 'e', 'data': spec('edited\n')},
        {'t': 'receipt', 'name': 'ER 1'},
    ]
    run = Run(lines)
    a1, a2 = {('root', 'a'): b'one\n'}, {('root', 'a'): b'two\n'}
    both = {('root', 'a'): b'one\n', ('root', 'b'): b'two\n'}
    early, late = ['F 1', 'C 1', 'XW 1'], ['F 1', 'C 1', 'XW 1', 'EF 1', 'ER 1']
    accepted = [
        ('nothing returned, nothing kept', [], {}, {}),
        ('a create without its write', [], {('root', 'a'): b''}, {}),
        ('a close that returned and was lost', early, a1, {}),
        ('an unsynced file kept', early, both, {}),
        ('a write cut partway', early, {**a2, ('fork', 'c'): b'xxx'}, {}),
        ('fsynced bytes still in the area', late, a1, {'e.tmp': b'edited\n'}),
        ('fsynced bytes moved to the tree', late,
         {**a2, ('fork', 'c'): b'x' * 8, ('root', 'e'): b'edited\n'}, {}),
    ]
    for what, returned, found, areas in accepted:
        wrong = run.judge(returned, found, areas)
        assert wrong is None, f'{what}: refused: {wrong}'
    refused = [
        # Planted: the fork's create kept with the root's rename lost, a later
        # call without an earlier one.
        ('a later call without an earlier one', ['F 1'], {**a1, ('fork', 'c'): b''}, {}, 'no prefix'),
        ('a write the fsync covered lost', ['F 1'], {('root', 'a'): b''}, {}, 'a returned fsync covered'),
        ('wrong bytes', ['F 1'], {('root', 'a'): b'onE\n'}, {}, 'no prefix'),
        ('everything lost after an fsync', ['F 1', 'C 1'], {}, {}, 'a returned fsync covered'),
        ('fsynced area bytes in neither place', early + ['EF 1'], a1, {}, 'neither'),
        ('fsynced area bytes cut short', early + ['EF 1'], a1, {'e.tmp': b'edi'}, 'neither'),
    ]
    for what, returned, found, areas, why in refused:
        wrong = run.judge(returned, found, areas)
        assert wrong is not None and why in wrong, f'{what}: not refused ({wrong})'
    try:
        run.bounds(['C 1'])
    except AssertionError:
        pass
    else:
        raise AssertionError('receipts out of order were accepted')
    print(f'power-loss oracle self-test: {len(accepted)} images accepted, {len(refused)} planted images refused')

def check(ops, returned, gitdir, fork, areas_dir):
    import subprocess
    import tree_git_read
    with open(ops) as f:
        run = Run([json.loads(line) for line in f if line.strip()])
    with open(returned) as f:
        returned = [line for line in f.read().splitlines() if line != 'S']
    refs = {'root': 'root', 'fork': f'refs/heads/{fork}'}
    found = {}
    for tree, path in run.paths():
        try:
            found[(tree, path)] = tree_git_read.read_once(gitdir, refs[tree], path)
        except subprocess.CalledProcessError:
            pass
    areas = {}
    for area, _, _ in run.legs.values():
        try:
            with open(os.path.join(areas_dir, 'root', area), 'rb') as f:
                areas[area] = f.read()
        except OSError:
            pass
    wrong = run.judge(returned, found, areas)
    if wrong:
        kept = {f'{tree}:{path}': len(data) for (tree, path), data in sorted(found.items())}
        sys.exit(f'{wrong}; returned receipts {returned[-3:]}; recovered file lengths {kept}; area files {sorted(areas)}')

if sys.argv[1] == 'self-test':
    self_test()
else:
    check(*sys.argv[2:])
PYORACLE
}
if [ "${1:-}" = self-test ]; then
  oracle self-test
  exit
fi
if [ -n "${CHECK_MARK:-}" ]; then
  point=$CHECK_MARK
elif [ "${CHECK_FINAL:-0}" = 1 ]; then
  point=final
else
  point=$(( $(wc -l < "$CHECK_COUNT") + 1 ))
  printf '%s\n' "$point" >> "$CHECK_COUNT"
fi
fail() { printf 'FAIL\t%s\t%s\t%s\n' "$FS" "$point" "$*" >> "$RESULTS"; echo "tree replay checkpoint $point: $*" >&2; exit 1; }
cleanup() { mountpoint -q "$CHECK_MNT" && umount "$CHECK_MNT" || true; dmsetup remove "$CHECK_DM" 2>/dev/null || true; }
on_exit cleanup
dmsetup create "$CHECK_DM" --table "0 $(blockdev --getsz "$REPLAY_DEV") snapshot $REPLAY_DEV $COW_DEV N 8" || fail 'snapshot creation'
mount "/dev/mapper/$CHECK_DM" "$CHECK_MNT" || fail 'filesystem mount'
m=$CHECK_MNT
if ! grep -qx S "$m/receipts" 2>/dev/null; then
  [ "${CHECK_FINAL:-0}" != 1 ] && [ -z "${CHECK_MARK:-}" ] || fail "required receipt missing"
  printf 'PASS\t%s\t%s\tbefore first durable receipt\n' "$FS" "$point" >> "$RESULTS"; exit 0
fi
# Every on-disk receipt must correspond to a returned syscall, independently
# observed off-device. Validate ordering as well as membership.
python3 - "$m/receipts" "$EXTERNAL_RECEIPTS" <<'PY' || fail 'receipts disagree'
import os, sys
on = open(sys.argv[1]).read().splitlines()
off = open(sys.argv[2]).read().splitlines()
assert on == off[:len(on)], (on, off)
if os.environ.get('CHECK_FINAL') == '1': assert on == off, (on, off)
PY
if ! "$AGENTFS_BIN" --state "$m/state" init --volume t >"$m/recovery.log" 2>&1; then
  install -m 0644 "$m/recovery.log" "$RECOVERY_LOG_DIR/recovery-$point.log" || true
  cat "$m/recovery.log" >&2 || true
  fail 'tree recovery'
fi
gitdir=$m/state/history.git
git --git-dir="$gitdir" fsck --full --strict --no-progress > /dev/null 2>&1 || fail 'git fsck'
# The receipts known returned before this image's cut: through the mark's own
# at a mark (the driver writes it off the device before it marks), else those
# the image itself holds.
if [ -n "${CHECK_MARK:-}" ]; then
  python3 - "$EXTERNAL_RECEIPTS" "$CHECK_MARK_KIND $CHECK_MARK_N" > "$m/returned" <<'PYMARK' || fail 'acknowledgement mark has no matching returned-call receipt'
import sys
lines = open(sys.argv[1]).read().splitlines()
print('\n'.join(lines[:lines.index(sys.argv[2]) + 1]))
PYMARK
else
  cp "$m/receipts" "$m/returned"
fi
if ! wrong=$(oracle check "$OPS" "$m/returned" "$gitdir" "$FORK" "$m/state/untracked" 2>&1); then
  fail "$(printf %s "$wrong" | tail -n 1)"
fi
tree_git_no_duplicate_opens "$gitdir" || fail "duplicate Agentfs-Open"
heads=$(git --git-dir="$gitdir" for-each-ref --format='%(refname) %(objectname)' refs/heads)
"$AGENTFS_BIN" --state "$m/state" init --volume t >/dev/null 2>&1 || fail 'second recovery'
[ "$(git --git-dir="$gitdir" for-each-ref --format='%(refname) %(objectname)' refs/heads)" = "$heads" ] || fail 'second recovery changed heads'
# The refs are a copy: without them, the journal alone brings back the same heads.
rm -rf "$gitdir/refs" "$gitdir/packed-refs"
"$AGENTFS_BIN" --state "$m/state" init --volume t >/dev/null 2>&1 || fail 'recovery without refs'
[ "$(git --git-dir="$gitdir" for-each-ref --format='%(refname) %(objectname)' refs/heads)" = "$heads" ] || fail 'refs differ after removing them'
printf 'PASS\t%s\t%s\tlast receipt %s\n' "$FS" "$point" "$(tail -n 1 "$m/receipts")" >> "$RESULTS"
