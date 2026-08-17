# ShinraMeter-BPSR

Minimal damage tracker for Blue Protocol: Star Resonance. A borderless, always-on-top overlay displaying per-player damage totals, DPS, contribution %, crit rate, and hit count. Styled after ShinraMeter. Designed for Windows with packet sniffing via WinDivert; developed cross-platform on WSL.

## Architecture

The pipeline flows: **protocol crate** (frame splitting, decompression, protobuf decoding) → **capture crate** (WinDivert packet sniffing on Windows, TCP reassembly, server detection) → **meter crate** (encounter state machine, per-player stats, reset logic) → **app crate** (egui overlay, channels, command dispatch).

```
┌─────────────────────────────────────────────────────────┐
│ ShinraMeter-BPSR (app)                                  │
│   egui overlay ◄─── Snapshot (dps, damage, player rows) │
│      │                                                   │
│      ├─ Pipeline (meter state, per-100ms snapshots)     │
│      │                                                   │
│      └─ bpsr-capture (WinDivert packet loop)            │
│           ├─ TCP reassembly (out-of-order, retransmits) │
│           ├─ Server detection (game signature scan)     │
│           └─ bpsr-protocol Decoder (frames → events)    │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

## Build

### Prerequisites

On WSL (cross-compiling to Windows):

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64
```

The `crates/capture` crate links `windivert` with the `vendored` feature, which invokes `x86_64-w64-mingw32-gcc`, `-dlltool`, and `-strip` during the build script — without mingw, even `cargo check --target x86_64-pc-windows-gnu` fails.

### Compile

Type-check on the host (fast):

```sh
cargo check --workspace
```

Cross-compile type-check (requires mingw above):

```sh
cargo check --workspace --target x86_64-pc-windows-gnu
```

Full build for Windows:

```sh
cargo build --release --target x86_64-pc-windows-gnu
```

The binary lands at `target/x86_64-pc-windows-gnu/release/ShinraMeter-BPSR.exe`.

### Windows Runtime

Nothing to install: `ShinraMeter-BPSR.exe` is self-contained. Download it, double-click it, and accept the UAC prompt.

The WinDivert 2.2.2 runtime (`WinDivert.dll` and the signed `WinDivert64.sys` kernel driver) is embedded in the executable and unpacked to `%LOCALAPPDATA%\ShinraMeter-BPSR\windivert\2.2.2\` on first run — the driver has to exist as a real file because Windows loads kernel drivers by path, but the user never handles it. Subsequent runs reuse the unpacked copy and write nothing.

The exe carries a manifest requesting `requireAdministrator`, so Windows prompts for elevation on launch; WinDivert installs its driver through the Service Control Manager, which is administrator-only.

To uninstall, delete the exe and `%LOCALAPPDATA%\ShinraMeter-BPSR\`.

#### Updating the bundled WinDivert

The binaries live in `crates/capture/vendor/windivert/`, taken verbatim from the official [reqrypt.org](https://reqrypt.org/wdivert.html) x64 release — the driver cannot be rebuilt locally, since Windows only loads a kernel driver carrying a signature it trusts. To move to a new release, replace the three files there and bump `WINDIVERT_VERSION` in `crates/capture/src/driver.rs`, which is also the name of the unpack directory and so keeps the new copy clear of the old one.

## Testing

Run all unit tests on the host:

```sh
cargo test --workspace
```

The `bpsr-protocol`, `bpsr-meter`, and `bpsr-capture` (tcp/detect) crates are fully testable on Linux; `ShinraMeter-BPSR` tests only pure formatting helpers.

## Troubleshooting

The status banner names the specific failure — each of these has a different fix, so read which one it says.

### "Windows blocked the WinDivert driver"
Antivirus, a VPN filter, or Core Isolation / Memory Integrity vetoed the driver. NordVPN and Brave are known conflicts; close them and retry. Otherwise check Windows Security → Device security → Core isolation.

### "A different version of the WinDivert driver is already loaded"
Another packet-capture tool holds an older WinDivert. Close it, or reboot, and retry.

### "The Base Filtering Engine service is disabled"
Start it: `services.msc` → Base Filtering Engine → Start.

### "Windows could not find the WinDivert driver file"
The driver is unpacked to `%LOCALAPPDATA%\ShinraMeter-BPSR\windivert\<version>\` at startup; this means it is not there when WinDivert looks. Check that antivirus is not quarantining it, then delete that folder to force a clean unpack.

### "Run as Administrator"
The manifest normally makes Windows prompt for this automatically, so seeing this means elevation was declined or stripped. Right-click the exe → Run as administrator.

### No damage data appears
1. Verify you are logged into the game and in combat.
2. Check the overlay status line for error messages.
3. Restart the meter and try again.
4. Confirm the game server is being detected — watch the overlay for `ServerChanged` events in debug logs if enabled.

### The overlay is black, blank, or never paints
The overlay normally opens as a genuinely transparent window: on a machine with a hardware DX12 adapter it is created with `WS_EX_NOREDIRECTIONBITMAP` and presents through DirectComposition (issue #89). Both are creation-time-only and cannot be undone on a live window, so if a driver, an RDP session or a virtualized GPU cannot present that way, set `SHINRA_NO_COMPOSITION=1` and restart: that forces the legacy path, whose only cost is the old flat-gray-until-resized startup. The startup log records which path was chosen and why.

### Logs
Logging (issue #69) is on by default at `info`, since the app carries no console to print to (`windows_subsystem = "windows"`) and stderr alone would go nowhere. Logs are written to `%APPDATA%\ShinraMeter-BPSR\logs\ShinraMeter-BPSR.log`, overridable with `SHINRA_LOG_FILE=<path>` (falling back to `ShinraMeter-BPSR.log` in the working directory if `APPDATA` is unset, e.g. a non-Windows dev host). Raise the verbosity with the standard `RUST_LOG` env var, e.g. `RUST_LOG=debug`. The file is rotated once at startup — renamed to `<path>.1`, replacing any previous one — once it reaches 5 MiB, so a long-lived overlay can't grow it unbounded.

Logs may contain player names and other identifying traffic — never attach one to an issue or PR; mint a minimal synthetic repro instead.

## License

This project is licensed under the GNU General Public License v3.0
(GPL-3.0-only). That license covers the code in this repository. It does not
cover, and does not purport to cover, the game-client-derived assets
described in `THIRD_PARTY_NOTICES.md` (the class icons and Imagine icons
under `crates/app/assets/`, and the Imagine id/name table) — those remain the
property of their respective owners and are redistributed here only on the
inferred basis explained there, not under a grant from this project.

Those assets will be removed promptly on request from the rights holder. To
request removal, open a GitHub issue on this repository.

Ported packet-format knowledge from these open-source trackers (all GPL-compatible):
- https://github.com/Blue-Protocol-Source/BPSR-ZDPS
- https://github.com/winjwinj/bpsr-logs
- https://github.com/resonance-logs/resonance-logs

UI design inspiration from:
- https://github.com/neowutran/ShinraMeter/tree/mvvm_refactor_wip

Note the branch. `master` is the released ShinraMeter and stopped at its final
TERA-era commit in November 2022; its toolbar art is solid white glyphs and its
mark is the kanji 神羅. The look this project styles toward — the rounded
translucent card, the oval stat pills, the thin outline glyphs, the collapse
chevron, the horned-emblem header gutter — is the unreleased `mvvm_refactor_wip`
UI rewrite (last touched April 2024, no tags or releases). Its icon vocabulary
lives in `DamageMeter.UI/Resources/SVG.xaml` as SVG path data, a file that does
not exist on `master`. Look there first when matching a reference render.
