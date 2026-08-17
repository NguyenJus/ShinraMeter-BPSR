# Consolidated action script for the UI debugging harness (issue #88).
#
# One entry point for everything the harness does to the overlay: deploy the
# cross-compiled exe, launch/kill it, screenshot it, probe its pixels, move or
# resize it, and raise/drop the colour backdrop. All the real work lives in
# lib.ps1; this file is only argument plumbing, so a job can be a single line.
#
# From WSL, through the elevated channel:
#
#     scripts/ctl.sh app -Action status
#     scripts/ctl.sh app -Action deploy
#     scripts/ctl.sh app -Action launch -RustLog debug
#     scripts/ctl.sh app -Action probe -Cols 6 -Rows 8
#     scripts/ctl.sh app -Action scan -Axis y -At 12
#     scripts/ctl.sh app -Action resize -W 640 -H 400
#
# Anything that manipulates the window (launch/resize/move/focus/kill) needs
# High integrity and therefore the elevated ctl server -- see the header of
# lib.ps1. Capture-only actions (shot/probe/scan/styles/status) also work from
# a plain WSL-spawned powershell.exe.

param(
    [ValidateSet(
        'status', 'deploy', 'launch', 'run', 'kill',
        'shot', 'probe', 'scan', 'styles', 'redraw',
        'resize', 'move', 'focus',
        'backdrop-on', 'backdrop-off', 'clean'
    )]
    [string]$Action = 'status',

    # deploy / launch
    [ValidateSet('auto', 'debug', 'release')][string]$BuildProfile = 'auto',
    [string]$RustLog = '',
    [string]$WgpuBackend = '',
    [hashtable]$EnvVars = @{},

    # shot / probe / scan
    [string]$Out = '',
    [int]$Pad = 0,
    [int]$Cols = 4,
    [int]$Rows = 5,
    [ValidateSet('x', 'y')][string]$Axis = 'y',
    [int]$At = 0,
    # Scan defaults live in Scan-Line (lib.ps1) and are tuned for legibility on
    # a real UI line, not for byte-exactness; -Tolerance 0 -MinRun 1 for exact.
    [int]$Tolerance = 6,
    [int]$MinRun = 4,
    [int]$MaxRuns = 40,
    [string]$Label = '',

    # resize / move
    [int]$X = [int]::MinValue,
    [int]$Y = [int]::MinValue,
    [int]$W = 0,
    [int]$H = 0,

    # backdrop-on
    [int]$BackdropX = 300, [int]$BackdropY = 200,
    [int]$BackdropW = 900, [int]$BackdropH = 1000,
    [int]$BackdropR = 255, [int]$BackdropG = 0, [int]$BackdropB = 255
)

$ErrorActionPreference = 'Stop'

# Prefer the library the ctl server published (it is the copy the server's
# Add-Type cache was primed from); fall back to the sibling file when this
# script is run directly from a WSL-spawned powershell.exe.
$libPath = if ($env:SHINRA_UIDBG_LIB) { $env:SHINRA_UIDBG_LIB } else { Join-Path $PSScriptRoot 'lib.ps1' }
. $libPath

# Build the launch environment once: explicit switches win over -EnvVars so a
# one-off `-RustLog trace` is not silently overridden.
function Get-LaunchEnv {
    $merged = @{}
    foreach ($k in $EnvVars.Keys) { $merged[$k] = $EnvVars[$k] }
    if ($RustLog -ne '') { $merged['RUST_LOG'] = $RustLog }
    # WGPU_BACKEND accepts vulkan|dx12|gl; forcing one is how a rendering defect
    # gets attributed to a backend rather than to the app.
    if ($WgpuBackend -ne '') { $merged['WGPU_BACKEND'] = $WgpuBackend }
    return $merged
}

switch ($Action) {

    'status' { Show-UidbgStatus }

    'deploy' { Copy-AppExe -BuildProfile $BuildProfile }

    'launch' { Start-App -EnvVars (Get-LaunchEnv) }

    'run' {
        # The normal edit->observe cycle: redeploy over the (killed) previous
        # binary, then relaunch. Copy-AppExe kills the app first because Windows
        # locks a running image -- see its comment.
        Copy-AppExe -BuildProfile $BuildProfile
        Start-App -EnvVars (Get-LaunchEnv)
    }

    'kill' {
        $wasRunning = Stop-App
        Write-Output ('OK killed wasRunning={0}' -f $wasRunning)
    }

    'shot' {
        $path = if ($Out -ne '') { $Out } else { Join-Path (Get-UidbgRoot) 'shots\shot.png' }
        # Default a bit of padding for screenshots so the desktop just outside
        # the window is visible; a translucent edge is only readable against it.
        $shotPad = if ($PSBoundParameters.ContainsKey('Pad')) { $Pad } else { 40 }
        Save-AppShot -Path $path -Pad $shotPad -Focus
    }

    'probe' {
        $probeLabel = if ($Label -ne '') { $Label } else { 'probe' }
        Probe -Label $probeLabel -Cols $Cols -Rows $Rows -Save $Out -Pad $Pad
    }

    'scan' {
        $scanLabel = if ($Label -ne '') { $Label } else { 'scan' }
        Scan-Line -Label $scanLabel -Axis $Axis -At $At -Tolerance $Tolerance `
            -MinRun $MinRun -MaxRuns $MaxRuns -Pad $Pad
    }

    'styles' { Show-Styles -Label $(if ($Label -ne '') { $Label } else { 'styles' }) }

    'redraw' { Invoke-AppRedraw }

    'resize' { Set-AppRect -W $W -H $H }

    'move' { Set-AppRect -X $X -Y $Y -W $W -H $H }

    'focus' { Set-AppFocus; Write-Output 'OK focus requested' }

    'backdrop-on' {
        Start-Backdrop -X $BackdropX -Y $BackdropY -W $BackdropW -H $BackdropH `
            -R $BackdropR -G $BackdropG -B $BackdropB
    }

    'backdrop-off' { Write-Output (Stop-Backdrop) }

    'clean' {
        # Tear down everything the harness started. Deliberately does NOT touch
        # the ctl server: Stop-Backdrop matches the backdrop's pidfile/title and
        # excludes $PID, precisely so a cleanup action cannot decapitate the
        # channel it is running on (issue #88).
        Write-Output (Stop-Backdrop)
        $wasRunning = Stop-App
        Write-Output ('OK cleaned appWasRunning={0} ctlServerPid={1} (left running)' -f $wasRunning, $PID)
    }
}
