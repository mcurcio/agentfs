#!/usr/bin/env bash
# Read published tree contents by the commit's format marker, including legacy roots.
tree_git_file() {
  local gitdir=$1 commit=$2 path=$3 prefix=
  if git --git-dir="$gitdir" show -s --format=%B "$commit" | grep -Fx 'Agentfs-Format: envelope-1' >/dev/null; then
    prefix=files/
    git --git-dir="$gitdir" cat-file -e "$commit:metadata.json" || return 1
  fi
  git --git-dir="$gitdir" show "$commit:$prefix$path"
}

tree_git_no_duplicate_opens() {
  local duplicates
  duplicates=$(git --git-dir="$1" log root --format='%(trailers:key=Agentfs-Open,valueonly)' | sed '/^$/d' | sort | uniq -d) || return 1
  [ -z "$duplicates" ] || { echo "duplicate Agentfs-Open: $duplicates" >&2; return 1; }
}
