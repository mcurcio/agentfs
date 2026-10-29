#!/usr/bin/env bash
# mergiraf as a merge driver in the built image (design §12, §17.3; bead
# agentfs-mia.3, acceptance 5). Two volumes, each with two patches that
# change different functions on adjacent lines of one .rs file, which a
# line merge calls a conflict:
#
# - with `merge.mergiraf.driver` in the volume's config and
#   `*.rs merge=mergiraf` in its `$GIT_DIR/info/attributes`, one
#   `agentfs patch apply` lands both and exits 0;
# - the control, the same patches without the attribute, holds the second
#   as a conflict, so it is mergiraf that merged them.
#
#   ci/e2e-mergiraf.sh           runs itself in the image ($AGENTFS_IMAGE,
#                                default agentfs:ci), as `inside`
#   ci/e2e-mergiraf.sh inside    the test, in a container of the image
#
# Needs docker and the image loaded. No FUSE: the queue runs on the
# backing tree with nothing attached, as at mount.

set -euo pipefail

if [ "${1:-}" != inside ]; then
  CI_DIR=$(cd "$(dirname "$0")" && pwd)
  exec docker run --rm -v "$CI_DIR:/ci:ro" --entrypoint bash \
    "${AGENTFS_IMAGE:-agentfs:ci}" /ci/e2e-mergiraf.sh inside
fi

export AGENTFS_BIN=${AGENTFS_BIN:-/usr/local/bin/agentfs}
. "$(dirname "$0")/lib.sh"

W=$(mktemp -d)
export HOME=$W/home GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
  GIT_AUTHOR_NAME=Producer GIT_AUTHOR_EMAIL=producer@example.com \
  GIT_COMMITTER_NAME=Producer GIT_COMMITTER_EMAIL=producer@example.com
mkdir -p "$HOME"

step "versions"
"$AGENTFS_BIN" --version
git --version
command -v mergiraf || fail "mergiraf is not on the PATH"
mergiraf --version

# One volume `demo` with lib.rs pushed to a bare remote, and patch 1
# (alpha) and patch 2 (beta) queued from two producers cut at that head.
# $1: "driver" sets up mergiraf, "control" does not.
volume_with_two_patches() {
  new_volume demo
  REMOTE=$VOL/remote.git
  git init -q --bare "$REMOTE"
  volume_config remote.origin.url "file://$REMOTE"
  if [ "$1" = driver ]; then
    volume_config merge.mergiraf.name mergiraf
    volume_config merge.mergiraf.driver \
      'mergiraf merge --git %O %A %B -s %S -x %X -y %Y -p %P -l %L'
    mkdir -p "$STATE/git/info"
    printf '*.rs merge=mergiraf\n' >"$STATE/git/info/attributes"
  fi
  printf 'fn alpha() -> u32 { 1 }\nfn beta() -> u32 { 2 }\n' >"$TREE/lib.rs"
  agentfs record lib.rs >/dev/null
  agentfs push >/dev/null
  for p in alpha beta; do
    git clone -q "file://$REMOTE" "$VOL/$p"
    git -C "$VOL/$p" checkout -q agentfs/demo
  done
  sed -i 's/{ 1 }/{ 10 }/' "$VOL/alpha/lib.rs"
  git -C "$VOL/alpha" commit -qam "alpha returns 10"
  sed -i 's/{ 2 }/{ 20 }/' "$VOL/beta/lib.rs"
  git -C "$VOL/beta" commit -qam "beta returns 20"
  for p in alpha beta; do
    "$AGENTFS_BIN" patch submit --volume demo --repo "$VOL/$p" HEAD
  done
}

step "control: without the attribute, the second patch conflicts"
volume_with_two_patches control
status=0
out=$(agentfs patch apply 2>&1) || status=$?
echo "$out"
assert_eq 1 "$status" "apply exits 1 with a patch held"
assert_contains "$out" "patch 2 held (conflict)" "patch 2 is held as a conflict"
assert_contains "$(cat "$TREE/lib.rs")" "{ 10 }" "patch 1 landed"

step "mergiraf merges both patches"
volume_with_two_patches driver
status=0
out=$(agentfs patch apply 2>&1) || status=$?
echo "$out"
assert_eq 0 "$status" "apply exits 0"
assert_eq "$(printf 'fn alpha() -> u32 { 10 }\nfn beta() -> u32 { 20 }')" "$(cat "$TREE/lib.rs")" \
  "both patches are in lib.rs"
for n in 1 2; do
  git --git-dir "$STATE/git" log --format=%B refs/heads/agentfs/demo |
    grep -qx "Agentfs-Inbox: $n" || fail "no commit applies patch $n"
done
pass "each patch is one commit on the branch"
