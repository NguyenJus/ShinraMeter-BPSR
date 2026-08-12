# shinra-bpsr

Minimal damage tracker for Blue Protocol: Star Resonance. A borderless, always-on-top overlay displaying per-player damage totals, DPS, contribution %, crit rate, and hit count. Styled after ShinraMeter. Designed for Windows with packet sniffing via WinDivert; developed cross-platform on WSL.

## Architecture

The pipeline flows: **protocol crate** (frame splitting, decompression, protobuf decoding) → **capture crate** (WinDivert packet sniffing on Windows, TCP reassembly, server detection) → **meter crate** (encounter state machine, per-player stats, reset logic) → **app crate** (egui overlay, channels, command dispatch).

```
┌─────────────────────────────────────────────────────────┐
│ shinra-bpsr (app)                                       │
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

The binary lands at `target/x86_64-pc-windows-gnu/release/shinra-bpsr.exe`.

### Windows Runtime

1. Download `WinDivert.dll` and `WinDivert64.sys` (version 2.x) from [reqrypt.org](https://reqrypt.org/wdivert.html).
2. Place both files next to `shinra-bpsr.exe` in the same directory.
3. Run the exe as Administrator. (The WinDivert kernel driver requires elevated privileges for packet interception.)

## Testing

Run all unit tests on the host:

```sh
cargo test --workspace
```

The `bpsr-protocol`, `bpsr-meter`, and `bpsr-capture` (tcp/detect) crates are fully testable on Linux; `shinra-bpsr` tests only pure formatting helpers.

## Troubleshooting

### Error: "WinDivert driver not found"
Ensure `WinDivert.dll` and `WinDivert64.sys` are in the same directory as the exe. If NordVPN or Brave browser are running, they may block the WinDivert driver (known conflict — restart them if capture fails).

### Error: "Run as Administrator"
The exe must be elevated to capture packets. Retry with administrator privileges.

### No damage data appears
1. Verify you are logged into the game and in combat.
2. Check the overlay status line for error messages.
3. Restart the meter and try again.
4. Confirm the game server is being detected — watch the overlay for `ServerChanged` events in debug logs if enabled.

## License

This project is licensed under the GNU General Public License v3.0 (GPL-3.0-only).

Ported packet-format knowledge from these open-source trackers (all GPL-compatible):
- https://github.com/Blue-Protocol-Source/BPSR-ZDPS
- https://github.com/winjwinj/bpsr-logs
- https://github.com/resonance-logs/resonance-logs

UI design inspiration from:
- https://github.com/neowutran/ShinraMeter
