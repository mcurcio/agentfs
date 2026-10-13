#!/usr/bin/env bash
# One power-cut point of ci/power-loss.sh, run by replay-log at every FUA
# or flush entry of the replay (or once at the end, with CHECK_FINAL=1).
# Mounts a snapshot of the replayed device (so recovery writes never
# reach the device the replay continues on), then checks:
#
# - the filesystem mounts, `agentfs flush` recovers and leaves no backlog;
# - `git fsck --full --strict` passes on the history and the remote;
# - Agentfs-Seq strictly increases along the branch, with no empty commit;
# - every record whose promise is on disk is in history: each path is at
#   its last promised state, or at the state of the one record in flight
#   (CHECK_FINAL: exactly at its last state);
# - every pushed head whose promise is on disk is on the remote branch.
#
# Environment from ci/power-loss.sh: REPLAY_DEV, COW_DEV, CHECK_MNT,
# CHECK_DM, MANIFEST, CHECK_COUNT, FS, AGENTFS_BIN.

set -euo pipefail
trap 'echo "power-cut check FAILED: line $LINENO: $BASH_COMMAND" >&2' ERR

fail() { printf 'power-cut check FAILED: %s\n' "$*" >&2; exit 1; }

cleanup() {
  umount "$CHECK_MNT" 2>/dev/null || true
  dmsetup remove "$CHECK_DM" 2>/dev/null || true
}
trap cleanup EXIT

dmsetup create "$CHECK_DM" --table \
  "0 $(blockdev --getsz "$REPLAY_DEV") snapshot $REPLAY_DEV $COW_DEV N 8"
mount "/dev/mapper/$CHECK_DM" "$CHECK_MNT" || fail "$FS does not mount"
echo x >>"$CHECK_COUNT"

m=$CHECK_MNT
promises=$m/promises
# Before the workload's setup was durable there is nothing to check.
grep -qx S "$promises" 2>/dev/null || exit 0

export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null HOME=$m
agentfs_() { "$AGENTFS_BIN" --state "$m/state" --root "$m/tree" "$@"; }
branch=refs/heads/agentfs/demo
git_() { git --git-dir "$m/state/git" "$@"; }

out=$(agentfs_ flush 2>&1) || fail "agentfs flush: $out"
status=$(agentfs_ status 2>&1) || fail "agentfs status: $status"
case $status in *"backlog:   0 record(s)"*) ;; *) fail "a backlog after flush: $status" ;; esac
[ -z "$(ls -A "$m/state/pending")" ] || fail "pending/ is not empty after flush"
git_ fsck --full --strict --no-progress >/dev/null 2>&1 ||
  fail "git fsck of the history: $(git_ fsck --full --strict --no-progress 2>&1 | tail -n 5)"
git --git-dir "$m/remote.git" fsck --full --strict --no-progress >/dev/null 2>&1 ||
  fail "git fsck of the remote"

last=$(grep '^R ' "$promises" | tail -n 1 | cut -d' ' -f2)
last=${last:-0}

if git_ rev-parse -q --verify "$branch" >/dev/null; then
  prev=
  for seq in $(git_ log --first-parent --format='%(trailers:key=Agentfs-Seq,valueonly)' "$branch"); do
    if [ -n "$prev" ] && [ "$seq" -ge "$prev" ]; then
      fail "Agentfs-Seq $seq comes before $prev on the first-parent history"
    fi
    prev=$seq
  done
  for c in $(git_ rev-list --first-parent "$branch"); do
    if git_ rev-parse -q --verify "$c^" >/dev/null && git_ diff --quiet "$c^" "$c"; then
      fail "empty commit $c"
    fi
  done
elif [ "$last" != 0 ]; then
  fail "record $last was promised, but there is no branch"
fi

# Each path: its last promised state, or the next record's if that one
# was in flight (it may have reached the journal before the cut).
in_flight=$((last + 1))
[ "${CHECK_FINAL:-0}" = 1 ] && in_flight=0
for path in $(cut -d' ' -f2 "$MANIFEST" | sort -u); do
  want=absent
  while read -r n p sum; do
    [ "$p" = "$path" ] && [ "$n" -le "$last" ] && want=$sum
  done <"$MANIFEST"
  [ "$want" = - ] && want=absent
  also=$want
  while read -r n p sum; do
    if [ "$n" = "$in_flight" ] && [ "$p" = "$path" ]; then
      also=$sum
      [ "$also" = - ] && also=absent
    fi
  done <"$MANIFEST"
  if git_ cat-file -e "$branch:$path" 2>/dev/null; then
    got=$(git_ cat-file blob "$branch:$path" | sha256sum | cut -d' ' -f1)
  else
    got=absent
  fi
  if [ "$got" != "$want" ] && [ "$got" != "$also" ]; then
    fail "$path is $got in history; record $last promised $want (in flight: $also)"
  fi
done

for oid in $(grep '^U ' "$promises" | cut -d' ' -f2); do
  git --git-dir "$m/remote.git" merge-base --is-ancestor "$oid" "$branch" 2>/dev/null ||
    fail "pushed head $oid is not on the remote branch"
done
