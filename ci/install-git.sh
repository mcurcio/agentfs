#!/usr/bin/env bash
# Build the pinned release of git (ci/git-release: the release, then its tarball's
# sha256) under PREFIX, for CI's runners, whose own git is another release. The image
# gets the same release from Debian (Dockerfile). Line identity depends on it (MODEL.md,
# finding 42), so an upgrade is a known change in how later saves align repeated lines.
#   ci/install-git.sh PREFIX
set -euo pipefail
prefix=${1:?a prefix}
pin=$(dirname "$0")/git-release
release=$(sed -n 1p "$pin")
sum=$(sed -n 2p "$pin")
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT
sudo apt-get update
sudo apt-get install -y gcc make zlib1g-dev libcurl4-openssl-dev libexpat1-dev
curl -fsSL -o "$build/git.tar.xz" "https://www.kernel.org/pub/software/scm/git/git-$release.tar.xz"
echo "$sum  $build/git.tar.xz" | sha256sum -c -
tar -xJf "$build/git.tar.xz" -C "$build"
flags=(prefix="$prefix" NO_GETTEXT=1 NO_TCLTK=1 NO_PERL=1 NO_PYTHON=1)
make -C "$build/git-$release" -j"$(nproc)" "${flags[@]}" all >/dev/null
make -C "$build/git-$release" "${flags[@]}" install >/dev/null
