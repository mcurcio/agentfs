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
