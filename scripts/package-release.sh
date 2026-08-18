#!/usr/bin/env bash
# Packages a built ShinraMeter-BPSR executable into a release zip.
#
# The zip ships the executable only. Every icon set — the game-derived class
# icons (`crates/app/assets/classes/`), game-derived Imagine icons
# (`crates/app/assets/imagines/`), and the MIT-licensed/project-authored
# toolbar and glyph icons (`crates/app/assets/icons/`, incl. `icons/glyphs/`)
# — is compiled into the executable via `include_bytes!` (issue #123), so
# there is no asset tree left to stage alongside it.
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

if [[ ! -f "$exe_path" ]]; then
  echo "::error::exe not found: $exe_path" >&2
  exit 1
fi

dist_dir="$repo_root/dist"
rm -rf "$dist_dir"
mkdir -p "$dist_dir"

cp "$exe_path" "$dist_dir/"

rm -f "$repo_root/$zip_name"
(
  cd "$dist_dir"
  zip -r "$repo_root/$zip_name" .
)

echo "wrote $zip_name from $dist_dir"
