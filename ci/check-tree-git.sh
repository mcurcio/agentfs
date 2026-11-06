#!/usr/bin/env bash
# Check the Git operations phase 1's copy of the refs and its recovery rest on.
# Reftable is not used: `git` needs only the files format (git-model §12).
set -euo pipefail
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT
git --version
repo=$probe/repo.git
git init --bare "$repo"
export GIT_DIR=$repo
printf '[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n\tfsync = objects,derived-metadata\n\tfsyncMethod = fsync\n' > "$repo/config"
printf 'ref: refs/heads/main\n' > "$repo/HEAD"
test "$(git config --get core.fsync)" = objects,derived-metadata
blob=$(printf 'bytes' | git hash-object -w --stdin)
tree=$(printf '100644 blob %s\ta\n' "$blob" | git mktree)
commit=$(git commit-tree -m tip "$tree")
table=$(printf 'table' | git hash-object -w --stdin)
zero=0000000000000000000000000000000000000000
create() { printf 'start\0update refs/heads/main\0%s\0%s\0update refs/agentfs/objects/main\0%s\0%s\0prepare\0commit\0' "$commit" "$zero" "$table" "$zero" | git update-ref --stdin -z; }
create

# A rebuild over a damaged HEAD and damaged refs: fsck is clean, refs the same.
echo garbage > "$repo/HEAD"
echo garbage > "$repo/refs/heads/main"
rm -rf "$repo/refs" "$repo/packed-refs"
printf 'ref: refs/heads/main\n' > "$repo/HEAD"
mkdir "$repo/refs"
create
test "$(git rev-parse refs/heads/main)" = "$commit"
test "$(git rev-parse refs/agentfs/objects/main)" = "$table"
git fsck --full --strict --no-progress

# What is lost of a tip: --missing=print names it, whether blob, tree or tip.
lost() {
  printf '%s\n' "$@" | git rev-list --objects --no-walk --missing=print --stdin | sed -n 's/^?//p'
}
test -z "$(lost "$commit" "$table")"
mv "$repo/objects/${blob:0:2}/${blob:2}" "$probe/blob"
test "$(lost "$commit" "$table")" = "$blob"
mv "$probe/blob" "$repo/objects/${blob:0:2}/${blob:2}"
mv "$repo/objects/${table:0:2}/${table:2}" "$probe/table"
test "$(lost "$commit" "$table")" = "$table"
mv "$probe/table" "$repo/objects/${table:0:2}/${table:2}"
