#!/usr/bin/env bash
# Installs pinned helm-docs into <dir>/bin, checking its sha256 sum, for
# the helm workflow's lint job (the chart READMEs, docs/helm.md §6).
# Linux amd64 only. Add <dir>/bin to PATH yourself.
#
# Usage: ci/install-helm-docs.sh <dir>

set -euo pipefail

# helm-docs v1.14.2 (https://github.com/norwoodj/helm-docs/releases/tag/v1.14.2)
VERSION=1.14.2
SHA256=a8cf72ada34fad93285ba2a452b38bdc5bd52cc9a571236244ec31022928d6cc

dir=${1:?install directory}
mkdir -p "$dir/bin"
tgz="$dir/helm-docs.tar.gz"
curl -fsSL --retry 4 --retry-all-errors -o "$tgz" \
  "https://github.com/norwoodj/helm-docs/releases/download/v$VERSION/helm-docs_${VERSION}_Linux_x86_64.tar.gz"
echo "$SHA256  $tgz" | sha256sum -c --quiet -
tar -xzf "$tgz" -C "$dir/bin" helm-docs
"$dir/bin/helm-docs" --version
