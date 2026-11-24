#!/usr/bin/env bash
# Shared mappings on a real mount (ledger R1.4), on Linux CI only: the page-cache
# cases, the strict mapping script, then close-to-open on a mount of its own.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
cleanup() {
  status=$?
  fusermount3 -uz "$work/main" >/dev/null 2>&1 || true
  fusermount3 -uz "$work/fork" >/dev/null 2>&1 || true
  [ -z "$pid" ] || tree_stop "$pid"
  [ "$status" = 0 ] || cat "$work/mount.log" >&2 || true
  rm -rf "$work"
}
on_exit cleanup
wait_mounted() {
  for _ in $(seq 1 200); do
    if mountpoint -q "$1"; then return; fi
    kill -0 "$pid" || { cat "$work/mount.log" >&2; exit 1; }
    sleep .1
  done
  mountpoint -q "$1"
}
mkdir "$work/main" "$work/fork"
"$bin" --state "$work/state" init --volume t
"$bin" --state "$work/state" mount "$work/main" --fork-mount "$work/fork" >"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
wait_mounted "$work/fork"
result=0
python3 ci/trees-pagecache.py "$work/main" "$work/state/history.git" "$work/fork" || result=$?
python3 ci/trees-mmap.py "$work/main" "$work/state/history.git" || result=$?
fusermount3 -u "$work/main"
fusermount3 -u "$work/fork"
tree_wait_exit "$pid"; pid=
git --git-dir="$work/state/history.git" fsck --strict

# A fork's takes would hold the actor too: this runs against the saves alone.
"$bin" --state "$work/close-open" init --volume t
# trees-close-open.py moves a file out of the tree to a name this places in
# the untracked area.
mkdir -p "$work/close-open/history.git/info"
printf '*.out\n' >"$work/close-open/history.git/info/exclude"
"$bin" --state "$work/close-open" mount "$work/main" >>"$work/mount.log" 2>&1 & pid=$!
wait_mounted "$work/main"
python3 ci/trees-close-open.py "$work/main" || result=$?
fusermount3 -u "$work/main"
tree_wait_exit "$pid"; pid=

# A delete waits for the file's pending save: the commits touching the file
# unlinked at once after its close are the delete, then that save, whose
# content is the overwrite (64 MiB of "o" with its last byte "n", as
# trees-close-open.py writes).
gitdir=$work/close-open/history.git
name=unlinked-at-once
mapfile -t touching < <(git --git-dir="$gitdir" log --format='%H %s' root -- "files/$(tree_git_fanned "$name")")
if [ "${#touching[@]}" -lt 2 ] || [[ "${touching[0]#* }" != "Unlink by "* ]]; then
  echo "close then unlink: the newest commit of $name is not its delete: ${touching[*]}" >&2
  result=1
elif ! cmp -s <(tree_git_file "$gitdir" "${touching[1]%% *}" "$name") \
  <(head -c $(((64 << 20) - 1)) /dev/zero | tr '\0' o; printf n); then
  echo "close then unlink: the commit before the delete is not the save: ${touching[1]}" >&2
  result=1
else
  echo "close then unlink: history has the save, then the delete"
fi
exit "$result"
