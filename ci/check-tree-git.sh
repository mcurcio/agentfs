#!/usr/bin/env bash
# Check the Git operations used by the phase-1 reftable backend.
set -euo pipefail
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT
git --version
git init --bare --ref-format=reftable "$probe/new"
test "$(git --git-dir="$probe/new" rev-parse --show-ref-format)" = reftable
git init --bare --ref-format=files "$probe/old"
git --git-dir="$probe/old" refs migrate --ref-format=reftable
printf 'start\0prepare\0commit\0' | git --git-dir="$probe/new" update-ref --stdin -z
