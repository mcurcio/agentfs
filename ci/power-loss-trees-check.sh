#!/usr/bin/env bash
# Called once per dm-log-writes replay checkpoint; recovery happens on a snapshot.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
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
trap cleanup EXIT
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
"$AGENTFS_BIN" --state "$m/state" trees init >"$m/recovery.log" 2>&1 || fail 'tree recovery'
gitdir=$m/state/history.git
git --git-dir="$gitdir" fsck --full --strict --no-progress > /dev/null 2>&1 || fail 'git fsck'
while read -r kind n; do
  [ "$kind" = W ] || continue
  want=$(printf 'acknowledged-%s\n' "$n" | sha256sum | cut -d' ' -f1)
  got=$(tree_git_file "$gitdir" main "f$n" | sha256sum | cut -d' ' -f1) || fail "missing acknowledged write $n"
  [ "$got" = "$want" ] || fail "wrong bytes for acknowledged write $n"
done < "$m/receipts"
if [ -n "${CHECK_MARK:-}" ]; then
  # Validate the external sequence through this mark, then require every
  # returned write represented by that prefix, including the current file.
  python3 - "$EXTERNAL_RECEIPTS" "$CHECK_MARK_KIND" "$CHECK_MARK_N" <<'PYMARK' || fail 'acknowledgement mark has no matching returned-call receipt'
import sys
lines = open(sys.argv[1]).read().splitlines()
kind, n = sys.argv[2], int(sys.argv[3])
assert kind in ('F', 'C') and n > 0
prefix = ['S']
for i in range(1, n + 1):
    prefix.extend([f'W {i}', f'F {i}'])
    if i < n or kind == 'C': prefix.append(f'C {i}')
assert lines[:len(prefix)] == prefix, (lines, prefix)
PYMARK
  for n in $(seq 1 "$CHECK_MARK_N"); do
    want=$(printf 'acknowledged-%s\n' "$n" | sha256sum | cut -d' ' -f1)
    got=$(tree_git_file "$gitdir" main "f$n" | sha256sum | cut -d' ' -f1) || fail "missing marked write $n"
    [ "$got" = "$want" ] || fail "wrong bytes for marked write $n"
  done
fi
tree_git_no_duplicate_opens "$gitdir" || fail "duplicate Agentfs-Open"
head=$(git --git-dir="$gitdir" rev-parse main)
"$AGENTFS_BIN" --state "$m/state" trees init >/dev/null 2>&1 || fail 'second recovery'
[ "$(git --git-dir="$gitdir" rev-parse main)" = "$head" ] || fail 'second recovery changed head'
printf 'PASS\t%s\t%s\tlast receipt %s\n' "$FS" "$point" "$(tail -n 1 "$m/receipts")" >> "$RESULTS"
