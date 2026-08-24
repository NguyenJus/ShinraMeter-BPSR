#!/usr/bin/env bash
# Stages a built ShinraMeter-BPSR executable as a release asset.
#
# The asset *is* the executable — nothing else ships. Every icon set — the
# game-derived class icons (`crates/app/assets/classes/`), game-derived
# Imagine icons (`crates/app/assets/imagines/`), and the MIT-licensed/
# project-authored toolbar and glyph icons (`crates/app/assets/icons/`, incl.
# `icons/glyphs/`) — is compiled into the executable via `include_bytes!`
# (issue #123), so there is no asset tree left to stage alongside it.
#
# Issue #249: this used to wrap that single executable in a zip. It no longer
# does — a zip whose only member is one .exe buys nothing but an extract step
# (and a "why am I unzipping an .exe" moment for the user). What the zip *did*
# carry was the version, in its own filename; with the wrapper gone that has
# to live on the executable's filename instead, which is why this script
# copies to a caller-supplied <asset-name> rather than preserving the plain
# `ShinraMeter-BPSR.exe` name `cargo build` emits. Several downloaded builds in
# one folder stay distinguishable that way.
#
# Both ci.yml and release.yml call this script so local and CI packaging
# cannot drift from each other.
#
# Usage: scripts/package-release.sh <exe-path> <asset-name>
#
# <asset-name> must end in `.exe`: it is handed straight to `gh release
# create`, and a release asset that silently lost its extension is a download
# Windows refuses to run. Tested by scripts/package-release.test.sh.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <exe-path> <asset-name>" >&2
  exit 1
fi

exe_path=$1
asset_name=$2

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if [[ ! -f "$exe_path" ]]; then
  echo "::error::exe not found: $exe_path" >&2
  exit 1
fi

if [[ "$asset_name" != *.exe ]]; then
  echo "::error::asset name must end in .exe: $asset_name" >&2
  exit 1
fi

# A path separator in the asset name would write outside the repo root (or
# into a subdirectory the workflows then fail to find), so reject it rather
# than silently producing an asset nowhere near where the caller expects.
if [[ "$asset_name" == */* || "$asset_name" == *\\* ]]; then
  echo "::error::asset name must be a bare filename, not a path: $asset_name" >&2
  exit 1
fi

dest="$repo_root/$asset_name"
rm -f "$dest"
cp "$exe_path" "$dest"
# `cargo build` leaves the cross-compiled exe non-executable-bit-clean on some
# hosts; the bit is meaningless to Windows but keeps a locally staged asset
# runnable under Wine/WSL interop.
chmod +x "$dest"

echo "wrote $asset_name from $exe_path"
