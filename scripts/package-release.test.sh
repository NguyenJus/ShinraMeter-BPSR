#!/usr/bin/env bash
# Tests for scripts/package-release.sh (issue #249).
#
# The packaging step is the one part of the release path no Rust test covers
# and that a tag push runs exactly once, unattended, with the tag already
# published — a naming mistake there is only discoverable after the fact.
# These assertions pin the contract both ci.yml and release.yml depend on: the
# staged asset lands at <repo-root>/<asset-name>, byte-identical to the input,
# and a name that would produce an unrunnable or misplaced download is
# rejected before anything is written.
#
# The script under test derives its repo root from its own `BASH_SOURCE`, so
# every case runs against a *copy* of it inside a scratch directory. Nothing
# here writes into the real working tree.
#
# Runs on any POSIX host (no Windows, no cargo) — the "exe" under test is just
# an opaque file to `cp`. Wired into ci.yml's `fmt` job.
set -uo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

failures=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; failures=$((failures + 1)); }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A stand-in repo root, so the script's `dirname $0/..` lands in $work/repo.
mkdir -p "$work/repo/scripts"
cp "$script_dir/package-release.sh" "$work/repo/scripts/package-release.sh"
package="$work/repo/scripts/package-release.sh"
root="$work/repo"

fake_exe="$work/ShinraMeter-BPSR.exe"
printf 'MZ\220\000fake pe image' > "$fake_exe"

# -- the version-bearing name release.yml builds ------------------------
asset="ShinraMeter-BPSR-v0.2.5-windows-x64.exe"
if "$package" "$fake_exe" "$asset" > /dev/null && [[ -f "$root/$asset" ]]; then
  pass "stages the asset at <repo-root>/$asset"
else
  fail "stages the asset at <repo-root>/$asset"
fi

if cmp -s "$fake_exe" "$root/$asset"; then
  pass "the staged asset is byte-identical to the built exe"
else
  fail "the staged asset is byte-identical to the built exe"
fi

# No zip, and no `dist/` staging tree, is left behind for the workflows to
# trip over — the asset is the only thing produced (issue #249).
if [[ ! -e "$root/dist" && -z "$(find "$root" -maxdepth 1 -name '*.zip' -print -quit)" ]]; then
  pass "leaves neither a zip nor a dist/ staging tree behind"
else
  fail "leaves neither a zip nor a dist/ staging tree behind"
fi

# -- ci.yml's unversioned name (no tag on a branch build) ---------------
ci_asset="ShinraMeter-BPSR-windows-x64.exe"
if "$package" "$fake_exe" "$ci_asset" > /dev/null && [[ -f "$root/$ci_asset" ]]; then
  pass "stages ci.yml's unversioned asset name"
else
  fail "stages ci.yml's unversioned asset name"
fi

# Re-staging over an existing asset overwrites rather than failing: both
# workflows run on runners that may reuse a workspace.
if "$package" "$fake_exe" "$ci_asset" > /dev/null; then
  pass "re-staging over an existing asset succeeds"
else
  fail "re-staging over an existing asset succeeds"
fi

# -- rejections ----------------------------------------------------------
if "$package" "$fake_exe" "ShinraMeter-BPSR-v0.2.5-windows-x64.zip" > /dev/null 2>&1; then
  fail "rejects an asset name that is not a .exe"
else
  pass "rejects an asset name that is not a .exe"
fi

if "$package" "$fake_exe" "dist/ShinraMeter-BPSR.exe" > /dev/null 2>&1; then
  fail "rejects an asset name containing a path separator"
else
  pass "rejects an asset name containing a path separator"
fi

if "$package" "$work/does-not-exist.exe" "ShinraMeter-BPSR.exe" > /dev/null 2>&1; then
  fail "rejects a missing build output"
else
  pass "rejects a missing build output"
fi

if "$package" "$fake_exe" > /dev/null 2>&1; then
  fail "rejects a missing argument"
else
  pass "rejects a missing argument"
fi

if [[ $failures -ne 0 ]]; then
  echo "$failures package-release.sh test(s) failed" >&2
  exit 1
fi
echo "all package-release.sh tests passed"
