#!/usr/bin/env bash
#
# WSL side of the interactive UI debugging harness (issue #88).
#
# Submits a PowerShell job to the elevated ctl server running on Windows, waits
# for it to finish, and prints its output. The server exists because the overlay
# manifest requests `requireAdministrator`: a WSL-spawned shell is Medium
# integrity and UIPI refuses its MoveWindow/SetForegroundWindow/SendInput calls
# against the High-integrity app with ERROR_ACCESS_DENIED (5). Screen capture is
# unaffected, so probes work either way -- window *control* is what needs this.
#
# Usage
#   scripts/ctl.sh --serve-cmd              # print the elevated start command
#   scripts/ctl.sh app -Action status       # run scripts/uidbg/app.ps1 with args
#   scripts/ctl.sh -c 'Probe -Cols 6'       # run an inline snippet
#   scripts/ctl.sh <<'PS'                   # or read the snippet from stdin
#     Probe -Label before -Cols 6 -Rows 8
#     Set-AppRect -W 640 -H 400
#     Probe -Label after  -Cols 6 -Rows 8
#   PS
#
# Options
#   --serve-cmd     print the command to start the elevated server, then exit
#   --wait-ready    block until the server publishes ready.txt, then exit
#   -c <script>     inline PowerShell instead of stdin
#   --raw           do not prepend the lib dot-source (for self-contained jobs)
#   --timeout <s>   seconds to wait for the job (default 120)
#   --keep          keep the job's output files in <root>/out after printing
#
# Environment
#   SHINRA_UIDBG_DIR   WSL path to the Windows-side working dir.
#                      Default /mnt/c/temp/shinra-uidbg, which is the WSL view
#                      of the PowerShell-side default C:\temp\shinra-uidbg.
#                      Override BOTH sides together if you move it.

set -euo pipefail

CTL_DIR="${SHINRA_UIDBG_DIR:-/mnt/c/temp/shinra-uidbg}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UIDBG_DIR="$REPO_ROOT/scripts/uidbg"

TIMEOUT_S=120
INLINE=""
PREPEND_LIB=1
KEEP=0
MODE="job"
declare -a APP_ARGS=()

die() { echo "ctl.sh: $*" >&2; exit 1; }

# Windows-visible form of a WSL path. Everything the server is told about has to
# be expressed this way -- it is a Windows process and cannot read /mnt/c paths.
winpath() { wslpath -w "$1"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --serve-cmd)  MODE="serve-cmd"; shift ;;
    --wait-ready) MODE="wait-ready"; shift ;;
    -c)           INLINE="${2:-}"; shift 2 ;;
    --raw)        PREPEND_LIB=0; shift ;;
    --keep)       KEEP=1; shift ;;
    --timeout)    TIMEOUT_S="${2:-}"; shift 2 ;;
    app)          MODE="app"; shift; APP_ARGS=("$@"); break ;;
    # Print the header comment block as the help text: skip the shebang, strip
    # the leading '# ', and stop at the first line that is not a comment.
    -h|--help)    awk 'NR>1 { if (!/^#/) exit; sub(/^#[[:space:]]?/, ""); print }' "${BASH_SOURCE[0]}"; exit 0 ;;
    *)            die "unknown argument '$1' (see --help)" ;;
  esac
done

# Create the working dir before anything else: `wslpath -w` and the inbox both
# want it to exist, and the server would otherwise create it as a different
# case/shape than the one printed by --serve-cmd.
mkdir -p "$CTL_DIR/in" "$CTL_DIR/out"

if [ "$MODE" = "serve-cmd" ]; then
  cat <<EOF
Start this ONCE per session in an elevated PowerShell on Windows
(Win+X -> "Terminal (Admin)"), and leave the window open:

  powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(winpath "$UIDBG_DIR/ctl-server.ps1")" -Root "$(winpath "$CTL_DIR")"

Then, back here:  scripts/ctl.sh --wait-ready
EOF
  exit 0
fi

if [ "$MODE" = "wait-ready" ]; then
  for _ in $(seq 1 "$((TIMEOUT_S * 4))"); do
    if [ -f "$CTL_DIR/ready.txt" ]; then cat "$CTL_DIR/ready.txt"; exit 0; fi
    sleep 0.25
  done
  die "server never published $CTL_DIR/ready.txt (start it with --serve-cmd)"
fi

if [ ! -f "$CTL_DIR/ready.txt" ]; then
  echo "ctl.sh: warning: no $CTL_DIR/ready.txt -- the elevated server may not be running." >&2
  echo "ctl.sh: run 'scripts/ctl.sh --serve-cmd' for the command to start it." >&2
fi

# Nanosecond id so lexical order == submission order on the server side, and so
# two concurrent submissions cannot collide on a filename.
job_id="job-$(date +%s%N)"
job_file="$CTL_DIR/in/$job_id.ps1"

# Assemble the job in a temp file and move it into the inbox only once it is
# complete: the server polls the inbox and would happily execute a half-written
# script if we wrote in place.
tmp_file="$(mktemp)"
trap 'rm -f "$tmp_file"' EXIT

{
  if [ "$PREPEND_LIB" -eq 1 ]; then
    # The server exports SHINRA_UIDBG_LIB, so a job never hardcodes a path.
    echo '. $env:SHINRA_UIDBG_LIB'
  fi
  if [ "$MODE" = "app" ]; then
    printf '& $env:SHINRA_UIDBG_APP'
    for a in "${APP_ARGS[@]}"; do
      # Single-quote every argument and double any embedded quote: PowerShell's
      # single-quoted strings have no escape sequences, so this is exact.
      printf " '%s'" "${a//\'/\'\'}"
    done
    printf '\n'
  elif [ -n "$INLINE" ]; then
    printf '%s\n' "$INLINE"
  else
    cat
  fi
} > "$tmp_file"

mv "$tmp_file" "$job_file"
trap - EXIT

deadline=$((TIMEOUT_S * 4))
for _ in $(seq 1 "$deadline"); do
  [ -f "$CTL_DIR/out/$job_id.done" ] && break
  sleep 0.25
done

if [ ! -f "$CTL_DIR/out/$job_id.done" ]; then
  echo "ctl.sh: no response for $job_id after ${TIMEOUT_S}s." >&2
  echo "ctl.sh: is the elevated server still running? (it prints each job it runs)" >&2
  echo "ctl.sh: the unrun job is at $job_file" >&2
  exit 1
fi

# The server writes .txt before .done, so by here the output is complete.
if [ -f "$CTL_DIR/out/$job_id.txt" ]; then
  cat "$CTL_DIR/out/$job_id.txt"
fi

if [ "$KEEP" -eq 0 ]; then
  rm -f "$CTL_DIR/out/$job_id.txt" "$CTL_DIR/out/$job_id.done"
fi
