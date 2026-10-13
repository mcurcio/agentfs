#!/usr/bin/env bash
# Builds xfstests' replay-log (src/log-writes), the tool that replays a
# dm-log-writes log onto a device entry by entry, from a pinned commit,
# each source file checked against its SHA-256. Prints
# REPLAY_LOG=<path> for $GITHUB_ENV.
#
#   ci/install-replay-log.sh <dir>
#
# Needs curl and gcc.

set -euo pipefail

dir=${1:?usage: ci/install-replay-log.sh <dir>}
commit=3e1ee800e52a0f53d7d9a7809be1ffc80ec1788f # kdave/xfstests master of 2026-08-28
base=https://raw.githubusercontent.com/kdave/xfstests/$commit/src/log-writes

mkdir -p "$dir"
while read -r sum file; do
  curl -fsSL --retry 3 -o "$dir/$file" "$base/$file"
  echo "$sum  $dir/$file" | sha256sum -c --quiet - >&2
done <<'SUMS'
4199c2398acdaa4ae23b6355222069b5e82cfdbf42ff643918394a245202f3c3 replay-log.c
158c62d45e751b6376a7ba7829db52e9fbf8db56c70dd941cee82906c0528abd log-writes.c
56d491aacac35c827cea5933b99371bcfe2605985c1e513b39e8fcf6883ae3d8 log-writes.h
SUMS
gcc -O2 -Wall -o "$dir/replay-log" "$dir/replay-log.c" "$dir/log-writes.c" >&2
echo "REPLAY_LOG=$dir/replay-log"
