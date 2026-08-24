# UI debugging: driving the real Windows window from WSL

Issue #88 committed the harness that this document describes. It exists because
UI defects in `ShinraMeter-BPSR` are not reproducible on the dev box: the app is
cross-compiled in WSL and only ever *renders* on Windows, under a real DWM
compositor, at real DPI, with a real GPU backend. Reasoning about a
transparency or layout bug from source alone stalls; measuring the actual
composited pixels resolves it in one sitting. That is what happened with the
startup-transparency defect the harness was originally built for.

The loop the harness supports is: cross-compile in WSL, deploy the exe to a
Windows-side working directory, launch it, observe it (screenshot, pixel probe,
style dump), change something, repeat — all without leaving the agent loop.

## The elevation problem

`crates/app/ShinraMeter-BPSR.manifest` requests `requireAdministrator`, so the
overlay always runs at **High** integrity. A PowerShell process spawned from a
WSL shell runs at **Medium**, and UIPI (User Interface Privilege Isolation)
refuses cross-integrity window manipulation from below: `MoveWindow`,
`SetForegroundWindow` and `SendInput` all return false with
`ERROR_ACCESS_DENIED` (5).

Screen capture is *not* filtered by UIPI. So an unelevated WSL-spawned shell can
watch the window perfectly well but cannot drive it — a split that is easy to
misdiagnose, because a blocked `MoveWindow` looks exactly like a window that
ignored you. The harness therefore routes everything through a small elevated
control channel, and `Invoke-UidbgWin32` in `scripts/uidbg/lib.ps1` reports
error 5 explicitly rather than letting a call fail silently.

## Setup

Once per debugging session, start the elevated control channel. Ask the harness
for the exact command (it embeds the `\\wsl.localhost\…` path to the repo and
the working directory):

```bash
scripts/ctl.sh --serve-cmd
```

Run the printed command in an **elevated** PowerShell on Windows (Win+X →
"Terminal (Admin)"), accept the single UAC prompt, and leave that window open.
It prints one line per job it runs, which is a useful liveness signal. Then
confirm from WSL:

```bash
scripts/ctl.sh --wait-ready     # prints "elevated=True pid=… root=… lib=…"
```

The channel is a directory watcher, not a service: it polls `<root>\in` for
`.ps1` job files, runs each at High integrity, writes the combined output to
`<root>\out\<job>.txt`, and signals completion with `<job>.done`. Executed jobs
are retained under `<root>\done`, so the entire elevated surface of a session is
auditable after the fact.

### Trust boundary

**The inbox is the credential.** There is no authentication and no origin
check: any `.ps1` that appears in `<root>\in` is executed as Administrator, so
write access to the working directory *is* the ability to run code as
Administrator. Everything else about the channel follows from that.

The server therefore creates `<root>` and its subdirectories with an explicit
**protected** ACL — inheritance disabled, full control granted to
Administrators, SYSTEM and the invoking user, and nobody else — and repairs an
existing directory the same way. It then re-reads the ACL that actually stuck
and refuses to start, with the offending principal named, if the directory is
owned by or writable by anyone outside that set. (An owner keeps the implicit
right to rewrite the ACL, so an untrusted owner is fatal even when the ACL
itself looks correct.)

That check exists because the default `C:\temp\shinra-uidbg` inherits `C:\temp`'s
permissions, which on many machines let any local account write. If you override
`-Root` / `SHINRA_UIDBG_DIR`, pick a path only your account can write to; a
share, a world-writable temp directory, or anything another user can reach turns
the harness into a local privilege-escalation channel. Stop the server when you
are done with the session — it is a debugging tool, not something to leave
running.

### Working directory and exe location

Nothing is hardcoded to one machine's `C:\temp`:

| What | Default | Override |
| --- | --- | --- |
| Windows-side working dir | `C:\temp\shinra-uidbg` | `SHINRA_UIDBG_DIR` (PowerShell), or `-Root` on `ctl-server.ps1` |
| Same dir, as seen from WSL | `/mnt/c/temp/shinra-uidbg` | `SHINRA_UIDBG_DIR` (bash) |
| Repo root | two levels above `scripts/uidbg/lib.ps1` | `SHINRA_UIDBG_REPO` |

The two `SHINRA_UIDBG_DIR` values are the same directory in two path syntaxes;
if you move it, set both.

The exe is *not* configured at all — it is found relative to the repo, under
`target/x86_64-pc-windows-gnu/{release,debug}/ShinraMeter-BPSR.exe`, taking
whichever was built most recently (`-BuildProfile debug|release` to pin one).
The deploy step copies it into `<root>\app\` and the harness launches that copy,
so a rebuild in WSL cannot swap the binary out from under a running process.

## The loop

```bash
cargo build --target x86_64-pc-windows-gnu           # in WSL, as usual
scripts/ctl.sh app -Action run -RustLog debug        # deploy + launch
scripts/ctl.sh app -Action probe -Cols 6 -Rows 8     # measure
scripts/ctl.sh app -Action shot -Out 'C:\temp\shinra-uidbg\shots\a.png'
```

`-Action run` deploys and launches in one step. Deploy kills the app first —
Windows holds an exclusive lock on a running image, so copying over a live
`ShinraMeter-BPSR.exe` fails with "Permission denied".

Actions, all via `scripts/ctl.sh app -Action <name>`:

| Action | What it does |
| --- | --- |
| `status` | working dir, repo, elevation, built/deployed exe, window rect and styles |
| `deploy` / `launch` / `run` / `kill` | lifecycle; `run` = deploy + launch |
| `shot` | PNG of the window plus `-Pad` pixels of surrounding desktop |
| `probe` | hex-RGB grid sampled inside the window |
| `scan` | run-length colour scan along one row/column — the boundary finder |
| `styles` | `GWL_STYLE` / `GWL_EXSTYLE`, layered-window alpha, DWM cloak state |
| `resize` / `move` / `focus` / `redraw` | window control (needs the elevated channel) |
| `backdrop-on` / `backdrop-off` | raise/drop the solid colour backdrop |
| `clean` | stop the backdrop and the app, leaving the channel up |

For anything the actions do not cover, submit PowerShell directly; the library
is already dot-sourced:

```bash
scripts/ctl.sh <<'PS'
Probe -Label before -Cols 6 -Rows 8
Set-AppRect -W 640 -H 400
Probe -Label after  -Cols 6 -Rows 8
PS
```

Launching with an arbitrary environment variable set — e.g. `SHINRA_DEMO=1`
(issue #91) to populate the header with a synthetic encounter instead of "No
target" — doesn't fit `-Action run`'s switches, which only cover named
options like `-RustLog`. Those switches exist for the common cases; arbitrary
env vars go through `run`'s `-EnvVars` hashtable instead, submitted as an
inline PowerShell snippet rather than `scripts/ctl.sh app`:

```bash
scripts/ctl.sh -c '& $env:SHINRA_UIDBG_APP -Action run -EnvVars @{ SHINRA_DEMO = "1" }'
```

## Reading probe output

```
probe [1204,318 520x742] 1e1e22 1e1e22 24242a 1e1e22 | 454548 454548 454548 454548 | …
       └─ rect: x,y w×h ──┘  └── row 0, four samples left→right ──┘  └── row 1 ──┘
```

The bracketed rect is the **DWM extended frame bounds**
(`DwmGetWindowAttribute` with `DWMWA_EXTENDED_FRAME_BOUNDS = 9`), not
`GetWindowRect`. On a composited desktop the window rect includes an invisible
resize border — typically 7px per side at 100% DPI — so a `GetWindowRect`-based
capture is offset and padded with desktop pixels, which is exactly the error
that makes an edge probe read the wrong thing. `GetWindowRect` remains the
fallback for windows DWM does not know about.

Samples are taken at *cell centres* of a `Cols × Rows` grid, so a probe never
lands on a boundary where rounding and antialiasing decide the answer.

`scan` is the tool for finding a boundary exactly:

```
$ scripts/ctl.sh app -Action scan -Axis y -At 12
scan [1204,318 520x742] y=12 tol=6 minrun=4 runs=3 0..6 1e1e22 | 7..512 454548 | 513..519 1e1e22
```

`-Axis y` walks a horizontal line at a fixed `y`; `-Axis x` walks a vertical
line at a fixed `x`. `-At` is window-relative and accepts a negative value to
index from the far edge.

**The defaults are deliberately lossy, and they have to be.** An exact
byte-equality scan across a real UI line returns ~350 runs — every antialiased
glyph edge, dithered gradient step and 1-LSB compositing wobble starts a new
one — which is strictly worse than the screenshot it was supposed to replace.
Two knobs, both echoed in the output line so a reading is never ambiguous about
how it was produced:

- `-Tolerance` (default 6) keeps a pixel in the current run while every channel
  stays within N of the run's **anchor** colour (its first pixel — comparing
  against the *previous* pixel would let a gradient drift arbitrarily far
  without ever tripping the threshold). 6 swallows antialiasing and dither
  while staying far below the ~39-per-channel step between the panel fill and
  the window background, so real boundaries still resolve. It also coalesces
  adjacent runs that end up within tolerance of each other, which is what stops
  a line of text emitting `41..101 0c0c0c | 102..135 0c0c0c | …` instead of one
  background run.
- `-MinRun` (default 4) **absorbs** shorter runs into the preceding one rather
  than deleting them. Deletion punches holes in the coordinate map and makes
  the printed offsets lie about where a boundary is; absorption keeps the runs
  contiguous over `0..span-1`, so an offset can be trusted.

In practice a scan down a busy terminal window goes from 354 runs to 19, each
one a real visual band. For pixel-exact work pass `-Tolerance 0 -MinRun 1` and
accept the volume. `-MaxRuns` (default 40) is the backstop truncation guard.

A probe costs ~50 tokens. Reading a screenshot costs thousands and yields a
guess rather than a number, so probe first and screenshot only when you need to
see *shape*.

## Compositing arithmetic

This is the part that turns a hex value into a diagnosis.

egui premultiplies alpha. A fill declared as
`Color32::from_rgba_unmultiplied(r, g, b, a)` is stored as
`(r·a/255, g·a/255, b·a/255, a)`, and the compositor lays it over whatever is
behind it:

```
observed = premultiplied_channel + (1 − a/255) · background_channel
         = r·a/255 + (1 − a/255) · bg
```

Worked example — the one that identified the startup-transparency defect.
`PANEL_FILL` in `crates/app/src/ui.rs` is
`Color32::from_rgba_unmultiplied_const(18, 18, 22, 200)`. Premultiplied that is
`(14, 14, 17)`, and `1 − 200/255 = 0.216`, so:

| Background behind the panel | Predicted observation |
| --- | --- |
| opaque white (`255,255,255`) | `14 + 0.216·255 = 69` → **`#454548`** |
| magenta backdrop (`255,0,255`) | `(69, 14, 72)` → `#450e48` |
| black (`0,0,0`) | `(14, 14, 17)` → `#0e0e11` |

The probe read `#454548`. That is not "a dark panel that looks slightly wrong" —
it is arithmetic proof that the panel was compositing over **opaque white**,
i.e. that something was painting an opaque background behind it instead of
letting the desktop through. Two probes and one subtraction located the defect
rect to the pixel.

Run the inverse when you need the background instead of the fill:
`bg = (observed − r·a/255) / (1 − a/255)`.

### The backdrop

`backdrop-on` puts a solid magenta (`255,0,255`) window behind the overlay so
translucency is unambiguous: correct compositing tints the probe toward
magenta, broken compositing does not. Magenta is the default precisely because
no colour in the app's palette is anywhere near it, so a single channel
comparison decides the question. `-BackdropR/G/B` change it; `-BackdropX/Y/W/H`
place it.

The backdrop must actually be *behind* the window and *under* the sampled
pixels. A stale `window_position` in `settings.json` can put the overlay on a
second monitor, where it composites over the desktop rather than the backdrop
and every reading silently means nothing. Check the rect in the probe output
against the backdrop's rect before trusting a translucency conclusion.

## Gotchas encoded in the scripts

Each of these cost real time to find; they are commented at the site in
`scripts/uidbg/` and summarised here so a reader knows to look.

- **PowerShell variable names are case-insensitive.** A loop-local `$rows` is
  the *same variable* as an `[int]$Rows` parameter, and assigning an array to it
  throws `Cannot convert the "System.Object[]" value … to type "System.Int32"`
  from a line that never mentions `$Rows`. Locals in `Probe` and `Scan-Line` are
  named so they cannot alias a parameter. The same hazard applies to automatic
  variables like `$args` and `$Profile` (hence `-BuildProfile`).
- **`DwmGetWindowAttribute` is declared twice, under distinct managed names.**
  One native entry point, two shapes we need — `out RECT` for
  `EXTENDED_FRAME_BOUNDS` and `out int` for `CLOAKED`. Under a single managed
  name, PowerShell's overload resolution picks by the runtime type of the `[ref]`
  argument and gets it wrong often enough to produce garbage rects. Explicit
  `EntryPoint` attributes remove the overload set entirely.
- **`Add-Type` is cached for the host's lifetime.** The ctl server is a
  long-lived PowerShell host, so editing `lib.ps1` mid-session does *not* reload
  the P/Invoke class — the `PSTypeName` guard finds the stale type and skips
  recompiling. Bump the version suffix on `ShinraUidbgNativeV1` (and every
  reference to it) when you change the class body, or restart the server.
- **Cleanup must never kill the control server.** The first version matched
  candidates with `CommandLine -like '*backdrop*'`; the server's own command
  line names the harness scripts, so cleanup killed the channel mid-job and it
  looked like a hang. `Stop-Backdrop` now matches only the backdrop's own
  pidfile and its distinctive `MainWindowTitle`, and explicitly excludes `$PID`.
  Never reintroduce a command-line match.
- **A false return is not always an error.** `SetForegroundWindow` returns
  false with `GetLastError() == 0` whenever Windows declines a foreground
  change from a process that does not hold foreground-eligibility — which is
  routine, and irrelevant to capture, since a screenshot reads the composited
  desktop rather than the active window. Printing `ERR … win32 error 0` above a
  perfectly good screenshot trains the reader to skim past `ERR` lines, which
  is precisely the habit that hides a real error 5. `Invoke-UidbgWin32`
  therefore diagnoses only a genuinely non-zero last error, and downgrades
  non-fatal calls to `NOTE` — error 5 always stays `ERR` with the full
  integrity-level explanation.
- **The app can veto your resize.** Snap-blocking and auto-fit logic in `ui.rs`
  may revert an external `MoveWindow`, and `MIN_INNER_SIZE` (220×90) clamps
  shrink tests. `Set-AppRect` re-reads the rect afterwards and prints
  `MISMATCH` when what stuck differs from what was asked for; never assume a
  move landed.
- **There is a ceiling too, and Win32 hides it from you.** A window extent is
  saturated at `SHRT_MAX`, so `Set-AppRect -H 100000` never fails — it reaches
  the app as `32767`, which is past the renderer's 8192 limit and used to panic
  it inside `Surface::configure` (issue #257). `Set-AppRect` now refuses
  anything over 8192 on either axis with `ERR out-of-range`; the app refuses it
  independently (`platform::oversize_response` pins `SWP_NOSIZE` and keeps the
  size it had), so an oversize request is a no-op at both ends rather than a
  silently different window.
- **Deploying over a running exe fails.** Windows locks a running image;
  `Copy-AppExe` kills the app first and retries the copy, because `Stop-Process`
  returns before the kernel has finished tearing the process down.
- **Multi-monitor and negative coordinates.** Captures are clamped to
  `SystemInformation.VirtualScreen`, which is normal for a display placed left
  of or above the primary. If probes come back as solid desktop colour, check
  the rect first.
- **DPI.** Rects and captures are both in physical pixels for a DPI-aware
  process. If the elevated host is running DPI-unaware, coordinates are
  virtualised and captures can land offset on a scaled display; compare a
  `status` rect against a screenshot once at the start of a session on a
  non-100% display.
- **A quoted parameter name is not a parameter name.** `ctl.sh app` used to
  single-quote every forwarded argument, so `-Action` reached PowerShell as
  the literal string `'-Action'` — a positional value, not the parameter name
  — and `scripts/ctl.sh app -Action status` failed with a confusing "the
  argument `-Action` does not belong to the set" from `ValidateSet`, nowhere
  near the actual cause. Arguments matching `-[A-Za-z]*` are now passed bare;
  a value like `-12` still gets single-quoted, since it isn't a parameter
  name.
