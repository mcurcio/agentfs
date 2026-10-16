#!/usr/bin/env bash
# Renders each logo SVG under docs/assets/ to the PNG beside it with
# pinned resvg (ci/install-resvg.sh). The SVGs are the source: hand-drawn
# on an integer pixel grid with shape-rendering="crispEdges", so every
# pixel edge lands on a PNG pixel edge and the render is exact.
#
# Usage: ci/render-assets.sh           rewrite the PNGs
#        ci/render-assets.sh --check   fail if a committed PNG differs from
#                                      a fresh render (the docs workflow)
#
# resvg must be on PATH, or named by RESVG.

set -euo pipefail

cd "$(dirname "$0")/.."
RESVG=${RESVG:-resvg}
check=false
case "${1:-}" in
  --check) check=true ;;
  "") ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

# file, then resvg's size option for its PNG: a whole-number scale of the
# pixel grid, so no pixel is split.
ASSETS=(
  "logo.svg --width 512"
  "logo-dark.svg --width 512"
  "wordmark.svg --zoom 2"
  "wordmark-dark.svg --zoom 2"
  "icon.svg --width 512"
  "social-preview.svg --zoom 1"
)

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
failed=0
for entry in "${ASSETS[@]}"; do
  read -r svg size_opt size <<<"$entry"
  src=docs/assets/$svg
  png=${src%.svg}.png
  grep -q 'shape-rendering="crispEdges"' "$src" || {
    echo "$src: the root <svg> needs shape-rendering=\"crispEdges\"" >&2
    failed=1
  }
  "$RESVG" "$size_opt" "$size" "$src" "$out/${svg%.svg}.png"
  if $check; then
    if ! cmp -s "$out/${svg%.svg}.png" "$png"; then
      echo "$png does not match $src: run ci/render-assets.sh and commit the PNG" >&2
      failed=1
    fi
  else
    cp "$out/${svg%.svg}.png" "$png"
  fi
done
# Every SVG in docs/assets/ has a row above, so none is left unrendered.
for src in docs/assets/*.svg; do
  name=$(basename "$src")
  printf '%s\n' "${ASSETS[@]}" | grep -q "^$name " || {
    echo "$src has no entry in ci/render-assets.sh" >&2
    failed=1
  }
done
exit "$failed"
