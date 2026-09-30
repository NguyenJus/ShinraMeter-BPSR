#!/usr/bin/env bash
# Shared release/CI assertions. Capture complete tool output before searching:
# grep -q in a pipe can SIGPIPE the producer and invert a check under pipefail.
set -euo pipefail

if [[ $# -ne 1 || ! -f "$1" ]]; then
  echo "usage: $0 <windows-exe-path>" >&2
  exit 1
fi

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# Tool failures must reject the artifact, even if partial output looks valid.
x86_64-w64-mingw32-objdump -p "$1" > "$scratch/imports"
strings -a "$1" > "$scratch/strings"
if grep -qi 'DLL Name: WinDivert' "$scratch/imports"; then
  echo "::error::WinDivert is linked at load time; the exe needs WinDivert.dll beside it" >&2
  exit 1
fi
if ! grep -q 'requireAdministrator' "$scratch/strings"; then
  echo "::error::the UAC manifest is missing from the executable" >&2
  exit 1
fi
