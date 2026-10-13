#!/usr/bin/env bash
# Runs README.md's quick start as written (design §11.2, §18): the first
# ```sh block under "## Quick start", as root from the repository root, in
# one bash with `set -euxo pipefail`. Its `cargo build` runs as the calling
# user through a shim, so the build uses the user's toolchain and target
# directory; everything else runs as root, as the README says. Then checks
# that stopping the mount pushed the final commit: the remote's log names
# notes.txt, and the stop printed a `final push: origin: pushed` line.
#
# Needs Linux with /dev/fuse, fuse3, git, cargo and passwordless sudo, and
# no /data or /workspace (the README's paths); reproducible on a CI runner:
#
#   ci/readme-quickstart.sh

. "$(dirname "$0")/lib.sh"

[ -c /dev/fuse ] || fail "/dev/fuse is missing: the quick start needs FUSE"
sudo -n true 2>/dev/null || fail "the quick start runs as root: it needs passwordless sudo"
for path in /data /workspace; do
  [ ! -e "$path" ] || fail "$path exists: the quick start would write into it"
done

block=$(awk '
  /^## / { in_section = ($0 == "## Quick start") }
  in_section && !done && /^```sh$/ { inside = 1; next }
  inside && /^```$/ { inside = 0; done = 1; next }
  inside { print }
' README.md)
[ -n "$block" ] || fail "no sh block under ## Quick start in README.md"
step "the quick start"
printf '%s\n' "$block"

W=$(mktemp -d)
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

out=$W/out.log
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
