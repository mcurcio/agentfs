#!/usr/bin/env bash
# Check the Git operations phase 1's copy of the refs and its recovery rest on.
# Reftable is not used: `git` needs only the files format (git-model §12).
set -euo pipefail
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT
# Which release answers matters: line identity depends on it (MODEL.md, finding 42).
# The model and the library both look it up in agentfs_align::GIT_PATH, CI's jobs and
# the image are pinned to one release (ci/git-release), and each fails if it has another.
GIT_PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
for git in $(PATH=$GIT_PATH type -ap git); do
    echo "$git: $("$git" --version)"
done
release=$(sed -n 1p "$(dirname "$0")/git-release")
found=$(PATH=$GIT_PATH git --version)
if [ "$found" != "git version $release" ]; then
    echo "agentfs runs $found, and ci/git-release pins $release" >&2
    exit 1
fi
repo=$probe/repo.git
git init --bare "$repo"
export GIT_DIR=$repo GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=probe GIT_AUTHOR_EMAIL=probe@localhost GIT_COMMITTER_NAME=probe GIT_COMMITTER_EMAIL=probe@localhost
printf '[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n\tfsync = objects,derived-metadata\n\tfsyncMethod = fsync\n' > "$repo/config"
printf 'ref: refs/heads/root\n' > "$repo/HEAD"
test "$(git config --get core.fsync)" = objects,derived-metadata
blob=$(printf 'bytes' | git hash-object -w --stdin)
tree=$(printf '100644 blob %s\ta\n' "$blob" | git mktree)
commit=$(git commit-tree -m tip "$tree")
# A sync queue entry names the marker blob `sync` (docs/trees.md).
entry=$(printf 'sync' | git hash-object -w --stdin)
zero=0000000000000000000000000000000000000000
create() { printf 'start\0update refs/heads/root\0%s\0%s\0update refs/queue/root/00000000000000000001\0%s\0%s\0prepare\0commit\0' "$commit" "$zero" "$entry" "$zero" | git update-ref --stdin -z; }
create

# A rebuild over a damaged HEAD and damaged refs: fsck is clean, refs the same.
echo garbage > "$repo/HEAD"
echo garbage > "$repo/refs/heads/root"
rm -rf "$repo/refs" "$repo/packed-refs"
printf 'ref: refs/heads/root\n' > "$repo/HEAD"
mkdir "$repo/refs"
create
test "$(git rev-parse refs/heads/root)" = "$commit"
test "$(git rev-parse refs/queue/root/00000000000000000001)" = "$entry"
git fsck --full --strict --no-progress

# What is lost of a tip: --missing=print names it, whether blob, tree or tip.
lost() {
  printf '%s\n' "$@" | git rev-list --objects --no-walk --missing=print --stdin | sed -n 's/^?//p'
}
test -z "$(lost "$commit" "$entry")"
mv "$repo/objects/${blob:0:2}/${blob:2}" "$probe/blob"
test "$(lost "$commit" "$entry")" = "$blob"
mv "$probe/blob" "$repo/objects/${blob:0:2}/${blob:2}"
mv "$repo/objects/${entry:0:2}/${entry:2}" "$probe/entry"
test "$(lost "$commit" "$entry")" = "$entry"
mv "$probe/entry" "$repo/objects/${entry:0:2}/${entry:2}"

# Every object writer agentfs uses must read core.fsync (`git mktree` does not,
# which is why agentfs does not use it: tree_git.rs).
flushes() { grep -c '"category":"fsync","name":"hardware-flush"' "$1" || true; }
printf 'another' | GIT_TRACE2_EVENT=$probe/blob-trace git hash-object -w --stdin >/dev/null
[ "$(flushes "$probe/blob-trace")" -ge 1 ]
printf '100644 bytes\0' | cat - <(printf '%s' "$blob" | sed 's/../\\x&/g' | xargs -0 printf) | GIT_TRACE2_EVENT=$probe/tree-trace git hash-object -t tree -w --stdin >/dev/null
[ "$(flushes "$probe/tree-trace")" -ge 1 ]
GIT_TRACE2_EVENT=$probe/commit-trace git commit-tree -m another "$tree" >/dev/null
[ "$(flushes "$probe/commit-trace")" -ge 1 ]
