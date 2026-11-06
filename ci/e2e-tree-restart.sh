#!/usr/bin/env bash
# A view-host restart with handles open (git-model §18), on Linux CI only.
set -euo pipefail
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
cleanup() {
  status=$?
  fusermount3 -uz "$work/main" >/dev/null 2>&1 || true
  [ "$status" = 0 ] || cat "$work/mount.log" >&2 || true
  rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main"
"$bin" --state "$work/state" trees init
python3 ci/trees-restart.py "$bin" "$work/state" "$work/main"
git --git-dir="$work/state/history.git" fsck --strict
