#!/usr/bin/env bash
# Installs pinned resvg into <dir>/bin, checking its sha256 sum, for
# ci/render-assets.sh (the logo PNGs under docs/assets/). Linux x86_64,
# which the docs workflow runs, and macOS arm64, for rendering locally.
# Add <dir>/bin to PATH yourself.
#
# Usage: ci/install-resvg.sh <dir>

set -euo pipefail

# resvg v0.48.1 (https://github.com/linebender/resvg/releases/tag/v0.48.1)
VERSION=0.48.1
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    asset=resvg-linux-x86_64.tar.gz
    SHA256=fa8c26495a187e592c501db15bf9e8a9fdc051d4b2b336b39703d5b59f912b9d
    ;;
  Darwin-arm64)
    asset=resvg-macos-aarch64.zip
    SHA256=06440eb5aa14a28cbfc7e40ae39e1ffa71adc051b89fbaa913b4f1d9b905d09f
    ;;
  *)
    echo "install-resvg.sh: no pinned resvg build for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

dir=${1:?install directory}
mkdir -p "$dir/bin"
archive="$dir/$asset"
curl -fsSL --retry 4 --retry-all-errors -o "$archive" \
  "https://github.com/linebender/resvg/releases/download/v$VERSION/$asset"
if command -v sha256sum >/dev/null; then
  echo "$SHA256  $archive" | sha256sum -c --quiet -
else
  echo "$SHA256  $archive" | shasum -a 256 -c --quiet -
fi
case "$asset" in
  *.zip) unzip -oq "$archive" resvg -d "$dir/bin" ;;
  *) tar -xzf "$archive" -C "$dir/bin" resvg ;;
esac
"$dir/bin/resvg" --version
