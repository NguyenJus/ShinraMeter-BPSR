# Solid-colour backdrop window for the UI debugging harness (issue #88).
#
# Placed behind the overlay so translucency becomes a measurable yes/no rather
# than a judgement call: if compositing is correct the probe reads the panel
# colour blended toward this colour, and if it is broken (the startup-
# transparency class of bug) the probe reads the panel over plain desktop or
# over opaque white instead. Magenta is the default because nothing in the
# app's palette is near it, so one channel is enough to decide.
#
# Started by Start-Backdrop in lib.ps1; not usually run by hand.

param(
    [int]$X = 300,
    [int]$Y = 200,
    [int]$W = 900,
    [int]$H = 1000,
    [int]$R = 255,
    [int]$G = 0,
    [int]$B = 255,
    [string]$PidFile = ''
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

# IDENTITY MARKERS (issue #88). Cleanup must be able to find exactly this
# process and nothing else. An early version of the harness matched candidates
# with `CommandLine -like '*backdrop*'`, which also matched the elevated ctl
# server (its command line names the harness scripts) and killed the control
# channel mid-job. So the backdrop announces itself two unambiguous ways:
#
#   1. a pidfile it writes here and deletes on exit, and
#   2. a distinctive window title that no other process sets.
#
# Stop-Backdrop keys on those two and nothing else, and refuses to act on its
# own $PID. See the comment on Stop-Backdrop in lib.ps1.
$BackdropTitle = 'SHINRA-UIDBG-BACKDROP'

if ($PidFile -ne '') {
    $pidDir = Split-Path -Parent $PidFile
    if ($pidDir -and -not (Test-Path -LiteralPath $pidDir)) {
        New-Item -ItemType Directory -Force -Path $pidDir | Out-Null
    }
    Set-Content -LiteralPath $PidFile -Value $PID -Encoding ascii
}

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'
$form.StartPosition = 'Manual'
$form.Location = New-Object System.Drawing.Point($X, $Y)
$form.Size = New-Object System.Drawing.Size($W, $H)
$form.BackColor = [System.Drawing.Color]::FromArgb($R, $G, $B)
$form.ShowInTaskbar = $false
# NOT TopMost: the whole point is to sit *behind* the overlay. A topmost
# backdrop would cover the window under test and every probe would read pure
# backdrop colour.
$form.TopMost = $false
$form.Text = $BackdropTitle

try {
    [System.Windows.Forms.Application]::Run($form)
} finally {
    # Best effort: if this process is killed with Stop-Process the finally block
    # never runs, so Stop-Backdrop also removes the pidfile itself.
    if ($PidFile -ne '') { Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue }
}
