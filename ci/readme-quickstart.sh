#!/usr/bin/env bash
# Runs a README.md quick start as written (design §11.2, §18): the first
# ```sh block under its heading in "## Quick start", in one bash with
# `set -euxo pipefail`, then checks what it should have left.
#
#   ci/readme-quickstart.sh          "### From source", as root, from the
#                                    repository root (the fuse job, ci.yml)
#   ci/readme-quickstart.sh docker   "### Docker", as the calling user
#                                    (the readme-docker job, docs.yml)
#
# From source: its `cargo build` runs as the calling user through a shim,
# so the build uses the user's toolchain and target directory; everything
# else runs as root, as the README says. Then checks that stopping the
# mount pushed the final commit: the remote's log names notes.txt, and the
# stop printed a `final push: origin: pushed` line. Needs Linux with
# /dev/fuse, fuse3, git, cargo and passwordless sudo, and no /data or
# /workspace (the README's paths).
#
# Docker: runs the block against $AGENTFS_IMAGE (the README's default,
# ghcr.io/mcurcio/agentfs:latest, when unset), then checks that the
# history left in the Docker volume holds the workload's hello.txt with
# uid 10000 as its author, and that nothing is left mounted. Needs Linux
# with Docker Engine 25+, the fuse module, and no volume agentfs-example.
#
# Both are reproducible on a CI runner.

. "$(dirname "$0")/lib.sh"

MODE=${1:-source}
case "$MODE" in
source) heading="### From source" ;;
docker) heading="### Docker" ;;
*) echo "usage: $0 [source|docker]" >&2; exit 2 ;;
esac

# The first ```sh block under $heading, within "## Quick start". Lines in
# fenced blocks are never headings.
block=$(awk -v h="$heading" '
  /^```/ {
    if (!fence) { fence = 1; inside = in_heading && !done && $0 == "```sh" }
    else { fence = 0; if (inside) { inside = 0; done = 1 } }
    next
  }
  fence { if (inside) print; next }
  /^## / { in_section = ($0 == "## Quick start") }
  /^#/ { in_heading = in_section && ($0 == h) }
' README.md)
[ -n "$block" ] || fail "no sh block under $heading in README.md's ## Quick start"

W=$(mktemp -d)
out=$W/out.log

if [ "$MODE" = docker ]; then
  IMAGE=${AGENTFS_IMAGE:-ghcr.io/mcurcio/agentfs:latest}
  MNT=examples/docker/mnt-run
  ! docker volume inspect agentfs-example >/dev/null 2>&1 ||
    fail "the Docker volume agentfs-example exists: the quick start would reuse it"
  cleanup() {
    status=$?
    docker rm -f app agentfs-example >/dev/null 2>&1 || true
    docker volume rm agentfs-example >/dev/null 2>&1 || true
    sudo umount -l "$PWD/$MNT/workspace" 2>/dev/null || true
    sudo rm -rf "$MNT" "$W"
    exit $status
  }
  trap cleanup EXIT

  step "the quick start ($heading)"
  printf '%s\n' "$block"
  # pipefail (lib.sh): a failing block fails the script.
  bash -euxo pipefail -c "$block" 2>&1 | tee "$out"

  step "checks"
  assert_contains "$(cat "$out")" "final commit" "stopping made the final commit"
  log=$(docker run --rm -v agentfs-example:/data --entrypoint git "$IMAGE" \
    --git-dir /data/state/git log --name-only --format='author %an' agentfs/example)
  assert_contains "$log" "hello.txt" "the history holds the workload's hello.txt"
  assert_contains "$log" "author uid-10000" "uid 10000 is the author"
  if grep -q " $PWD/$MNT/workspace " /proc/self/mountinfo; then
    fail "$MNT/workspace is still mounted"
  fi
  pass "nothing is left mounted"
  exit 0
fi

[ -c /dev/fuse ] || fail "/dev/fuse is missing: the quick start needs FUSE"
sudo -n true 2>/dev/null || fail "the quick start runs as root: it needs passwordless sudo"
for path in /data /workspace; do
  [ ! -e "$path" ] || fail "$path exists: the quick start would write into it"
done

step "the quick start ($heading)"
printf '%s\n' "$block"

cleanup() {
  status=$?
  sudo sh -c 'grep -q " /workspace " /proc/self/mountinfo && umount -l /workspace' || true
  sudo rm -rf /data /workspace
  rm -rf "$W"
  exit $status
}
trap cleanup EXIT

# cargo as the calling user, with the calling user's PATH.
mkdir -p "$W/shim"
cat >"$W/shim/cargo" <<SHIM
#!/bin/sh
exec sudo -u '$(id -un)' env 'PATH=$PATH' 'HOME=$HOME' cargo "\$@"
SHIM
chmod 755 "$W/shim/cargo"

sudo env "PATH=$W/shim:$PATH" bash -euxo pipefail -c "$block" 2>&1 | tee "$out"

step "checks"
text=$(cat "$out")
assert_contains "$text" "final commit: " "the stop made the final commit"
assert_contains "$text" "final push: origin: pushed" "the stop pushed the final commit"
log=$(sudo git --git-dir /data/remote.git log --name-only --format=%s agentfs/demo)
assert_contains "$log" "notes.txt" "the remote's history has notes.txt"
if grep -q " /workspace " /proc/self/mountinfo; then
  fail "/workspace is still mounted"
fi
pass "nothing is left mounted"
