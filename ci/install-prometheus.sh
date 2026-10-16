#!/usr/bin/env bash
# Installs pinned prometheus and promtool into <dir>/bin, checking the
# release's sha256 sum, for the alerts workflow (ci/e2e-alerts.sh and
# the promtool checks of deploy/alerts). Linux amd64 only.
#
# Usage: ci/install-prometheus.sh <dir>
#
# Add <dir>/bin to PATH yourself.

set -euo pipefail

# Prometheus v3.15.0 (https://github.com/prometheus/prometheus/releases/tag/v3.15.0).
VERSION=3.15.0
SHA256=2a542df32eac02ee17b9d844fb2aa1de00dafa5476579ba8a3ba862e9d572ea0

dir=${1:?install directory}
mkdir -p "$dir/bin"
name=prometheus-$VERSION.linux-amd64
curl -fsSL --retry 4 --retry-all-errors -o "$dir/$name.tar.gz" \
  "https://github.com/prometheus/prometheus/releases/download/v$VERSION/$name.tar.gz"
echo "$SHA256  $dir/$name.tar.gz" | sha256sum -c --quiet -
tar -xzf "$dir/$name.tar.gz" -C "$dir" "$name/prometheus" "$name/promtool"
mv "$dir/$name/prometheus" "$dir/$name/promtool" "$dir/bin/"
rm -rf "${dir:?}/$name" "$dir/$name.tar.gz"
