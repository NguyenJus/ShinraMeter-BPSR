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
# auditable after the fact rather than being a black box. The inbox is
# unauthenticated -- see the trust boundary section below, which is why the
# working dir is created with an explicit ACL instead of an inherited one.
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

# ---------------------------------------------------------------------------
# Trust boundary
# ---------------------------------------------------------------------------
# The inbox IS the credential: anything dropped in <root>\in is executed here at
# Administrator integrity, unauthenticated. A directory created under a default
# C:\temp inherits an ACL that lets any local account write to it, which would
# turn this server into an open local privilege-escalation channel. So the whole
# working dir is created with an explicit protected ACL -- inheritance off,
# Administrators / SYSTEM / the invoking user only -- an existing one is
# repaired the same way, and we verify what actually stuck rather than trusting
# the repair. If it is still reachable by anyone else, refuse to serve.

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# Universal well-known SIDs, written out rather than looked up by name: these
# are identical on every Windows install, and name lookups break on localised
# systems where the group is called e.g. "Administratoren".
$trustedSids = @(
    [Security.Principal.SecurityIdentifier]'S-1-5-32-544',  # BUILTIN\Administrators
    [Security.Principal.SecurityIdentifier]'S-1-5-18',      # NT AUTHORITY\SYSTEM
    $identity.User
)

# Rights that let a principal put a job file in the inbox, or re-open the ACL so
# it can. Read access is not interesting here; write access is code execution.
$writeRights = [Security.AccessControl.FileSystemRights]'CreateFiles, CreateDirectories, Delete, DeleteSubdirectoriesAndFiles, ChangePermissions, TakeOwnership'

function New-UidbgLockedAcl {
    # A FRESH descriptor per call: Set-Acl clears the object's modified-section
    # flags when it persists, so reusing one instance silently no-ops on the
    # second and later directories.
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false)  # inheritance off, inherited ACEs dropped
    foreach ($sid in $trustedSids) {
        $rule = New-Object Security.AccessControl.FileSystemAccessRule(
            $sid,
            [Security.AccessControl.FileSystemRights]::FullControl,
            [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit',
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow)
        $acl.AddAccessRule($rule)
    }
    return $acl
}

function Assert-UidbgDirPrivate {
    param([string]$Path, [string]$RepairError = '')

    $acl = Get-Acl -LiteralPath $Path
    $problems = @()

    # An owner keeps WRITE_DAC implicitly, so a directory someone else owns can
    # have the ACL below undone at will and is not safe no matter what it says.
    if (-not ($trustedSids -contains $acl.GetOwner([Security.Principal.SecurityIdentifier]))) {
        $problems += ('owned by ' + $acl.Owner)
    }

    foreach ($rule in $acl.Access) {
        if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow) { continue }
        if (($rule.FileSystemRights -band $writeRights) -eq 0) { continue }
        # An identity that will not resolve to a SID counts as untrusted: this
        # check errs toward refusing rather than toward serving.
        $sid = $null
        try { $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]) } catch { }
        if (-not ($trustedSids -contains $sid)) { $problems += ('writable by ' + $rule.IdentityReference.Value) }
    }

    if ($problems.Count -gt 0) {
        $detail = if ($RepairError) { " (ACL repair failed: $RepairError)" } else { '' }
        throw ("refusing to serve: $Path is " + (($problems | Select-Object -Unique) -join '; ') + $detail +
               ". Every .ps1 dropped in the inbox runs as Administrator, so the working dir must be" +
               " writable only by you. Point -Root (and SHINRA_UIDBG_DIR on the WSL side) at a" +
               " directory only your account can write to, or fix this one's permissions.")
    }
}

# Resolve to an absolute path up front: the job output below is written with
# [IO.File], which resolves relative paths against the process CWD rather than
# PowerShell's current location.
$Root = (New-Item -ItemType Directory -Force -Path $Root).FullName
$inDir = Join-Path $Root 'in'
$outDir = Join-Path $Root 'out'
$doneDir = Join-Path $Root 'done'
foreach ($d in @($Root, $inDir, $outDir, $doneDir)) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    $repairError = ''
    try { Set-Acl -LiteralPath $d -AclObject (New-UidbgLockedAcl) -ErrorAction Stop }
    catch { $repairError = $_.Exception.Message }
    Assert-UidbgDirPrivate -Path $d -RepairError $repairError
}

# Publish configuration through the environment rather than making jobs hardcode
# paths: every job inherits these, and `ctl.sh` prepends `. $env:SHINRA_UIDBG_LIB`
# so a job never needs to know where the repo is mounted.
$env:SHINRA_UIDBG_DIR = $Root
$env:SHINRA_UIDBG_LIB = $libPath
$env:SHINRA_UIDBG_APP = $appPath
if ($RepoRoot) { $env:SHINRA_UIDBG_REPO = $RepoRoot }

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

# $false = emit no byte-order mark; see the job-output write below.
$utf8NoBom = New-Object Text.UTF8Encoding($false)

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
            # BOM-less UTF-8, written with [IO.File] because neither Set-Content
            # default gets there on PS 5.1: the default is ANSI, which mangles
            # non-ASCII in output and stack traces once ctl.sh cats the file into
            # a UTF-8 WSL terminal, and -Encoding utf8 prepends a BOM that would
            # show up as garbage at the head of every job's output.
            [IO.File]::WriteAllText((Join-Path $outDir "$name.txt"), [string]$result, $utf8NoBom)
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
