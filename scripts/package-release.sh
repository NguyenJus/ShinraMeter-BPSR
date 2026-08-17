#!/usr/bin/env bash
# Packages a built ShinraMeter-BPSR executable into a release zip.
#
# Ships as files under dist/assets/, loaded from disk at runtime:
#   - crates/app/assets/classes/   (game-derived class icons, issue #103)
#   - crates/app/assets/imagines/  (game-derived Imagine icons, issue #103)
#
# Stays embedded in the executable and is NOT copied here:
#   - crates/app/assets/icons/ (incl. icons/glyphs/), svg/, shinra.ico
#     (MIT-licensed / project-authored art with no takedown exposure, plus
#     the Win32 .ico resource linked by build.rs)
#
# Both ci.yml and release.yml call this script so local and CI packaging
# cannot drift from each other.
#
# Usage: scripts/package-release.sh <exe-path> <zip-name>
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <exe-path> <zip-name>" >&2
  exit 1
fi

exe_path=$1
zip_name=$2

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
assets_root="$repo_root/crates/app/assets"

if [[ ! -f "$exe_path" ]]; then
  echo "::error::exe not found: $exe_path" >&2
  exit 1
fi

dist_dir="$repo_root/dist"
rm -rf "$dist_dir"
mkdir -p "$dist_dir/assets/classes" "$dist_dir/assets/imagines"

cp "$exe_path" "$dist_dir/"

for tree in classes imagines; do
  src="$assets_root/$tree"
  if [[ ! -d "$src" ]] || [[ -z "$(ls -A "$src" 2>/dev/null)" ]]; then
    echo "::error::$src is missing or empty; refusing to ship an icon-less zip" >&2
    exit 1
  fi
  cp -R "$src/." "$dist_dir/assets/$tree/"
done

(
  cd "$dist_dir"
  zip -r "$repo_root/$zip_name" .
)

echo "wrote $zip_name from $dist_dir"
