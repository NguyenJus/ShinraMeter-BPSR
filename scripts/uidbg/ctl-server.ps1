# Elevated command channel for the WSL -> Windows UI debugging harness
# (issue #88). Start this ONCE per debugging session, from an elevated
# PowerShell on the Windows side; `scripts/ctl.sh --serve-cmd` prints the exact
# command including the \\wsl.localhost path to this file.
#
# WHY AN ELEVATED CHANNEL AT ALL
# ------------------------------
# `crates/app/ShinraMeter-BPSR.manifest` requests `requireAdministrator`, so the
# overlay runs at High integrity. A shell launched from WSL runs at Medium, and
# UIPI blocks a Medium process from posting input to or repositioning a High
# process: `MoveWindow` / `SetForegroundWindow` / `SendInput` all fail with
# `ERROR_ACCESS_DENIED` (5). Screen capture is not filtered, so an unelevated
# agent can screenshot and probe but cannot drive the window. This server is
# the smallest thing that closes that gap: one UAC prompt at the start of the
# session, after which WSL can submit work that runs at High integrity.
#
# HOW IT WORKS
# ------------
# It watches <root>\in for .ps1 job files, runs each one, writes the combined
# output to <root>\out\<job>.txt, then touches <root>\out\<job>.done as the
# completion signal the WSL-side poller waits on. Executed jobs are moved to
# <root>\done and kept, so the entire elevated surface of a session is
# auditable after the fact rather than being a black box.
#
# Stop it with Ctrl+C or by closing the window.

param(
    # Windows-side working directory. Must match SHINRA_UIDBG_DIR on the WSL
    # side (scripts/ctl.sh derives its own path from the same default).
    [string]$Root = $(if ($env:SHINRA_UIDBG_DIR) { $env:SHINRA_UIDBG_DIR } else { 'C:\temp\shinra-uidbg' }),

    # Windows-visible repo root, only needed if these scripts were copied out of
    # the repo. lib.ps1 otherwise derives it as two levels above itself.
    [string]$RepoRoot = $env:SHINRA_UIDBG_REPO,

    [int]$PollMs = 250
)

$ErrorActionPreference = 'Continue'

$libPath = Join-Path $PSScriptRoot 'lib.ps1'
$appPath = Join-Path $PSScriptRoot 'app.ps1'

$inDir = Join-Path $Root 'in'
$outDir = Join-Path $Root 'out'
$doneDir = Join-Path $Root 'done'
foreach ($d in @($Root, $inDir, $outDir, $doneDir)) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

# Publish configuration through the environment rather than making jobs hardcode
# paths: every job inherits these, and `ctl.sh` prepends `. $env:SHINRA_UIDBG_LIB`
# so a job never needs to know where the repo is mounted.
$env:SHINRA_UIDBG_DIR = $Root
$env:SHINRA_UIDBG_LIB = $libPath
$env:SHINRA_UIDBG_APP = $appPath
if ($RepoRoot) { $env:SHINRA_UIDBG_REPO = $RepoRoot }

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Host "shinra uidbg ctl server up (issue #88)"
Write-Host "  pid      = $PID"
Write-Host "  elevated = $elevated"
Write-Host "  root     = $Root"
Write-Host "  lib      = $libPath"
if (-not $elevated) {
    Write-Host ("WARNING: not elevated. Capture and probes will work, but MoveWindow / " +
                "SetForegroundWindow / SendInput against the requireAdministrator overlay " +
                "will fail with error 5. Restart this in an elevated PowerShell.") -ForegroundColor Yellow
}

# Readiness/handshake file the WSL side can check before submitting work, plus a
# pidfile so a stale server is identifiable. Note this is the SERVER's pid file;
# the backdrop has its own, and cleanup must never confuse the two (issue #88).
Set-Content -LiteralPath (Join-Path $Root 'ready.txt') `
    -Value ("elevated=$elevated pid=$PID root=$Root lib=$libPath")
Set-Content -LiteralPath (Join-Path $Root 'server.pid') -Value $PID -Encoding ascii

try {
    while ($true) {
        # Sort by name: ctl.sh names jobs with a nanosecond timestamp, so
        # lexical order is submission order.
        $jobs = @(Get-ChildItem -LiteralPath $inDir -Filter *.ps1 -ErrorAction SilentlyContinue | Sort-Object Name)
        foreach ($job in $jobs) {
            $name = [IO.Path]::GetFileNameWithoutExtension($job.Name)
            Write-Host ("[{0}] running {1}" -f (Get-Date -Format HH:mm:ss), $job.Name)

            $result = ''
            try {
                # 2>&1 folds the error stream into the captured output: a job
                # that throws should return its stack trace to WSL, not vanish.
                $result = (& $job.FullName 2>&1 | Out-String)
            } catch {
                $result = "ERR " + $_.Exception.Message + "`n" + $_.ScriptStackTrace
            }

            # Move the script out of the inbox BEFORE signalling done, so the
            # inbox can never re-run a job the poller has already collected.
            Move-Item -Force -LiteralPath $job.FullName -Destination (Join-Path $doneDir $job.Name)
            Set-Content -LiteralPath (Join-Path $outDir "$name.txt") -Value $result
            # The .done marker is written last and is the only thing the WSL
            # poller waits on, so it can never read a half-written .txt.
            Set-Content -LiteralPath (Join-Path $outDir "$name.done") -Value '1' -Encoding ascii
        }
        Start-Sleep -Milliseconds $PollMs
    }
} finally {
    Remove-Item -LiteralPath (Join-Path $Root 'ready.txt') -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $Root 'server.pid') -Force -ErrorAction SilentlyContinue
    Write-Host 'shinra uidbg ctl server stopped'
}
