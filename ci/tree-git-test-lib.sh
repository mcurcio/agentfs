#!/usr/bin/env bash
# Read published tree contents by the commit's format marker, including legacy roots.

# A file's place in a commit or checkout, <aa>/<bb>/<name>: aa and bb are hex
# digits 1-2 and 3-4 of `git hash-object --stdin` of the name (docs/trees.md).
tree_git_fanned() {
  local hex
  hex=$(printf %s "$1" | git hash-object --stdin) || return 1
  printf '%s/%s/%s\n' "${hex:0:2}" "${hex:2:2}" "$1"
}

tree_git_file() {
  local gitdir=$1 commit=$2 path=$3
  if git --git-dir="$gitdir" show -s --format=%B "$commit" | grep -Fx 'Agentfs-Format: envelope-1' >/dev/null; then
    git --git-dir="$gitdir" cat-file -e "$commit:lines/$(tree_git_fanned "$path")" || return 1
    path=files/$(tree_git_fanned "$path")
  fi
  git --git-dir="$gitdir" show "$commit:$path"
}

tree_git_no_duplicate_opens() {
  local duplicates
  duplicates=$(git --git-dir="$1" log root --format='%(trailers:key=Agentfs-Open,valueonly)' | sed '/^$/d' | sort | uniq -d) || return 1
  [ -z "$duplicates" ] || { echo "duplicate Agentfs-Open: $duplicates" >&2; return 1; }
}

# The tip of the root's Clients chain in Git (refs/agentfs/clients/root); empty
# before its first event is built.
tree_git_root_clients() {
  git --git-dir="$1" rev-parse -q --verify refs/agentfs/clients/root 2>/dev/null || true
}

# Waits until a mount's attach (or rebind) of the root's client is built: the
# root's Clients chain in Git has moved past `before`, its tip before the mount.
# A mount of the root alone records that one step, one batch for the builder,
# before it serves, so a failpoint counted past the mount's own hits then meets
# the next client call's.
tree_git_wait_root_attached() {
  local gitdir=$1 before=$2
  for _ in $(seq 1 300); do
    [ "$(tree_git_root_clients "$gitdir")" = "$before" ] || return 0
    sleep .1
  done
  echo 'the mount did not attach its root client' >&2; return 1
}
