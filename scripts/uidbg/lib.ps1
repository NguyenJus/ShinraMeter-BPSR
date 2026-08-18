# Shared helpers for the WSL -> Windows interactive UI debugging harness (issue
# #88). Dot-source this from any job you submit through `scripts/ctl.sh`:
#
#     . $env:SHINRA_UIDBG_LIB
#
# (`ctl-server.ps1` sets `SHINRA_UIDBG_LIB` for every job it runs, and
# `ctl.sh` prepends that dot-source line unless you pass `--raw`.)
#
# WHY THIS EXISTS
# ---------------
# `crates/app/ShinraMeter-BPSR.manifest` requests `requireAdministrator`, so the
# overlay always runs at High integrity. A PowerShell spawned from WSL runs at
# Medium, and UIPI silently refuses cross-integrity input/window manipulation:
# `MoveWindow`, `SetForegroundWindow` and `SendInput` come back false with
# `ERROR_ACCESS_DENIED` (5). Screen capture is *not* filtered by UIPI, so an
# unelevated shell can watch the window but cannot drive it. That asymmetry is
# the entire reason for the elevated control channel in `ctl-server.ps1`; the
# `Invoke-UidbgWin32` wrapper below turns "returned false" into an explicit
# "error 5 (ERROR_ACCESS_DENIED) -- run this through the elevated ctl server"
# diagnostic instead of a silent no-op.

# Deliberately no `Set-StrictMode` here: this file is dot-sourced into the
# long-lived elevated host and into every ad-hoc job, and tightening the mode
# globally would change the semantics of jobs written interactively at 2am.
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

# Where this file lives, captured NOW rather than read from $PSScriptRoot
# inside the functions below. Dot-sourcing runs this file in the *caller's*
# scope, and the engine restores $PSScriptRoot to the caller's own value once
# the dot-source returns -- so a function that reads $PSScriptRoot at call time
# would resolve the job script's directory (or $null at the console) instead of
# scripts/uidbg/. Capturing it at load time is the only reading that is correct.
$script:UidbgScriptDir =
    if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }

# The process name of the overlay, without the .exe. Used for every
# `Get-Process` lookup below.
$script:UidbgProcessName = 'ShinraMeter-BPSR'

# Unambiguous window title stamped on the backdrop form. Cleanup matches on
# this (and on the pidfile), never on a command line -- see Stop-Backdrop.
$script:UidbgBackdropTitle = 'SHINRA-UIDBG-BACKDROP'

# ---------------------------------------------------------------------------
# Native interop
# ---------------------------------------------------------------------------
#
# GOTCHA (issue #88): the elevated ctl server is a single long-lived PowerShell
# host, and `Add-Type` caches a compiled type for the lifetime of that host.
# Editing this file mid-session therefore has NO effect on the P/Invoke class
# until the class *name* changes -- the `PSTypeName` guard below finds the stale
# type and skips recompiling, and you spend twenty minutes debugging a fix that
# was never loaded. If you change the class body, bump the version suffix
# (V1 -> V2) here and in every reference. The name is deliberately long and
# project-specific so it cannot collide with a type some other script in the
# same host already registered.
$script:UidbgNative = 'ShinraUidbgNativeV1'

if (-not ([System.Management.Automation.PSTypeName]$script:UidbgNative).Type) {
    Add-Type @"
using System;
using System.Runtime.InteropServices;

public class ShinraUidbgNativeV1 {
  [DllImport("user32.dll", SetLastError=true)] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool GetLayeredWindowAttributes(IntPtr h, out uint key, out byte alpha, out uint flags);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool RedrawWindow(IntPtr h, IntPtr rect, IntPtr rgn, uint flags);
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int index);

  // GOTCHA (issue #88): `DwmGetWindowAttribute` is one native entry point with
  // two shapes we care about -- `out RECT` for DWMWA_EXTENDED_FRAME_BOUNDS (9)
  // and `out int` for DWMWA_CLOAKED (14). If both are declared under the same
  // managed name, PowerShell's overload resolution picks by the *runtime* type
  // of the [ref] argument and gets it wrong often enough to produce garbage
  // rects and bogus HRESULTs. Declaring them under distinct managed names with
  // an explicit `EntryPoint` removes the overload set entirely, so the call
  // site chooses the marshalling instead of the binder guessing.
  [DllImport("dwmapi.dll", EntryPoint="DwmGetWindowAttribute")] public static extern int DwmGetRect(IntPtr h, int attr, out RECT r, int size);
  [DllImport("dwmapi.dll", EntryPoint="DwmGetWindowAttribute")] public static extern int DwmGetInt(IntPtr h, int attr, out int v, int size);

  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
}

# DWM window attribute ids we use.
$script:DWMWA_EXTENDED_FRAME_BOUNDS = 9
$script:DWMWA_CLOAKED = 14

# ---------------------------------------------------------------------------
# Configuration: working directory, repo root, exe location
# ---------------------------------------------------------------------------

function Get-UidbgRoot {
    <#
      Windows-side scratch directory for the harness: job inbox/outbox, deployed
      exe, screenshots, pidfiles. Never hardcode a personal C:\temp path -- the
      default is only a default. Override with SHINRA_UIDBG_DIR (and set the
      matching value on the WSL side for `scripts/ctl.sh`).
    #>
    if ($env:SHINRA_UIDBG_DIR) { return $env:SHINRA_UIDBG_DIR }
    return 'C:\temp\shinra-uidbg'
}

function Get-UidbgRepoRoot {
    <#
      Repo root, so the harness can find a freshly cross-compiled exe without
      anyone editing a path into a script. This file lives at
      <repo>/scripts/uidbg/lib.ps1, so two Split-Paths gets there.

      When the ctl server is started against the repo copy of these scripts
      (via the \\wsl.localhost\<distro>\... path that `ctl.sh --serve-cmd`
      prints) this resolves to the WSL worktree over UNC and Copy-Item reads
      straight out of it. If you instead copied the scripts somewhere else, set
      SHINRA_UIDBG_REPO to the Windows-visible repo root.

      `scripts/ctl.sh` sets SHINRA_UIDBG_REPO on every job it submits, to ITS
      OWN repo root -- see the comment on WIN_REPO_ROOT there. This matters
      because the ctl server is long-lived and may have been started from a
      different worktree than the one calling ctl.sh right now: without this,
      a deploy would silently build from the server's birthplace tree instead
      of the caller's. Running one of these scripts directly (not through
      ctl.sh) leaves the env var unset and hits the two-Split-Paths fallback
      above, same as always.
    #>
    if ($env:SHINRA_UIDBG_REPO) { return $env:SHINRA_UIDBG_REPO }
    return (Split-Path -Parent (Split-Path -Parent $script:UidbgScriptDir))
}

function Get-UidbgBuiltExe {
    <#
      Locate the cross-compiled overlay under the repo's Windows target dir.
      `-BuildProfile auto` (the default) picks whichever of release/debug was
      built most recently, which is almost always the one you just built.

      Note the parameter is `-BuildProfile`, not `-Profile`: `$Profile` is an
      automatic variable in PowerShell and binding a parameter over it is a
      good way to break the host.
    #>
    param([ValidateSet('auto', 'debug', 'release')][string]$BuildProfile = 'auto')

    $targetDir = Join-Path (Get-UidbgRepoRoot) 'target\x86_64-pc-windows-gnu'
    $wanted = if ($BuildProfile -eq 'auto') { @('release', 'debug') } else { @($BuildProfile) }

    $found = @()
    foreach ($p in $wanted) {
        $candidate = Join-Path $targetDir "$p\$script:UidbgProcessName.exe"
        if (Test-Path -LiteralPath $candidate) { $found += (Get-Item -LiteralPath $candidate) }
    }
    if ($found.Count -eq 0) {
        throw ("no $script:UidbgProcessName.exe under $targetDir " +
               "(cargo build --target x86_64-pc-windows-gnu first; " +
               'set SHINRA_UIDBG_REPO if the repo root is not two levels above this script)')
    }
    return ($found | Sort-Object LastWriteTime -Descending | Select-Object -First 1).FullName
}

function Get-UidbgDeployedExe {
    # The copy the harness actually launches, inside the working dir. Running
    # the deployed copy rather than the build output means a rebuild in WSL
    # cannot swap the binary out from under a running process.
    return (Join-Path (Get-UidbgRoot) "app\$script:UidbgProcessName.exe")
}

# ---------------------------------------------------------------------------
# Win32 call wrapper with an ERROR_ACCESS_DENIED diagnostic
# ---------------------------------------------------------------------------

# Result of the most recent Invoke-UidbgWin32 call. The wrapper writes its
# diagnostic straight to the output stream (that is the only stream the ctl
# server captures back to WSL) and reports success out-of-band here, so callers
# never have to separate a stray boolean out of their own output.
$script:UidbgLastCallOk = $true

function Invoke-UidbgWin32 {
    <#
      Run a BOOL-returning Win32 call and translate a failure into a useful
      line of output. The case that matters is 5 / ERROR_ACCESS_DENIED: that is
      UIPI refusing a Medium-integrity caller against the High-integrity
      overlay, and it is indistinguishable from "nothing happened" unless you
      look at GetLastError. Every window-manipulation call in this file goes
      through here so that failure mode can never be silent again.

      Success/failure is reported via $script:UidbgLastCallOk, not a return
      value -- see the comment on that variable.

      A false return with GetLastError() == 0 is NOT reported. Several user32
      calls return false as a normal outcome without setting an error code, and
      SetForegroundWindow is the one that bit this harness: Windows refuses a
      foreground change from a process that does not currently hold
      foreground-eligibility, returns false, and sets no error. Printing that
      as "ERR ... win32 error 0" alongside a perfectly good screenshot is worse
      than useless -- it teaches the reader to skim past ERR lines, which is
      exactly the habit that makes a real error 5 invisible. So: diagnose only a
      genuinely non-zero last error.

      -Soft marks a call whose failure is not fatal to the operation in
      progress (again, SetForegroundWindow: capture works regardless of who has
      focus). Soft failures are reported as NOTE rather than ERR -- except
      error 5, which always gets the full integrity-level explanation because it
      means the caller is unelevated and every *other* control call in the job
      is about to fail too.
    #>
    param(
        [Parameter(Mandatory = $true)][string]$What,
        [Parameter(Mandatory = $true)][scriptblock]$Call,
        [switch]$Soft
    )
    $succeeded = & $Call
    # Read the thread-last-error immediately: any intervening pipeline activity
    # can clobber it.
    $err = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
    $script:UidbgLastCallOk = [bool]$succeeded
    if ($succeeded -or $err -eq 0) { return }

    if ($err -eq 5) {
        Write-Output ("ERR $What failed: 5 ERROR_ACCESS_DENIED -- UIPI blocked a " +
                      'cross-integrity call. The overlay manifest requests ' +
                      'requireAdministrator; submit this job through the elevated ' +
                      'ctl server (scripts/ctl.sh) instead of running it directly.')
    } else {
        $severity = if ($Soft) { 'NOTE' } else { 'ERR' }
        Write-Output ("$severity $What failed: win32 error $err")
    }
}

# ---------------------------------------------------------------------------
# Window lookup and geometry
# ---------------------------------------------------------------------------

function Get-AppProcess {
    return (Get-Process -Name $script:UidbgProcessName -ErrorAction SilentlyContinue |
            Where-Object { $_.MainWindowHandle -ne 0 } |
            Select-Object -First 1)
}

function Get-AppHwnd {
    $p = Get-AppProcess
    if (-not $p) { return [IntPtr]::Zero }
    return $p.MainWindowHandle
}

function Get-AppRect {
    <#
      DWMWA_EXTENDED_FRAME_BOUNDS (9) rather than GetWindowRect: on composited
      Windows the window rect includes the invisible resize border (typically
      7px per side at 100% DPI), so a GetWindowRect-based capture is offset and
      padded with desktop pixels, which wrecks pixel probes near an edge. Fall
      back to GetWindowRect if DWM says no (HRESULT != 0), which happens for
      windows that have never been composited.
    #>
    param([Parameter(Mandatory = $true)][IntPtr]$Hwnd)

    $r = New-Object "$script:UidbgNative+RECT"
    if ([ShinraUidbgNativeV1]::DwmGetRect($Hwnd, $script:DWMWA_EXTENDED_FRAME_BOUNDS, [ref]$r, 16) -ne 0) {
        [void][ShinraUidbgNativeV1]::GetWindowRect($Hwnd, [ref]$r)
    }
    return $r
}

function Format-UidbgRect {
    param([Parameter(Mandatory = $true)]$Rect)
    return ('{0},{1} {2}x{3}' -f $Rect.Left, $Rect.Top, ($Rect.Right - $Rect.Left), ($Rect.Bottom - $Rect.Top))
}

function Get-AppScreenBitmap {
    <#
      Grab the on-screen pixels covered by the window. This is a *screen*
      capture, not a window capture: it deliberately reads whatever the
      compositor actually put on the display, which is the only way to observe
      translucency (a PrintWindow-style capture would give you the app's own
      un-composited surface and hide the bug you are hunting).

      -Pad widens the grab by N pixels on each side, clamped to the virtual
      screen so a window near an edge -- or on a monitor at negative virtual
      coordinates, which is normal for a second display placed left of or above
      the primary -- still produces a valid bitmap.
    #>
    param([int]$Pad = 0)

    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) { return $null }
    $r = Get-AppRect -Hwnd $h

    $vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $x = [Math]::Max($vs.X, $r.Left - $Pad)
    $y = [Math]::Max($vs.Y, $r.Top - $Pad)
    $w = [Math]::Min($vs.Right, $r.Right + $Pad) - $x
    $ht = [Math]::Min($vs.Bottom, $r.Bottom + $Pad) - $y
    if ($w -le 0 -or $ht -le 0) { return $null }

    $bmp = New-Object System.Drawing.Bitmap $w, $ht
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try { $g.CopyFromScreen($x, $y, 0, 0, $bmp.Size) } finally { $g.Dispose() }

    return [pscustomobject]@{
        Bitmap  = $bmp
        Rect    = $r
        OriginX = $x
        OriginY = $y
        Width   = $w
        Height  = $ht
    }
}

# ---------------------------------------------------------------------------
# Probes
# ---------------------------------------------------------------------------

function Probe {
    <#
      Print a compact hex-RGB grid sampled from inside the window.

      This is the workhorse: it makes UI state *measurable* instead of
      eyeballed, and one probe costs ~50 tokens where reading a screenshot
      costs thousands. See docs/ui-debugging.md for how to turn the hex values
      back into (colour, alpha, background) triples.

      GOTCHA (issue #88): PowerShell variable names are case-INsensitive, so a
      loop-local `$rows` is the *same variable* as the `[int]$Rows` parameter.
      Assigning an array to it throws the memorable-for-the-wrong-reasons
      `Cannot convert the "System.Object[]" value ... to type "System.Int32"`
      from a line that never mentions $Rows. Every local in here is therefore
      named so it cannot alias a parameter: $lines, $cells, $rowIndex, $colIndex.
    #>
    param(
        [string]$Label = 'probe',
        [int]$Cols = 4,
        [int]$Rows = 5,
        [string]$Save = '',
        [int]$Pad = 0
    )

    $cap = Get-AppScreenBitmap -Pad $Pad
    if (-not $cap) { Write-Output "$Label ERR no-window-or-bad-rect"; return }

    try {
        $lines = @()
        for ($rowIndex = 0; $rowIndex -lt $Rows; $rowIndex++) {
            # Sample cell centres, not edges: a sample exactly on a boundary is
            # at the mercy of rounding and antialiasing.
            $sampleY = [int](($rowIndex + 0.5) * $cap.Height / $Rows)
            $cells = @()
            for ($colIndex = 0; $colIndex -lt $Cols; $colIndex++) {
                $sampleX = [int](($colIndex + 0.5) * $cap.Width / $Cols)
                $c = $cap.Bitmap.GetPixel($sampleX, $sampleY)
                $cells += ('{0:x2}{1:x2}{2:x2}' -f $c.R, $c.G, $c.B)
            }
            $lines += ($cells -join ' ')
        }
        if ($Save -ne '') { Save-UidbgBitmap -Bitmap $cap.Bitmap -Path $Save }
        Write-Output ('{0} [{1}] {2}' -f $Label, (Format-UidbgRect $cap.Rect), ($lines -join ' | '))
    } finally {
        $cap.Bitmap.Dispose()
    }
}

function Scan-Line {
    <#
      Run-length scan along one row or column of the captured window and print
      the colour runs. This is the boundary finder: instead of guessing where a
      panel edge or a stray opaque rect starts, you get
      `0..7 #1e1e22 | 8..219 #454548` and read the exact pixel off it.

      -Axis x scans a vertical line (fixed x, varying y); -Axis y scans a
      horizontal line (fixed y, varying x). -At is in window-relative pixels,
      and accepts a negative value to index from the far edge.

      DEFAULTS ARE DELIBERATELY LOSSY (issue #88). An exact byte-equality scan
      across a real UI line returns ~350 runs: antialiased text edges, dithered
      gradients and 1-LSB compositing noise each start a new run, and the result
      is a wall of one-pixel entries that is strictly worse than a screenshot.
      Two knobs tame that, and both are tuned so the *default* invocation is
      legible:

        -Tolerance (default 6) treats a pixel as part of the current run while
          every channel stays within N of the run's ANCHOR colour (the first
          pixel of the run, not the previous pixel -- comparing against the
          previous pixel lets a gradient drift arbitrarily far while never
          tripping the threshold). 6 swallows antialiasing and dither but is
          well below the ~39-per-channel step between the panel fill and the
          window background, so real boundaries still resolve.

        -MinRun (default 4) absorbs runs shorter than N pixels into the
          preceding run rather than DELETING them, which is what an earlier
          version did. Deletion punches holes in the coordinate map and makes
          the output lie about where a boundary is; absorption keeps the runs
          contiguous over 0..span-1 so the printed offsets can be trusted.

      For pixel-exact work pass -Tolerance 0 -MinRun 1 and accept the volume.

      -MaxRuns (default 40) is the backstop token guard for a scan that is
      pathological even after the above; output is truncated with a count.
    #>
    param(
        [string]$Label = 'scan',
        [ValidateSet('x', 'y')][string]$Axis = 'y',
        [int]$At = 0,
        [int]$Tolerance = 6,
        [int]$MinRun = 4,
        [int]$MaxRuns = 40,
        [int]$Pad = 0
    )

    $cap = Get-AppScreenBitmap -Pad $Pad
    if (-not $cap) { Write-Output "$Label ERR no-window-or-bad-rect"; return }

    try {
        # Length of the scan and the fixed coordinate, clamped into the bitmap.
        $span = if ($Axis -eq 'y') { $cap.Width } else { $cap.Height }
        $across = if ($Axis -eq 'y') { $cap.Height } else { $cap.Width }
        $fixed = if ($At -lt 0) { $across + $At } else { $At }
        $fixed = [Math]::Max(0, [Math]::Min($across - 1, $fixed))

        # Pass 1: split into runs, each anchored on its first pixel's colour.
        $rawRuns = @()
        $anchorR = -1; $anchorG = -1; $anchorB = -1
        $runStart = 0
        for ($i = 0; $i -lt $span; $i++) {
            $c = if ($Axis -eq 'y') { $cap.Bitmap.GetPixel($i, $fixed) } else { $cap.Bitmap.GetPixel($fixed, $i) }
            if ($anchorR -lt 0) {
                $anchorR = $c.R; $anchorG = $c.G; $anchorB = $c.B; $runStart = $i
                continue
            }
            $delta = [Math]::Max([Math]::Abs($c.R - $anchorR),
                     [Math]::Max([Math]::Abs($c.G - $anchorG), [Math]::Abs($c.B - $anchorB)))
            if ($delta -gt $Tolerance) {
                $rawRuns += [pscustomobject]@{ Start = $runStart; End = $i - 1; R = $anchorR; G = $anchorG; B = $anchorB }
                $anchorR = $c.R; $anchorG = $c.G; $anchorB = $c.B; $runStart = $i
            }
        }
        if ($anchorR -ge 0) {
            $rawRuns += [pscustomobject]@{ Start = $runStart; End = $span - 1; R = $anchorR; G = $anchorG; B = $anchorB }
        }

        # Pass 2: fold each run into its predecessor when either
        #
        #   (a) it is shorter than MinRun -- absorbed, not DELETED as an earlier
        #       version did: deletion punches holes in the coordinate map and
        #       makes the printed offsets lie about where a boundary is; or
        #   (b) its colour is within Tolerance of the predecessor's anchor.
        #
        # (b) is what makes a scan across text legible. A glyph's antialiased
        # edge is a short run that rule (a) absorbs, but the body of background
        # that follows it re-anchors and would otherwise be emitted as a
        # separate entry with the SAME colour -- producing
        # "41..101 0c0c0c | 102..135 0c0c0c | 136..173 0c0c0c" across a line of
        # text instead of one run. Comparison is against the predecessor's fixed
        # anchor, never the running value, so a gradient cannot drift across an
        # unbounded distance one tolerance-step at a time.
        #
        # Folding only ever grows a run, so it cannot create a new short one and
        # a single left-to-right pass is sufficient. A short run at offset 0 is
        # kept: at the very edge of a window a 1px border is usually the thing
        # being measured, not noise.
        $mergedRuns = @()
        foreach ($run in $rawRuns) {
            $isShort = ($run.End - $run.Start + 1) -lt $MinRun
            $isSimilar = $false
            if ($mergedRuns.Count -gt 0) {
                $prev = $mergedRuns[-1]
                $prevDelta = [Math]::Max([Math]::Abs($run.R - $prev.R),
                             [Math]::Max([Math]::Abs($run.G - $prev.G), [Math]::Abs($run.B - $prev.B)))
                $isSimilar = $prevDelta -le $Tolerance
            }
            if (($isShort -or $isSimilar) -and $mergedRuns.Count -gt 0) {
                $mergedRuns[-1].End = $run.End
            } else {
                $mergedRuns += $run
            }
        }

        $totalRuns = $mergedRuns.Count
        $shown = if ($MaxRuns -gt 0 -and $totalRuns -gt $MaxRuns) { $mergedRuns[0..($MaxRuns - 1)] } else { $mergedRuns }
        $suffix = if ($totalRuns -gt $shown.Count) {
            (' ... +{0} more runs (raise -Tolerance/-MinRun or narrow the scan)' -f ($totalRuns - $shown.Count))
        } else { '' }

        $cells = @()
        foreach ($run in $shown) {
            $cells += ('{0}..{1} {2:x2}{3:x2}{4:x2}' -f $run.Start, $run.End, $run.R, $run.G, $run.B)
        }
        Write-Output ('{0} [{1}] {2}={3} tol={4} minrun={5} runs={6} {7}{8}' -f
            $Label, (Format-UidbgRect $cap.Rect), $Axis, $fixed, $Tolerance, $MinRun,
            $totalRuns, ($cells -join ' | '), $suffix)
    } finally {
        $cap.Bitmap.Dispose()
    }
}

function Show-Styles {
    <#
      Dump the window's style bits, layered-window attributes and DWM cloak
      state. Useful when the window is invisible or opaque for a structural
      reason (WS_EX_LAYERED alpha, DWM cloaking during startup) rather than a
      painting reason.
    #>
    param([string]$Label = 'styles')

    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) { Write-Output "$Label ERR no-window"; return }

    $key = 0; $alpha = 0; $flags = 0
    $layered = [ShinraUidbgNativeV1]::GetLayeredWindowAttributes($h, [ref]$key, [ref]$alpha, [ref]$flags)
    $cloak = 0
    [void][ShinraUidbgNativeV1]::DwmGetInt($h, $script:DWMWA_CLOAKED, [ref]$cloak, 4)
    # -16 = GWL_STYLE, -20 = GWL_EXSTYLE.
    Write-Output ('{0} hwnd=0x{1:x} style=0x{2:x} ex=0x{3:x} layeredAttrs={4}(key=0x{5:x},alpha={6},flags={7}) cloaked={8} visible={9}' -f
        $Label, [int64]$h,
        [ShinraUidbgNativeV1]::GetWindowLong($h, -16),
        [ShinraUidbgNativeV1]::GetWindowLong($h, -20),
        $layered, $key, $alpha, $flags, $cloak,
        [ShinraUidbgNativeV1]::IsWindowVisible($h))
}

function Save-UidbgBitmap {
    param(
        [Parameter(Mandatory = $true)][System.Drawing.Bitmap]$Bitmap,
        [Parameter(Mandatory = $true)][string]$Path
    )
    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    $Bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
}

function Save-AppShot {
    <#
      Save a PNG of the window (plus -Pad pixels of surrounding desktop, which
      is how you check that a translucent edge blends with what is behind it).
      Write it somewhere under the working dir so the WSL side can read it back
      through /mnt/c.
    #>
    param(
        [string]$Path = '',
        [int]$Pad = 40,
        [switch]$Focus
    )

    if ($Path -eq '') { $Path = Join-Path (Get-UidbgRoot) 'shots\shot.png' }
    if ($Focus) { Set-AppFocus }

    $cap = Get-AppScreenBitmap -Pad $Pad
    if (-not $cap) { Write-Output 'ERR no-window-or-bad-rect'; return }
    try {
        Save-UidbgBitmap -Bitmap $cap.Bitmap -Path $Path
        Write-Output ('OK shot={0} rect=[{1}] captured={2}x{3} origin={4},{5}' -f
            $Path, (Format-UidbgRect $cap.Rect), $cap.Width, $cap.Height, $cap.OriginX, $cap.OriginY)
    } finally {
        $cap.Bitmap.Dispose()
    }
}

# ---------------------------------------------------------------------------
# Window control (needs High integrity -- see the header)
# ---------------------------------------------------------------------------

function Set-AppFocus {
    # Emits only diagnostics (never a bare boolean) so it can be called inline
    # from other output-producing functions.
    #
    # -Soft: a refused foreground change is routine (Windows only grants
    # foreground to a process that already has it, or that the user just
    # interacted with) and it does not affect capture at all, since screen
    # capture reads the composited desktop rather than the active window. It
    # must not print an ERR line above an otherwise successful screenshot.
    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) { Write-Output 'ERR no-window'; return }
    Invoke-UidbgWin32 -Soft -What 'SetForegroundWindow' -Call { [ShinraUidbgNativeV1]::SetForegroundWindow($h) }
    # Compositing settles a frame or two after activation; probing immediately
    # can catch the old surface.
    Start-Sleep -Milliseconds 350
}

function Set-AppRect {
    <#
      Move and/or resize the window, then RE-READ the rect and report both what
      was asked for and what actually stuck.

      GOTCHA (issue #88): the overlay is not a passive target. Its snap-blocking
      and auto-fit logic (ui.rs) can veto or immediately revert an external
      MoveWindow, and `MIN_INNER_SIZE` (220x90) clamps any shrink test below it.
      A harness that assumes its resize took effect will happily report a
      "reproduced" bug at a size the window never actually had. Always compare
      requested vs actual -- this function prints both, and says MISMATCH when
      they differ so the difference cannot be skimmed past.
    #>
    param(
        [int]$X = [int]::MinValue,
        [int]$Y = [int]::MinValue,
        [int]$W = 0,
        [int]$H = 0,
        [int]$SettleMs = 500
    )

    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) { Write-Output 'ERR no-window'; return }
    $before = Get-AppRect -Hwnd $h

    $nx = if ($X -eq [int]::MinValue) { $before.Left } else { $X }
    $ny = if ($Y -eq [int]::MinValue) { $before.Top } else { $Y }
    $nw = if ($W -gt 0) { $W } else { $before.Right - $before.Left }
    $nh = if ($H -gt 0) { $H } else { $before.Bottom - $before.Top }

    Invoke-UidbgWin32 -What 'MoveWindow' -Call {
        [ShinraUidbgNativeV1]::MoveWindow($h, $nx, $ny, $nw, $nh, $true)
    }
    $moveOk = $script:UidbgLastCallOk
    Start-Sleep -Milliseconds $SettleMs

    $after = Get-AppRect -Hwnd $h
    $actualW = $after.Right - $after.Left
    $actualH = $after.Bottom - $after.Top
    $verdict = if ($after.Left -eq $nx -and $after.Top -eq $ny -and $actualW -eq $nw -and $actualH -eq $nh) {
        'OK'
    } else {
        'MISMATCH(app auto-fit/min-size/snap logic overrode the request)'
    }
    Write-Output ('{0} moveWindow={1} requested={2},{3} {4}x{5} before=[{6}] actual=[{7}]' -f
        $verdict, $moveOk, $nx, $ny, $nw, $nh, (Format-UidbgRect $before), (Format-UidbgRect $after))
}

function Invoke-AppRedraw {
    # RDW_INVALIDATE|RDW_ERASE|RDW_FRAME|RDW_ALLCHILDREN|RDW_UPDATENOW
    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) { Write-Output 'ERR no-window'; return }
    Invoke-UidbgWin32 -What 'RedrawWindow' -Call {
        [ShinraUidbgNativeV1]::RedrawWindow($h, [IntPtr]::Zero, [IntPtr]::Zero, 0x0507)
    }
    if ($script:UidbgLastCallOk) { Write-Output 'OK redraw' }
}

# ---------------------------------------------------------------------------
# Process lifecycle
# ---------------------------------------------------------------------------

function Stop-App {
    param([int]$TimeoutMs = 5000)

    $procs = @(Get-Process -Name $script:UidbgProcessName -ErrorAction SilentlyContinue)
    if ($procs.Count -eq 0) { return $false }
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (-not (Get-Process -Name $script:UidbgProcessName -ErrorAction SilentlyContinue)) { return $true }
        Start-Sleep -Milliseconds 100
    }
    return $true
}

function Copy-AppAssets {
    <#
      Mirror crates/app/assets next to the deployed exe (issue #107 fallout).

      Since PR #107 the app loads class/Imagine icons from files on disk
      instead of embedding them: `assets::resolve` (crates/app/src/assets.rs)
      tries, in order, SHINRA_ASSETS_DIR, then `<exe dir>/assets`, then the
      crate's own `assets/` (the `cargo run` dev layout). A deploy that copies
      only the exe gives it none of those, so the running app logs "no asset
      root found" and every class/Imagine icon fails to load. Deploying next
      to the exe (candidate 2 above) is the one of the three a *deployed*
      build can actually hit.

      Delete-then-copy rather than an overwrite-in-place Copy-Item -Recurse:
      the latter only adds/replaces files, so an icon removed or renamed
      upstream since the last deploy would silently survive in the deployed
      copy and mask the real (rebuilt) asset set.

      Missing source assets is reported, not thrown -- a deploy should still
      land a working (if icon-less, same as the "asset root not found"
      runtime fallback) exe rather than fail outright over an asset tree that
      e.g. hasn't been generated yet in a fresh checkout.
    #>
    param([Parameter(Mandatory = $true)][string]$DstDir)

    $srcAssets = Join-Path (Get-UidbgRepoRoot) 'crates\app\assets'
    $dstAssets = Join-Path $DstDir 'assets'

    if (-not (Test-Path -LiteralPath $srcAssets)) {
        Write-Output "WARN no assets dir at $srcAssets -- class/Imagine icons will not be deployed"
        return 'assets=MISSING'
    }

    try {
        if (Test-Path -LiteralPath $dstAssets) {
            Remove-Item -LiteralPath $dstAssets -Recurse -Force
        }
        Copy-Item -LiteralPath $srcAssets -Destination $dstAssets -Recurse -Force -ErrorAction Stop
        return "assets=$dstAssets"
    } catch {
        Write-Output "WARN asset copy failed: $($_.Exception.Message)"
        return 'assets=FAILED'
    }
}

function Copy-AppExe {
    <#
      Copy the cross-compiled exe (and its asset tree, see Copy-AppAssets)
      into the working dir.

      GOTCHA (issue #88): Windows holds an exclusive lock on a running image, so
      copying over a live ShinraMeter-BPSR.exe fails with "Permission denied" /
      "being used by another process". The first version of this harness hit
      that on every second iteration. Deploy therefore kills the app first and
      waits for the handle to actually drop -- Stop-Process returns before the
      kernel has finished tearing the process down, so a bare kill-then-copy
      still races. Retry the copy a few times to cover that window.
    #>
    param(
        [ValidateSet('auto', 'debug', 'release')][string]$BuildProfile = 'auto',
        [int]$Retries = 10
    )

    $src = Get-UidbgBuiltExe -BuildProfile $BuildProfile
    $dst = Get-UidbgDeployedExe
    $dstDir = Split-Path -Parent $dst
    if (-not (Test-Path -LiteralPath $dstDir)) { New-Item -ItemType Directory -Force -Path $dstDir | Out-Null }

    $killed = Stop-App

    for ($attempt = 1; $attempt -le $Retries; $attempt++) {
        try {
            Copy-Item -LiteralPath $src -Destination $dst -Force -ErrorAction Stop
            $assetResult = Copy-AppAssets -DstDir $dstDir
            Write-Output ('OK deployed src={0} dst={1} killedRunning={2} attempts={3} {4}' -f $src, $dst, $killed, $attempt, $assetResult)
            return
        } catch {
            if ($attempt -eq $Retries) {
                Write-Output ('ERR deploy failed after {0} attempts: {1}' -f $Retries, $_.Exception.Message)
                return
            }
            Start-Sleep -Milliseconds 250
        }
    }
}

function Start-App {
    <#
      Launch the deployed exe and wait for its main window.

      -EnvVars takes a hashtable of environment overrides applied to this
      PowerShell process just long enough for Start-Process to hand them to the
      child, then restored. The useful ones are RUST_LOG (the app's env_logger
      filter) and the WGPU_* backend overrides -- WGPU_BACKEND=gl / dx12 /
      vulkan and WGPU_POWER_PREF -- for isolating a rendering defect to one
      backend.
    #>
    param(
        [hashtable]$EnvVars = @{},
        [string]$Exe = '',
        [int]$TimeoutMs = 15000
    )

    if ($Exe -eq '') { $Exe = Get-UidbgDeployedExe }
    if (-not (Test-Path -LiteralPath $Exe)) {
        Write-Output ("ERR no deployed exe at $Exe -- run the deploy action first")
        return
    }

    [void](Stop-App)

    # GOTCHA (issue #88): the ctl server is ONE long-lived elevated host that
    # runs every job in the same process, so a bare `Set-Item Env:...` here is
    # not scoped to this launch -- it is a permanent mutation of the server.
    # A single `-RustLog debug` would silently keep RUST_LOG=debug set for every
    # later launch that asked for no logging at all. Save the prior value,
    # restore it in a finally so a throw cannot leak it either. $null from
    # GetEnvironmentVariable means "was not set", and SetEnvironmentVariable
    # with $null removes the variable -- which is distinct from setting it to ''.
    $applied = @()
    $saved = @{}
    foreach ($k in $EnvVars.Keys) {
        $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
        Set-Item -Path "Env:$k" -Value $EnvVars[$k]
        $applied += ('{0}={1}' -f $k, $EnvVars[$k])
    }

    try {
        # The child inherits the environment at creation, so the restore below
        # is safe the instant Start-Process returns.
        Start-Process -FilePath $Exe -WorkingDirectory (Split-Path -Parent $Exe)
    } finally {
        foreach ($k in @($saved.Keys)) {
            [Environment]::SetEnvironmentVariable($k, $saved[$k], 'Process')
        }
    }

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 250
        if ((Get-AppHwnd) -ne [IntPtr]::Zero) { break }
    }
    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) { Write-Output 'ERR window-never-appeared'; return }

    $r = Get-AppRect -Hwnd $h
    Write-Output ('OK launched exe={0} env=[{1}] rect=[{2}]' -f $Exe, ($applied -join ' '), (Format-UidbgRect $r))
}

# ---------------------------------------------------------------------------
# Backdrop
# ---------------------------------------------------------------------------

function Get-UidbgBackdropPidFile {
    return (Join-Path (Get-UidbgRoot) 'backdrop.pid')
}

function Start-Backdrop {
    <#
      Put a solid, saturated window behind the overlay so translucency becomes
      a yes/no observation: correct compositing tints the probe toward the
      backdrop colour, broken compositing does not. Magenta (255,0,255) is the
      default because no colour in the app's palette is anywhere near it, so a
      single channel comparison is conclusive.
    #>
    param(
        [int]$X = 300, [int]$Y = 200, [int]$W = 900, [int]$H = 1000,
        [int]$R = 255, [int]$G = 0, [int]$B = 255
    )

    Stop-Backdrop | Out-Null

    $backdropScript = Join-Path $script:UidbgScriptDir 'backdrop.ps1'
    $pidFile = Get-UidbgBackdropPidFile
    $pidDir = Split-Path -Parent $pidFile
    if (-not (Test-Path -LiteralPath $pidDir)) { New-Item -ItemType Directory -Force -Path $pidDir | Out-Null }
    # NOT named $args: that is a PowerShell automatic variable, and the same
    # case-insensitive-name hazard that bites $Rows in Probe applies to it.
    $childArgs = @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-WindowStyle', 'Hidden',
        '-File', $backdropScript,
        '-X', $X, '-Y', $Y, '-W', $W, '-H', $H,
        '-R', $R, '-G', $G, '-B', $B,
        '-PidFile', $pidFile
    )
    Start-Process -FilePath 'powershell.exe' -ArgumentList $childArgs -WindowStyle Hidden

    # Wait for the child to publish its pid so a probe issued straight after
    # this does not race the window creation.
    $deadline = [DateTime]::UtcNow.AddMilliseconds(8000)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $pidFile) {
            Start-Sleep -Milliseconds 250
            Write-Output ('OK backdrop pid={0} rect={1},{2} {3}x{4} color=#{5:x2}{6:x2}{7:x2}' -f
                (Get-Content -LiteralPath $pidFile -Raw).Trim(), $X, $Y, $W, $H, $R, $G, $B)
            return
        }
        Start-Sleep -Milliseconds 100
    }
    Write-Output 'ERR backdrop never published a pidfile'
}

function Stop-Backdrop {
    <#
      Kill the backdrop -- and ONLY the backdrop.

      GOTCHA (issue #88), the nastiest one in this harness: the first cleanup
      matched candidate processes with `CommandLine -like '*backdrop*'`. The
      elevated ctl server's own command line contains the path to the harness
      scripts, and any job that mentions the backdrop is itself a powershell
      command line containing "backdrop" -- so cleanup matched the *server*,
      killed it mid-job, and the whole channel died silently in the middle of a
      run that looked like it had merely hung. Never match on command line.

      Cleanup now keys on two unambiguous markers the backdrop publishes about
      itself: a pidfile it writes at startup and deletes on exit, and a
      distinctive MainWindowTitle. Both are additionally guarded against $PID
      (this host, i.e. the ctl server) so no matching bug can ever be fatal to
      the channel again.
    #>
    $pidFile = Get-UidbgBackdropPidFile
    $stopped = @()

    if (Test-Path -LiteralPath $pidFile) {
        $raw = (Get-Content -LiteralPath $pidFile -Raw).Trim()
        $backdropPid = 0
        if ([int]::TryParse($raw, [ref]$backdropPid) -and $backdropPid -gt 0 -and $backdropPid -ne $PID) {
            $p = Get-Process -Id $backdropPid -ErrorAction SilentlyContinue
            if ($p) { $p | Stop-Process -Force -ErrorAction SilentlyContinue; $stopped += $backdropPid }
        }
        Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
    }

    # Belt and braces for a backdrop whose pidfile was lost (host crashed, dir
    # wiped): match the window TITLE, which only the backdrop form sets, and
    # still refuse to touch this process.
    $orphans = @(Get-Process -Name 'powershell', 'pwsh' -ErrorAction SilentlyContinue |
                 Where-Object { $_.Id -ne $PID -and $_.MainWindowTitle -eq $script:UidbgBackdropTitle })
    foreach ($o in $orphans) {
        $o | Stop-Process -Force -ErrorAction SilentlyContinue
        $stopped += $o.Id
    }

    if ($stopped.Count -eq 0) { return 'OK no backdrop running' }
    return ('OK backdrop stopped pids={0}' -f ($stopped -join ','))
}

# ---------------------------------------------------------------------------
# Status
# ---------------------------------------------------------------------------

function Show-UidbgStatus {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

    Write-Output ('root={0}' -f (Get-UidbgRoot))
    Write-Output ('repo={0}' -f (Get-UidbgRepoRoot))
    Write-Output ('elevated={0} hostPid={1}' -f $elevated, $PID)
    if (-not $elevated) {
        Write-Output ('WARN not elevated -- capture works, but MoveWindow/SetForegroundWindow/SendInput ' +
                      'against the requireAdministrator overlay will fail with error 5')
    }

    try { Write-Output ('builtExe={0}' -f (Get-UidbgBuiltExe)) } catch { Write-Output ('builtExe=ERR {0}' -f $_.Exception.Message) }
    $deployed = Get-UidbgDeployedExe
    Write-Output ('deployedExe={0} present={1}' -f $deployed, (Test-Path -LiteralPath $deployed))

    $h = Get-AppHwnd
    if ($h -eq [IntPtr]::Zero) {
        Write-Output 'app=not-running'
    } else {
        Write-Output ('app=running rect=[{0}]' -f (Format-UidbgRect (Get-AppRect -Hwnd $h)))
        Show-Styles -Label 'app'
    }

    $pidFile = Get-UidbgBackdropPidFile
    if (Test-Path -LiteralPath $pidFile) {
        Write-Output ('backdrop=pid {0}' -f (Get-Content -LiteralPath $pidFile -Raw).Trim())
    } else {
        Write-Output 'backdrop=off'
    }
}
