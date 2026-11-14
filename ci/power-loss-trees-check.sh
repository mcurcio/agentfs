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
if ! "$AGENTFS_BIN" --state "$m/state" trees init --volume t >"$m/recovery.log" 2>&1; then
  install -m 0644 "$m/recovery.log" "$RECOVERY_LOG_DIR/recovery-$point.log" || true
  cat "$m/recovery.log" >&2 || true
  fail 'tree recovery'
fi
gitdir=$m/state/history.git
git --git-dir="$gitdir" fsck --full --strict --no-progress > /dev/null 2>&1 || fail 'git fsck'
# A returned write's bytes: f<n> on the root, s<n> on the fork's branch. The sandbox
# files ride through the fork's own pointers, in the same journal.
check_write() {
  local kind=$1 n=$2 want got
  if [ "$kind" = W ]; then
    want=$(printf 'acknowledged-%s\n' "$n" | sha256sum | cut -d' ' -f1)
    got=$(tree_git_file "$gitdir" root "f$n" | sha256sum | cut -d' ' -f1) || fail "missing acknowledged write $n"
  else
    want=$(printf 'sandbox-%s\n' "$n" | sha256sum | cut -d' ' -f1)
    got=$(tree_git_file "$gitdir" "refs/heads/$FORK" "s$n" | sha256sum | cut -d' ' -f1) || fail "missing sandbox write $n"
  fi
  [ "$got" = "$want" ] || fail "wrong bytes for $kind write $n"
}
# A write with no fsync, marked once its close returned: its bytes (git-model §3, R12).
check_unsynced() {
  local n=$1 want got
  want=$(printf 'unsynced-%s\n' "$n" | sha256sum | cut -d' ' -f1)
  got=$(tree_git_file "$gitdir" root "x$n" | sha256sum | cut -d' ' -f1) || fail "missing unsynced write $n"
  [ "$got" = "$want" ] || fail "wrong bytes for unsynced write $n"
}
# A rename marked once an fsync of the view's directory returned: the new name,
# with the old one gone.
check_rename() {
  local want got
  want=$(printf 'renamed\n' | sha256sum | cut -d' ' -f1)
  got=$(tree_git_file "$gitdir" root r | sha256sum | cut -d' ' -f1) || fail "missing renamed file"
  [ "$got" = "$want" ] || fail "wrong bytes for renamed file"
  ! tree_git_file "$gitdir" root y >/dev/null 2>&1 || fail "old name of the rename still present"
}
while read -r kind n; do
  case "$kind" in
    W|SW) check_write "$kind" "$n" ;;
    XW) check_unsynced "$n" ;;
    RN) check_rename ;;
  esac
done < "$m/receipts"
if [ -n "${CHECK_MARK:-}" ]; then
  # Validate the external sequence through this mark, then require every
  # returned write represented by that prefix, including the current file.
  python3 - "$EXTERNAL_RECEIPTS" "$CHECK_MARK_KIND" "$CHECK_MARK_N" <<'PYMARK' || fail 'acknowledgement mark has no matching returned-call receipt'
import sys
lines = open(sys.argv[1]).read().splitlines()
kind, n = sys.argv[2], int(sys.argv[3])
assert kind in ('F', 'C', 'SF', 'SC', 'XW', 'RN') and n > 0
main = n if kind in ('F', 'C') else 8
forked = n if kind in ('SF', 'SC') else (2 if kind in ('XW', 'RN') else 0)
prefix = ['S']
for i in range(1, main + 1):
    prefix.extend([f'W {i}', f'F {i}'])
    if i < n or kind != 'F': prefix.append(f'C {i}')
for i in range(1, forked + 1):
    prefix.extend([f'SW {i}', f'SF {i}'])
    if i < forked or kind != 'SF': prefix.append(f'SC {i}')
if kind in ('XW', 'RN'):
    prefix.extend(f'XW {i}' for i in range(1, (n if kind == 'XW' else 2) + 1))
if kind == 'RN': prefix.append('RN 1')
assert lines[:len(prefix)] == prefix, (lines, prefix)
PYMARK
  case "$CHECK_MARK_KIND" in
    F|C) main_files=$CHECK_MARK_N; sandbox_files=0 ;;
    SF|SC) main_files=8; sandbox_files=$CHECK_MARK_N ;;
    *) main_files=8; sandbox_files=2 ;;
  esac
  for n in $(seq 1 "$main_files"); do check_write W "$n"; done
  for n in $(seq 1 "$sandbox_files"); do check_write SW "$n"; done
  case "$CHECK_MARK_KIND" in
    XW) for n in $(seq 1 "$CHECK_MARK_N"); do check_unsynced "$n"; done ;;
    RN) for n in 1 2; do check_unsynced "$n"; done; check_rename ;;
  esac
fi
tree_git_no_duplicate_opens "$gitdir" || fail "duplicate Agentfs-Open"
heads=$(git --git-dir="$gitdir" for-each-ref --format='%(refname) %(objectname)' refs/heads)
"$AGENTFS_BIN" --state "$m/state" trees init --volume t >/dev/null 2>&1 || fail 'second recovery'
[ "$(git --git-dir="$gitdir" for-each-ref --format='%(refname) %(objectname)' refs/heads)" = "$heads" ] || fail 'second recovery changed heads'
# The refs are a copy: without them, the journal alone brings back the same heads.
rm -rf "$gitdir/refs" "$gitdir/packed-refs"
"$AGENTFS_BIN" --state "$m/state" trees init --volume t >/dev/null 2>&1 || fail 'recovery without refs'
[ "$(git --git-dir="$gitdir" for-each-ref --format='%(refname) %(objectname)' refs/heads)" = "$heads" ] || fail 'refs differ after removing them'
printf 'PASS\t%s\t%s\tlast receipt %s\n' "$FS" "$point" "$(tail -n 1 "$m/receipts")" >> "$RESULTS"
