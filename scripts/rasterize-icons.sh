#!/usr/bin/env bash
# Rasterizes the vendored glyph SVGs under crates/app/assets/icons/svg/ into the
# committed PNGs under crates/app/assets/icons/glyphs/ (issue #59).
#
# Dev-time only: the PNGs are committed and `include_bytes!`d by
# crates/app/src/icons.rs, so a normal build never runs this. Re-run it after
# editing any SVG, then commit the regenerated PNGs.
#
# Requires resvg (cargo install resvg).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
svg="$root/crates/app/assets/icons/svg"
out="$root/crates/app/assets/icons/glyphs"
mkdir -p "$out"

# The emblem is drawn both at 60pt (header gutter) and at 200pt (background
# wash), so it needs a much larger raster than the 12-14pt glyphs. See
# docs/plans/2026-08-15-ui-mvvm-match.md for the sizing rationale.
render() {
  resvg -w "$2" -h "$2" "$svg/$1.svg" "$out/$1.png"
  echo "  $1.png ${2}x${2}"
}

echo "rasterizing glyphs:"
render emblem 512
for name in timer speed heart skull mouse_off pin share; do
  render "$name" 64
done
